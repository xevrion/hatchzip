//! In-place extraction, independent of the archive format.
//!
//! Units are processed from the highest offset down. After each batch the
//! archive is truncated to where the batch began, which every filesystem
//! supports, FAT32 and exFAT included, so this path needs no hole punching.
//!
//! A unit too large for the free space would need room for all of its
//! output before any of its archive bytes could go. Where the filesystem can
//! punch holes and the format can resume decoding mid-file, such a unit is
//! streamed instead: every [`stream_step`] of output it is flushed, a resume
//! point is journaled, and only then are the archive bytes before that point
//! punched out. The archive's length stays the same until the unit commits
//! through the ordinary path.
//!
//! A compressed tar is one stream that decodes only from the front, so it is
//! one unit holding every entry, always streamed, front to back, with resume
//! points between the files as well as inside them (see
//! [`Run::stream_solid`]). Without hole punching it still streams, so a
//! crash resumes from the last point, but nothing is freed until the end.
//!
//! A format whose decoder finds a unit only by reading everything before it
//! (RAR, through UnRAR) says so with [`Format::serial`]; its batches are
//! decoded on one thread, lowest unit first, by one worker.
//!
//! An extraction that keeps the archive (`Journal::keep`) runs the same
//! batches, checks and commits, so it resumes the same way, but never
//! truncates, punches or deletes: its storage is protected, and the run
//! frees nothing, exactly like an output on another disk.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use rayon::prelude::*;

use crate::archive::resumable::{self, Decoder};
use crate::archive::storage::{Reads, Storage, Tail};
use crate::archive::targz::{Event, Profile};
use crate::archive::{
    self, Codec, Format, Item, Kind, PasswordCheck, Sink, SolidWalker, Stream, Unit,
};
use crate::journal::{Journal, Link, Paths, Resume, TarAt};
use crate::{fsutil, platform, replace};

/// Larger batches extract in parallel better, smaller ones redo less work
/// after a crash and keep the archive shrinking in fine steps.
const BATCH_MAX_UNITS: usize = 512;
const BATCH_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const BUF_SIZE: usize = 1024 * 1024;
const MAX_SYMLINK: u64 = 64 * 1024;

/// Output written between two resume points of a streamed unit. Each point
/// costs a flush and a journal write; the space check counts one step as the
/// most a streamed unit holds that its archive bytes don't offset.
const STREAM_STEP: u64 = 256 * 1024 * 1024;

fn stream_step() -> u64 {
    test_knob("HATCHZIP_STREAM_STEP").map_or(STREAM_STEP, u64::from)
}

/// Units smaller than this are always extracted whole: streaming one would
/// save less than a couple of steps.
fn stream_threshold() -> u64 {
    2 * stream_step()
}

/// The test suite streams every unit it can, whether it fits or not, so the
/// streaming path runs on small archives.
fn always_stream() -> bool {
    test_knob("HATCHZIP_STREAM_STEP").is_some()
}

/// More than any disk holds. An archive whose files add up to more is
/// refused, so the space check's sums of sizes stay far from overflowing.
const MAX_TOTAL_SIZE: u64 = 1 << 62;

/// The most an archive may hold after its last unit: the index and whatever
/// else is saved before the extraction starts.
fn max_tail() -> u64 {
    test_knob("HATCHZIP_MAX_TAIL").map_or(1_000_000_000, u64::from)
}

fn on_disk(u: &Unit) -> u64 {
    u.items.iter().fold(0u64, |sum, i| {
        let size = i.size.checked_next_multiple_of(4096).unwrap_or(u64::MAX);
        sum.saturating_add(size)
    })
}

pub struct Job {
    pub storage: Arc<Storage>,
    pub format_name: &'static str,
    format: Box<dyn Format>,
    /// Units still to do, highest offset first.
    pub units: Vec<Unit>,
    pub tail_start: u64,
    /// Where the archive currently ends: its original length, or wherever
    /// the last commit truncated it.
    pub top: u64,
    /// Whether the archive's filesystem can free bytes inside a file, and
    /// how much of a punched range it may keep allocated at each end. Probed
    /// once, on first use.
    punch: OnceLock<Option<u64>>,
    /// The streamed unit a previous run left half done, if any.
    resume: Option<Resume>,
    /// Whether a run may have extracted anything yet. Until it has, what
    /// sits at the paths of entries belongs to the person: a fresh archive,
    /// or a run still moving the files it replaces aside.
    started: bool,
}

impl Job {
    /// `password` is only needed for encrypted archives. A missing one shows
    /// up as [`archive::PasswordNeeded`], from here or from
    /// [`Job::check_password`]. What opening reads is counted in `reads`.
    pub fn open(
        path: &Path,
        writable: bool,
        password: Option<&str>,
        reads: Arc<Reads>,
    ) -> Result<Self> {
        let job = Self::open_as_is(path, writable, password, reads)?;
        job.check_layout()?;
        Ok(job)
    }

    /// [`Job::open`] without [`Job::check_layout`]. Restore checks what it
    /// rebuilt with this: an archive an older release started on may hold
    /// entries that collide, and giving it back is still the right thing to
    /// do. A later extraction of it is refused as usual.
    pub(crate) fn open_as_is(
        path: &Path,
        writable: bool,
        password: Option<&str>,
        reads: Arc<Reads>,
    ) -> Result<Self> {
        let mut storage = Storage::open(path, writable)?;
        storage.count_reads(reads);
        let storage = Arc::new(storage);
        let opened = archive::open(&storage, None, password)?;
        let top = storage.original_len();
        // The tail is read into memory when the extraction begins and stays
        // there until it ends. An index is nowhere near this big. A run
        // that already began has it loaded, so only a new one is refused.
        if top.saturating_sub(opened.tail_start) > max_tail() {
            bail!(
                "this archive holds more than {} after its last entry, which HatchZip would \
                 have to keep in memory for the whole extraction. The archive was not modified",
                human(max_tail())
            );
        }
        Self::build(storage, opened, top)
    }

    /// Reopens an archive a previous run already started on. A read-only
    /// open leaves a pending truncation for the next writable one. The
    /// password is never stored, so an encrypted archive needs it again.
    pub fn resume(
        journal: &Journal,
        tail: Arc<Tail>,
        writable: bool,
        password: Option<&str>,
        reads: Arc<Reads>,
    ) -> Result<Self> {
        let mut storage = Storage::from_parts(&journal.parts, writable)?;
        storage.count_reads(reads);
        let storage = Arc::new(storage);
        let current = storage.current_len()?;
        if journal.keep {
            // Never truncated, so `committed_len` only marks progress and
            // the whole archive must still be there.
            storage.protect();
            if current != storage.original_len() {
                bail!(
                    "{} has changed since the extraction started. Put the original archive back \
                     to resume, or run restore to discard the progress",
                    storage.primary().display()
                );
            }
        }
        if current < journal.committed_len {
            bail!(
                "{} is shorter than HatchZip left it and will not be modified",
                storage.primary().display()
            );
        }
        if current > journal.committed_len && writable && !journal.keep {
            // The last run committed a batch but stopped before truncating.
            storage.set_len(journal.committed_len)?;
        }
        let point = journal.resume.as_ref();
        let opened = archive::open(&storage, Some((&journal.format, tail, point)), password)?;
        let mut job = Self::build(storage, opened, journal.committed_len)?;
        job.started = journal.replace.as_ref().is_none_or(|r| r.moved);
        if let Some(r) = &journal.resume {
            // Only the highest uncommitted unit is ever streamed, so a point
            // for any other one means the journal and archive disagree.
            if job.units.first().map(|u| u.start) != Some(r.unit) {
                bail!(
                    "the journal of {} records a partially extracted file that is not the next one \
                     in the archive. The archive will not be modified",
                    job.storage.primary().display()
                );
            }
            job.resume = Some(r.clone());
        }
        Ok(job)
    }

    fn build(storage: Arc<Storage>, opened: archive::Opened, top: u64) -> Result<Self> {
        let mut units = opened.units;
        // Units from `top` up were extracted by an earlier run. Ones that
        // start at or past the end of the file never existed, and dropping
        // them quietly would lose their files with the archive.
        let len = storage.original_len();
        if opened.tail_start > len || units.iter().any(|u| u.start >= len) {
            return Err(archive::Damaged(anyhow!(
                "this archive records data past the end of the file, so it is damaged or \
                 incomplete. Nothing was modified"
            ))
            .into());
        }
        units.retain(|u| u.start < top);
        units.sort_by_key(|u| std::cmp::Reverse(u.start));
        // A unit owns its bytes up to the next one's start. Two that start
        // at the same offset can't both own them: committing one would
        // truncate the other before it is extracted.
        if units.windows(2).any(|w| w[0].start == w[1].start) {
            bail!(
                "two parts of this archive start at the same offset, so extracting it in place \
                 could destroy data before it is extracted; extraction refused"
            );
        }
        let total = units
            .iter()
            .flat_map(|u| &u.items)
            .try_fold(0u64, |sum, i| sum.checked_add(i.size));
        if total.is_none_or(|t| t > MAX_TOTAL_SIZE) {
            return Err(archive::Damaged(anyhow!(
                "the file sizes recorded in this archive add up to more than any disk can hold, \
                 so the archive is damaged. Nothing was modified"
            ))
            .into());
        }

        let mut seen = HashSet::new();
        for item in units.iter().flat_map(|u| &u.items) {
            for part in item.path.iter() {
                if let Some(why) = platform::name_problem(&part.to_string_lossy()) {
                    bail!(
                        "'{}' cannot be created at this location: {why}. The archive was not modified",
                        item.path.display()
                    );
                }
            }
            // Case-insensitive filesystems would let two items race for one
            // file inside a parallel batch.
            if item.kind != Kind::Dir && !seen.insert(item.path.to_string_lossy().to_lowercase()) {
                bail!(
                    "'{}' appears more than once in the archive (ignoring case); extraction refused",
                    item.path.display()
                );
            }
        }
        Ok(Self {
            storage,
            format_name: opened.name,
            format: opened.format,
            units,
            tail_start: opened.tail_start,
            top,
            punch: OnceLock::new(),
            resume: None,
            started: false,
        })
    }

    /// Refuses entries that can't all be created where the archive puts
    /// them: a file or link at a path where another entry needs a folder,
    /// which includes any entry inside a link, and a hard link to anything
    /// but a file in the archive. Links are only made at the very end, once
    /// the archive is gone, so without this such an archive fails after it
    /// has been consumed. Paths are compared ignoring case, as for
    /// duplicates. So are names ending in [`PART_SUFFIX`].
    ///
    /// Only a fresh archive is checked: a resumed run no longer lists the
    /// entries it has extracted, and one an older release started on has to
    /// stay resumable and restorable.
    fn check_layout(&self) -> Result<()> {
        let key = |p: &Path| p.to_string_lossy().to_lowercase();
        let items = || self.units.iter().flat_map(|u| &u.items);
        // Every file is first written under its name plus the suffix. An
        // entry named so would share that name with another entry's file,
        // and which of the two is lost would depend on timing.
        if let Some(item) = items().find(|i| {
            i.path
                .iter()
                .any(|c| c.to_string_lossy().to_lowercase().ends_with(PART_SUFFIX))
        }) {
            bail!(
                "'{}' uses a name ending in {PART_SUFFIX}, which HatchZip gives a file while it \
                 writes it, so extraction is refused. Extract this archive with another program. \
                 Nothing was modified",
                item.path.display()
            );
        }
        // Every folder the extraction creates, with the entry that needs it:
        // the folder's own entry if it has one, else the first entry inside.
        let mut folders: HashMap<String, (&Path, bool)> = HashMap::new();
        for item in items() {
            if item.kind == Kind::Dir {
                folders.insert(key(&item.path), (&item.path, true));
            }
            for parent in item.path.ancestors().skip(1) {
                if parent.as_os_str().is_empty() {
                    break;
                }
                folders.entry(key(parent)).or_insert((&item.path, false));
            }
        }
        let files: HashSet<&Path> = items()
            .filter(|i| i.kind == Kind::File)
            .map(|i| i.path.as_path())
            .collect();
        for item in items() {
            let what = match &item.kind {
                Kind::Dir => continue,
                Kind::File => "a file",
                Kind::Symlink => "a symbolic link",
                Kind::HardLink(_) => "a hard link",
            };
            match folders.get(&key(&item.path)) {
                Some((dir, true)) if *dir == item.path => bail!(
                    "'{}' is both {what} and a folder in the archive; extraction refused",
                    item.path.display()
                ),
                Some((dir, true)) => bail!(
                    "'{}' is {what}, and '{}' is a folder in the archive; extraction refused",
                    item.path.display(),
                    dir.display()
                ),
                Some((inside, false)) => bail!(
                    "'{}' is {what}, but '{}' requires a folder at that location; extraction \
                     refused",
                    item.path.display(),
                    inside.display()
                ),
                None => {}
            }
            if let Kind::HardLink(target) = &item.kind
                && !files.contains(target.as_path())
            {
                bail!(
                    "hard link '{}' points to '{}', which is not a file in the archive; \
                     extraction refused",
                    item.path.display(),
                    target.display()
                );
            }
        }
        Ok(())
    }

    /// `Some(granule)` if holes punched in the archive give its space back,
    /// where `granule` is how much may stay allocated at each end of a
    /// punched range. `None` on filesystems that can only truncate, such as
    /// FAT32 and exFAT, which keep today's whole-unit behavior.
    pub fn punch_support(&self) -> Option<u64> {
        *self.punch.get_or_init(|| {
            if test_knob("HATCHZIP_NO_PUNCH").is_some() {
                return None;
            }
            let dir = match self.storage.primary().parent() {
                Some(d) if !d.as_os_str().is_empty() => d,
                _ => Path::new("."),
            };
            fsutil::can_punch(dir).then(|| platform::punch_granule(dir))
        })
    }

    pub fn encrypted(&self) -> bool {
        self.units.iter().any(|u| u.encrypted)
    }

    /// The archive is one compressed stream, extracted front to back.
    pub fn is_stream(&self) -> bool {
        self.format.solid().is_some()
    }

    /// The archive is one compressed stream that stays whole until the run
    /// ends, because the filesystem can't free space inside it: the disk
    /// has to hold the archive and every extracted file at once.
    pub fn kept_whole(&self, reclaims: bool) -> bool {
        self.is_stream() && reclaims && self.punch_support().is_none()
    }

    /// Proves the password before anything is deleted: decrypts the smallest
    /// encrypted unit, which checks its CRC, up to a limit so a huge solid
    /// block doesn't stall. ZipCrypto accepts about one wrong password in 256
    /// at first sight; reading the data catches those.
    pub fn check_password(&self) -> Result<()> {
        const ENOUGH: u64 = 64 * 1024 * 1024;
        let Some(unit) = self.smallest_encrypted() else {
            return Ok(());
        };
        let left = match self.format.password_check() {
            PasswordCheck::Headers => return self.format.prove_password(),
            PasswordCheck::Sample => ENOUGH,
            // A wrong password only shows in the checksum at the end, so a
            // sample would let it through and the run would fail later, after
            // the archive is partly gone.
            PasswordCheck::WholeUnit => u64::MAX,
        };
        let mut worker = self.format.worker(1)?;
        let mut sink = ProbeSink {
            left,
            buf: vec![0; BUF_SIZE],
        };
        match worker.extract(unit, &mut sink) {
            Ok(()) => Ok(()),
            Err(e) if e.chain().any(|c| c.is::<ProbeDone>()) => Ok(()),
            Err(e) if e.chain().any(|c| c.is::<archive::PasswordNeeded>()) => Err(e),
            Err(_) => Err(archive::WrongPassword.into()),
        }
    }

    /// The smallest encrypted unit with data: an empty file decodes with
    /// any password in formats whose only check is the file's checksum.
    fn smallest_encrypted(&self) -> Option<&Unit> {
        self.units
            .iter()
            .filter(|u| u.encrypted)
            .min_by_key(|u| (u.size() == 0, u.size()))
    }

    /// When [`check_password`](Self::check_password) has to decode a whole
    /// file to prove the password, as for RAR 4: that file and its size, so
    /// a front end can say the check may take a while.
    pub fn slow_password_check(&self) -> Option<(&Path, u64)> {
        if !self.encrypted() || self.format.password_check() != PasswordCheck::WholeUnit {
            return None;
        }
        let unit = self.smallest_encrypted()?;
        Some((unit.items.first()?.path.as_path(), unit.size()))
    }

    /// Why the archive can't be extracted in place, if it can't. It can
    /// still be extracted while it is kept.
    pub fn in_place_refusal(&self) -> Option<String> {
        self.format.refuses_in_place()
    }

    pub fn item_count(&self) -> usize {
        self.units.iter().map(|u| u.items.len()).sum()
    }

    pub fn remaining_size(&self) -> u64 {
        let done = self.resume.as_ref().map_or(0, |r| r.written);
        self.units
            .iter()
            .fold(0u64, |sum, u| sum.saturating_add(u.size()))
            .saturating_sub(done)
    }

    /// The resume point recorded for `units[i]`, if it is half streamed.
    fn resumed(&self, i: usize) -> Option<&Resume> {
        self.resume
            .as_ref()
            .filter(|r| r.unit == self.units[i].start)
    }

    /// Where the data of `units[i]` lies, if it is one file whose decoding
    /// can be resumed from the middle.
    fn stream_of(&self, i: usize) -> Option<Stream> {
        let unit = &self.units[i];
        match unit.items.as_slice() {
            [item] if item.kind == Kind::File && !unit.encrypted => {
                self.format.stream(unit).ok().flatten()
            }
            _ => None,
        }
    }

    /// Whether `units[i]` may free its own archive bytes while it is
    /// extracted. A half-streamed unit always continues streaming: the bytes
    /// before its resume point may be gone.
    fn streams(&self, i: usize, reclaims: bool) -> Option<Stream> {
        if self.resumed(i).is_some() {
            return self.stream_of(i);
        }
        if !reclaims || self.units[i].size() < stream_threshold() {
            return None;
        }
        self.punch_support()?;
        self.stream_of(i)
    }

    /// Bytes freed once `units[i]` is committed: everything from its start up
    /// to whatever came after it.
    fn span(&self, i: usize) -> u64 {
        let end = if i == 0 {
            self.top
        } else {
            self.units[i - 1].start
        };
        end - self.units[i].start
    }

    /// Replays the extraction one unit at a time and returns `(peak, end)`:
    /// the most free space it ever consumes beyond what is free now, and how
    /// much it consumes once finished. `fresh` means the tail hasn't been
    /// saved yet; `reclaims` that freed archive space lands on the output's
    /// disk.
    pub fn space_needed(&self, fresh: bool, reclaims: bool) -> (u64, i128) {
        let tail = self.storage.original_len().saturating_sub(self.tail_start) as i128;
        let mut extra: i128 = if fresh { tail } else { 0 };
        let mut peak = extra;
        for (i, u) in self.units.iter().enumerate() {
            let span = self.span(i) as i128;
            if let Some(solid) = self.format.solid() {
                let (rise, net) = self.solid_cost(i, solid.profile(), span, reclaims);
                peak = peak.max(extra + rise);
                extra += net;
                continue;
            }
            if let Some(s) = self.streams(i, reclaims) {
                let (rise, net) = self.stream_cost(i, &s, span, reclaims);
                peak = peak.max(extra + rise);
                extra += net;
                continue;
            }
            let on_disk = on_disk(u) as i128;
            peak = peak.max(extra + on_disk);
            extra += on_disk;
            if reclaims {
                extra -= span;
            }
        }
        if reclaims {
            // Whatever precedes the first unit is deleted at the very end,
            // along with the saved tail.
            let stub = self.units.last().map_or(self.top, |u| u.start);
            extra -= stub as i128 + tail;
        }
        (peak.max(0) as u64, extra)
    }

    /// What streaming `units[i]` costs in free space: `(rise, net)`, the most
    /// it consumes at any moment and what it consumes once committed.
    ///
    /// At any moment the unit holds the output written since the last resume
    /// point (at most a step, plus one read buffer or deflate block), the
    /// archive bytes at either end of the punched range the filesystem keeps
    /// (a granule each), and however far the output so far has outgrown the
    /// input it came from. For stored data that last part is zero. For
    /// deflate it only grows as decoding goes on, since encoders fall back to
    /// stored blocks rather than expand data, so the end state bounds it; a
    /// 1/1024 margin covers block headers.
    ///
    /// Without punching (the filesystem stopped supporting it after a run
    /// began), the unit holds all of its remaining output until it commits.
    fn stream_cost(&self, i: usize, s: &Stream, span: i128, reclaims: bool) -> (i128, i128) {
        let at = self.resumed(i);
        // What a crashed run already wrote is on disk, so the free space
        // measured now has paid for it.
        let done = at.map_or(0, |r| r.written / 4096 * 4096);
        let out_left = on_disk(&self.units[i]).saturating_sub(done) as i128;
        let read_to = at.map_or(s.data_start, |r| r.bit / 8);
        let data_left = (s.data_start + s.packed).saturating_sub(read_to) as i128;
        // At most this much was punched already. It is free now, so the
        // final truncation gives back that much less; assuming the most
        // keeps the estimate on the safe side.
        let freed = read_to.saturating_sub(s.data_start) as i128;
        let net = if reclaims {
            out_left - (span - freed)
        } else {
            out_left
        };
        let rise = match self.punch_support() {
            Some(granule) if reclaims => {
                let slack = match s.codec {
                    Codec::Stored => 0,
                    Codec::Deflate => out_left / 1024,
                };
                let step = (stream_step() + BUF_SIZE as u64 + 2 * granule) as i128;
                ((out_left - data_left).max(0) + slack + step).min(out_left)
            }
            _ => out_left,
        };
        (rise, net)
    }

    /// What streaming a compressed tar, `units[i]`, costs in free space:
    /// `(rise, net)` as for [`stream_cost`](Self::stream_cost).
    ///
    /// Before each resume point is taken, the files hold what they held at
    /// the point before, `lead` at most ahead of the compressed bytes freed
    /// up to it, plus what was written since: under a step, plus the output
    /// to the next point after the step is full, at most `gap`. The scan
    /// measured both at the very points the run will meet. A read buffer
    /// and a granule at each end of the punched range come on top. Links are
    /// made at the end, once the archive is gone.
    fn solid_cost(&self, i: usize, p: &Profile, span: i128, reclaims: bool) -> (i128, i128) {
        let out_left = (p.files + p.links) as i128;
        // Assumed punched already, as for a single streamed file.
        let freed = self.resumed(i).map_or(0, |r| r.bit / 8) as i128;
        let net = if reclaims {
            out_left - (span - freed)
        } else {
            out_left
        };
        let rise = match self.punch_support() {
            Some(granule) if reclaims => {
                let ahead = stream_step() + p.gap + BUF_SIZE as u64 + 2 * granule;
                (p.lead.max(0) + ahead as i128).min(p.files as i128)
            }
            // Nothing is freed until the run commits, so every file is on
            // disk next to the whole archive.
            _ => out_left,
        };
        (rise.max(net), net)
    }

    /// The entry a resume point stopped inside of, if any: the first item of
    /// the first unit, whose part file holds `written` bytes.
    fn open_entry(&self) -> Option<&Item> {
        let r = self.resume.as_ref()?;
        let inside = match &r.stream {
            Some(s) => matches!(s.at, TarAt::File { .. }),
            None => true,
        };
        if !inside {
            return None;
        }
        self.units.first().and_then(|u| u.items.first())
    }

    /// Output a crashed run wrote for units it never committed. Nothing was
    /// at those paths when it started, or it was moved aside before the
    /// first entry was extracted, so anything there now is ours. A
    /// half-streamed file is not in the list: its archive bytes may be gone,
    /// so it is the only copy of its first part. For a compressed tar that
    /// is the entry its point is in; the files after the point are still in
    /// the archive.
    pub fn uncommitted(&self, out: &Path) -> Vec<(PathBuf, u64)> {
        if !self.started {
            return Vec::new();
        }
        let open = self.open_entry().is_some();
        let mut found = Vec::new();
        for (i, u) in self.units.iter().enumerate() {
            for (k, item) in u.items.iter().enumerate() {
                if item.kind != Kind::File || (open && i == 0 && k == 0) {
                    continue;
                }
                let dest = out.join(&item.path);
                for p in [part_path(&dest), dest] {
                    if let Ok(m) = fs::symlink_metadata(&p)
                        && m.is_file()
                    {
                        found.push((p, m.len().next_multiple_of(4096)));
                    }
                }
            }
        }
        found
    }

    /// Output a crashed stream wrote past its last resume point. It is cut
    /// off before the stream continues, so it counts as free.
    pub fn past_resume_point(&self, out: &Path) -> u64 {
        let (Some(r), Some(item)) = (self.resume.as_ref(), self.open_entry()) else {
            return 0;
        };
        let dest = out.join(&item.path);
        [part_path(&dest), dest]
            .iter()
            .find_map(|p| fs::metadata(p).ok())
            .map_or(0, |m| {
                (m.len().next_multiple_of(4096)).saturating_sub(r.written.next_multiple_of(4096))
            })
    }

    /// Reads every unit and lets the decoders check their checksums,
    /// writing nothing.
    pub fn verify(&self, on_bytes: &(dyn Fn(u64) + Sync)) -> Result<()> {
        let failures = if self.format.serial() {
            self.verify_serial(on_bytes)?
        } else {
            self.verify_parallel(on_bytes)
        };
        if failures.is_empty() {
            return Ok(());
        }
        let mut msg = format!("{} damaged:", failures.len());
        for f in failures.iter().take(10) {
            msg.push_str("\n  ");
            msg.push_str(f);
        }
        Err(anyhow!(msg))
    }

    /// One worker walks every unit from the lowest up.
    fn verify_serial(&self, on_bytes: &(dyn Fn(u64) + Sync)) -> Result<Vec<String>> {
        let mut worker = self.format.worker(1)?;
        let mut sink = VerifySink {
            on_bytes,
            buf: vec![0; BUF_SIZE],
        };
        Ok(self
            .units
            .iter()
            .rev()
            .filter_map(|u| worker.extract(u, &mut sink).err())
            .map(|e| format!("{e:#}"))
            .collect())
    }

    fn verify_parallel(&self, on_bytes: &(dyn Fn(u64) + Sync)) -> Vec<String> {
        let threads = threads_per_unit(self.units.len());
        self.units
            .par_iter()
            .map_init(
                || {
                    (
                        self.format.worker(threads),
                        VerifySink {
                            on_bytes,
                            buf: vec![0; BUF_SIZE],
                        },
                    )
                },
                |(worker, sink), unit| {
                    let mut run = || -> Result<()> {
                        let w = worker.as_mut().map_err(|e| anyhow!("{e:#}"))?;
                        w.extract(unit, sink)
                    };
                    run().err().map(|e| format!("{e:#}"))
                },
            )
            .flatten()
            .collect()
    }
}

fn threads_per_unit(units: usize) -> u32 {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    (cores / units.max(1)).max(1) as u32
}

/// Added to a file's name while it is written.
const PART_SUFFIX: &str = ".hatchzip-part";

pub(crate) fn part_path(dest: &Path) -> PathBuf {
    let mut part = dest.as_os_str().to_owned();
    part.push(PART_SUFFIX);
    PathBuf::from(part)
}

/// Creates the part file a new file is first written to. Whatever is
/// already at that name was not made by this run: a file there is someone
/// else's to keep, and a link there would send the data wherever it points.
/// Fresh runs refuse both before the archive is touched.
fn create_part(part: &Path) -> Result<File> {
    platform::create_new_no_follow(part).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            anyhow!(
                "{} already exists, and HatchZip does not replace a file it did not create. \
                 Move it, then run the extraction again",
                part.display()
            )
        } else {
            anyhow::Error::new(e).context(format!("creating {}", part.display()))
        }
    })
}

/// Opens a streamed file's part file positioned at `written`, cutting off
/// anything written after that point.
fn open_part(dest: &Path, part: &Path, written: u64) -> Result<File> {
    if written == 0 {
        return create_part(part);
    }
    if fs::symlink_metadata(part).is_err() && fs::symlink_metadata(dest).is_ok() {
        // The previous run finished the file and renamed it, then stopped
        // before committing. The point it journaled still holds.
        fs::rename(dest, part).with_context(|| format!("reopening {}", dest.display()))?;
    }
    let mut file = match platform::open_existing_no_follow(part) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => bail!(
            "{} is missing, and the archive data it was extracted from is already released, so \
             this file cannot be completed",
            part.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("reopening {}", part.display())),
    };
    if file.metadata()?.len() < written {
        bail!(
            "{} is shorter than when its progress was saved; the extraction cannot continue",
            part.display()
        );
    }
    file.set_len(written)?;
    file.seek(SeekFrom::Start(written))?;
    Ok(file)
}

/// A file of a compressed tar being written to its part file.
struct OpenFile {
    item: Item,
    dest: PathBuf,
    part: PathBuf,
    file: File,
    written: u64,
}

impl OpenFile {
    /// Gives the finished file its metadata and its real name.
    fn finish(self) -> Result<()> {
        if self.written != self.item.size {
            bail!("it is smaller than the archive records");
        }
        if let Some(t) = self.item.mtime {
            self.file.set_modified(t)?;
        }
        platform::apply_mode(&self.file, self.item.mode)?;
        if platform::SYNC_EACH_FILE {
            self.file.sync_all()?;
        }
        drop(self.file);
        fs::rename(&self.part, &self.dest)?;
        Ok(())
    }
}

/// The output just before `written`, which a deflate stream may refer back
/// to. It is on disk, flushed before the point was journaled, so the
/// journal doesn't have to carry it.
fn read_window(file: &mut File, written: u64) -> Result<Vec<u8>> {
    let n = written.min(resumable::WINDOW as u64);
    let mut window = vec![0; n as usize];
    file.seek(SeekFrom::Start(written - n))?;
    file.read_exact(&mut window)?;
    file.seek(SeekFrom::Start(written))?;
    Ok(window)
}

pub struct Progress {
    pub on_bytes: Box<dyn Fn(u64) + Sync>,
    pub on_commit: Box<dyn Fn(u64) + Sync>,
}

pub enum Outcome {
    Finished,
    Paused,
}

pub struct Run<'a> {
    pub job: Job,
    pub journal: Journal,
    pub paths: Paths,
    pub stop: &'a AtomicBool,
    pub progress: Progress,
    /// Free space the run must never eat into.
    pub keep_free: u64,
}

impl Run<'_> {
    pub fn execute(mut self) -> Result<Outcome> {
        let out = self.journal.output.clone();
        fs::create_dir_all(&out)
            .with_context(|| format!("creating output folder {}", out.display()))?;

        // Keeping the archive frees nothing, which is exactly the case of an
        // output on another disk: nothing is streamed or punched.
        let reclaims =
            !self.journal.keep && fsutil::same_filesystem(self.job.storage.primary(), &out);
        if self.journal.keep {
            self.job.storage.protect();
        }
        if let Some(r) = self.journal.replace.as_mut()
            && !r.moved
        {
            replace::set_aside(&self.job, &out)?;
            r.moved = true;
            self.journal.save(&self.paths)?;
            self.job.started = true;
        }
        let mut next = 0;
        while next < self.job.units.len() {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(Outcome::Paused);
            }
            if let Some(solid) = self.job.format.solid() {
                let walk = solid.walk(self.job.resumed(next))?;
                let Some(links) = self.stream_solid(next, walk, reclaims, &out)? else {
                    return Ok(Outcome::Paused);
                };
                self.commit(next, next + 1, links, &out)?;
                next += 1;
                continue;
            }
            if let Some(stream) = self.stream_now(next, reclaims, &out)? {
                if !self.stream_unit(next, &stream, &out)? {
                    return Ok(Outcome::Paused);
                }
                self.commit(next, next + 1, Vec::new(), &out)?;
                next += 1;
                continue;
            }
            let end = self.next_batch(next, &out, reclaims)?;
            let batch = &self.job.units[next..end];

            let Some(links) = self.extract_batch(batch, &out)? else {
                return Ok(Outcome::Paused);
            };
            self.commit(next, end, links, &out)?;
            next = end;
        }

        self.finish(&out)?;
        Ok(Outcome::Finished)
    }

    /// Commits `units[start..end]`, all extracted, and truncates the archive
    /// behind them.
    fn commit(&mut self, start: usize, end: usize, links: Vec<Link>, out: &Path) -> Result<()> {
        let batch = &self.job.units[start..end];
        // The journal must never claim a file that a power cut could still
        // take back, since its archive bytes are about to be truncated.
        platform::sync_batch(out).context("flushing extracted files to disk")?;
        if platform::SYNC_EACH_FILE {
            // Links are only recorded here and made at the end, so their
            // folders may not exist yet.
            let dirs: HashSet<PathBuf> = batch
                .iter()
                .flat_map(|u| &u.items)
                .filter(|it| matches!(it.kind, Kind::File | Kind::Dir))
                .filter_map(|it| out.join(&it.path).parent().map(Path::to_path_buf))
                .collect();
            for d in &dirs {
                fsutil::sync_dir(d)?;
            }
        }
        crash_point("after-extract");

        // Other programs on the computer, antivirus software above all, can
        // remove a file after it is written. Claiming one in the journal
        // would shorten the archive behind the only copy left. A streamed
        // unit with a resume point is skipped: some of its archive is
        // already freed, so some of its output may have no other copy, and
        // a rerun resumes past it.
        if self.journal.resume.is_none() {
            remove_after_extract(batch, out);
            verify_extracted(batch, out)?;
        }

        let new_len = batch.last().expect("batches are never empty").start;
        self.journal.committed_len = new_len;
        self.journal.links.extend(links);
        // A streamed unit's point is spent once the unit itself is claimed;
        // both change in the same atomic write.
        self.journal.resume = None;
        self.journal.done = end == self.job.units.len();
        self.job.resume = None;
        self.journal.save(&self.paths)?;
        crash_point("after-journal");

        if self.journal.keep {
            return Ok(());
        }
        self.job
            .storage
            .set_len(new_len)
            .context("shrinking the archive")?;
        (self.progress.on_commit)(new_len);
        Ok(())
    }

    /// Whether `units[i]` is extracted by streaming: always if it is half
    /// streamed already, otherwise if it can be and its output wouldn't fit
    /// whole in the space there is right now. When it fits, the ordinary
    /// path is faster, since it extracts many units in parallel.
    fn stream_now(&self, i: usize, reclaims: bool, out: &Path) -> Result<Option<Stream>> {
        if self.job.resumed(i).is_some() {
            return match self.job.stream_of(i) {
                Some(s) => Ok(Some(s)),
                None => bail!(
                    "the journal records '{}' as partially extracted, but it cannot be resumed",
                    self.job.units[i].items[0].path.display()
                ),
            };
        }
        let Some(stream) = self.job.streams(i, reclaims) else {
            return Ok(None);
        };
        if always_stream() {
            return Ok(Some(stream));
        }
        let avail = fsutil::available_space(out)?.saturating_sub(self.keep_free);
        Ok((on_disk(&self.job.units[i]) > avail).then_some(stream))
    }

    /// Extracts the one file of `units[i]` while freeing the archive bytes
    /// behind it. Returns false if a stop request interrupted it; the last
    /// resume point stays in the journal and the next run continues there.
    ///
    /// The order at every point is what makes a crash harmless: the output
    /// up to the point is flushed, then the point is journaled, and only then
    /// is the archive punched, never past the byte the point restarts in. A
    /// crash before the journal write leaves the previous point in force,
    /// and everything it needs is still in the archive.
    fn stream_unit(&mut self, i: usize, stream: &Stream, out: &Path) -> Result<bool> {
        let unit_start = self.job.units[i].start;
        let item = self.job.units[i].items[0].clone();
        let dest = out.join(&item.path);
        let part = part_path(&dest);
        let at = self.job.resumed(i).cloned().unwrap_or(Resume {
            unit: unit_start,
            written: 0,
            bit: stream.data_start * 8,
            crc: 0,
            stream: None,
        });
        let started_here = at.written == 0;
        let what = || format!("extracting '{}'", item.path.display());
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(what)?;
        }
        // Opened before anything below may delete it: if something is in the
        // way of a new part file, it is not ours to remove.
        let file = open_part(&dest, &part, at.written).with_context(what)?;
        let result = self
            .stream_file(&item, stream, at, file, &dest, &part, out)
            .with_context(what);
        if result.is_err() && started_here && self.journal.resume.is_none() {
            // Nothing was punched yet, so the next run starts this file over.
            let _ = fs::remove_file(&part);
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn stream_file(
        &mut self,
        item: &Item,
        stream: &Stream,
        at: Resume,
        mut file: File,
        dest: &Path,
        part: &Path,
        out: &Path,
    ) -> Result<bool> {
        let window = match stream.codec {
            Codec::Stored => Vec::new(),
            Codec::Deflate => read_window(&mut file, at.written)?,
        };
        let mut decoder = Decoder::new(self.job.storage.reader(None), stream, at.bit, &window)?;
        let mut crc = crc32fast::Hasher::new_with_initial(at.crc);
        let mut written = at.written;
        let mut since_point = 0;
        let step = stream_step();
        let mut punching = self.job.punch_support();
        let what = format!("'{}'", item.path.display());
        self.ensure_room(out, &what, item.size - written, punching)?;

        let mut buf = vec![0; BUF_SIZE];
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let n = decoder.read(&mut buf)?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > item.size {
                bail!("it is larger than the archive records");
            }
            file.write_all(&buf[..n])?;
            crc.update(&buf[..n]);
            (self.progress.on_bytes)(n as u64);
            since_point += n as u64;

            if since_point < step || written == item.size {
                continue;
            }
            match decoder.restart_point() {
                Some(bit) => {
                    let point = Resume {
                        unit: at.unit,
                        written,
                        bit,
                        crc: crc.clone().finalize(),
                        stream: None,
                    };
                    punching = self.save_point(&file, part, out, point, stream, punching)?;
                    since_point = 0;
                    self.ensure_room(out, &what, item.size - written, punching)?;
                }
                // A deflate block runs as long as its encoder likes; zlib-ng
                // and zlib-rs write level 1 as a single block. Until it ends
                // there is nowhere to resume from, so nothing can be freed,
                // and the run has to stop before the disk fills.
                None => {
                    let avail = fsutil::available_space(out)?.saturating_sub(self.keep_free);
                    if avail < 2 * BUF_SIZE as u64 {
                        bail!(
                            "not enough disk space to continue: the compressed data of this entry has \
                             no resume point in the last {}, so its archive space cannot be \
                             released yet. {} more is required (the remainder of this file). \
                             Progress is saved; free up space and resume the extraction.",
                            human(since_point),
                            human(item.size - written),
                        );
                    }
                }
            }
        }
        if written != item.size {
            bail!("it is smaller than the archive records");
        }
        if let Some(expected) = stream.crc
            && crc.finalize() != expected
        {
            bail!("its data is damaged (CRC mismatch)");
        }
        if let Some(t) = item.mtime {
            file.set_modified(t)?;
        }
        platform::apply_mode(&file, item.mode)?;
        file.sync_all()?;
        drop(file);
        // From here a crash leaves the finished file under its real name
        // while the journal still holds the last point; `open_part` takes it
        // back as the part file.
        fs::rename(part, dest)?;
        Ok(true)
    }

    /// Makes `point` the one a crash resumes from, then frees the archive
    /// bytes it no longer needs. Returns whether to keep punching.
    fn save_point(
        &mut self,
        file: &File,
        part: &Path,
        out: &Path,
        point: Resume,
        stream: &Stream,
        punching: Option<u64>,
    ) -> Result<Option<u64>> {
        file.sync_all()
            .context("flushing the extracted file to disk")?;
        // The part file and the folders above it may be new; a point that
        // outlives its file would resume into nothing.
        let mut dir = part.parent();
        while let Some(d) = dir {
            fsutil::sync_dir(d)?;
            if d == out {
                break;
            }
            dir = d.parent();
        }
        crash_point("stream-after-sync");

        self.journal.resume = Some(point.clone());
        self.journal.save(&self.paths)?;
        self.job.resume = Some(point.clone());
        crash_point("stream-after-journal");

        // Decoding restarts inside the byte holding `point.bit`, so that byte
        // and everything after it must stay.
        if punching.is_some()
            && !self
                .job
                .storage
                .punch(stream.data_start, point.bit / 8)
                .context("freeing extracted archive bytes")?
        {
            // The probe said this works; if the filesystem disagrees after
            // all, the run continues without freeing anything mid-file.
            return Ok(None);
        }
        crash_point("stream-after-punch");
        Ok(punching)
    }

    /// Extracts a compressed tar, `units[i]`, front to back from `walk`,
    /// freeing the compressed bytes behind the decoder. Returns the links
    /// found since the last resume point, for the commit, or `None` if a
    /// stop request interrupted it; the last point stays in the journal and
    /// the next run continues there.
    ///
    /// Each point follows the order of [`stream_unit`](Self::stream_unit):
    /// every file written before it is flushed, then the point is journaled,
    /// with the links found so far, and only then is the archive punched up
    /// to the byte it restarts in. Files finished after the last point are
    /// written again on resume, the half-written one is cut back to the
    /// point.
    fn stream_solid(
        &mut self,
        i: usize,
        mut walk: Box<dyn SolidWalker>,
        reclaims: bool,
        out: &Path,
    ) -> Result<Option<Vec<Link>>> {
        let unit = self.job.units[i].start;
        let step = stream_step();
        // With the output on another disk, freeing the archive's space
        // doesn't help it, and the space check didn't count on it.
        let mut punching = self.job.punch_support().filter(|_| reclaims);
        let profile = match self.job.format.solid() {
            Some(s) => s.profile().clone(),
            None => bail!("the archive is not a compressed stream"),
        };
        let mut left = profile.files;
        // This run hasn't punched anything yet. Its first punch starts at 0,
        // in case the last run stopped between journaling a point and
        // punching up to it.
        let mut punched = 0;
        let mut since_point = 0u64;
        let mut checked = 0u64;
        let mut open: Option<OpenFile> = None;
        let mut links = Vec::new();
        let mut dirs = HashSet::new();
        self.ensure_room(out, "the archive", left, punching)?;

        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            match walk.next(BUF_SIZE)? {
                Event::Entry { item, link, done } => {
                    let dest = out.join(&item.path);
                    let what = || format!("extracting '{}'", item.path.display());
                    match &item.kind {
                        Kind::Dir => {
                            fs::create_dir_all(&dest).with_context(what)?;
                            dirs.insert(dest);
                        }
                        Kind::Symlink => {
                            let target = String::from_utf8(link.unwrap_or_default())
                                .map_err(|_| anyhow!("the link target is not valid UTF-8"))
                                .with_context(what)?;
                            links.push(Link {
                                path: item.path.clone(),
                                target: PathBuf::from(target),
                                hard: false,
                            });
                        }
                        Kind::HardLink(target) => links.push(Link {
                            path: item.path.clone(),
                            target: target.clone(),
                            hard: true,
                        }),
                        Kind::File => {
                            if let Some(parent) = dest.parent() {
                                fs::create_dir_all(parent).with_context(what)?;
                            }
                            let part = part_path(&dest);
                            let file = open_part(&dest, &part, done).with_context(what)?;
                            open = Some(OpenFile {
                                item,
                                dest,
                                part,
                                file,
                                written: done,
                            });
                        }
                    }
                }
                Event::Data(bytes) => {
                    let f = open
                        .as_mut()
                        .ok_or_else(|| anyhow!("the tar stream holds data outside a file"))?;
                    f.file
                        .write_all(bytes)
                        .with_context(|| format!("extracting '{}'", f.item.path.display()))?;
                    let n = bytes.len() as u64;
                    f.written += n;
                    since_point += n;
                    left = left.saturating_sub(n);
                    (self.progress.on_bytes)(n);
                    // A deflate block runs as long as its encoder likes, and
                    // until it ends there is nowhere to resume from, so nothing
                    // can be freed: stop before the disk fills.
                    if punching.is_some()
                        && since_point >= step
                        && since_point - checked >= BUF_SIZE as u64
                    {
                        checked = since_point;
                        let avail = fsutil::available_space(out)?.saturating_sub(self.keep_free);
                        if avail < 2 * BUF_SIZE as u64 {
                            bail!(
                                "not enough disk space to continue: the compressed data has no \
                                 resume point in the last {}, so its space cannot be released \
                                 yet. {} more is required (the remainder of the archive). \
                                 Progress is saved; free up space and resume the extraction.",
                                human(since_point),
                                human(left),
                            );
                        }
                    }
                }
                Event::FileEnd => {
                    let f = open
                        .take()
                        .ok_or_else(|| anyhow!("the tar stream ends a file it never began"))?;
                    let pad = f.item.size.next_multiple_of(4096) - f.item.size;
                    let path = f.item.path.clone();
                    let parent = f.dest.parent().map(Path::to_path_buf);
                    f.finish()
                        .with_context(|| format!("extracting '{}'", path.display()))?;
                    dirs.extend(parent);
                    since_point += pad;
                    left = left.saturating_sub(pad);
                }
                Event::Point => {
                    if since_point < step {
                        continue;
                    }
                    let mut point = walk.point()?;
                    point.unit = unit;
                    punching = self.solid_point(
                        point,
                        open.as_ref(),
                        &mut links,
                        &mut dirs,
                        out,
                        punching,
                        &mut punched,
                    )?;
                    since_point = 0;
                    checked = 0;
                    self.ensure_room(out, "the archive", left, punching)?;
                }
                Event::End => break,
            }
        }
        if open.is_some() {
            bail!("the tar stream ended inside a file");
        }
        Ok(Some(links))
    }

    /// Makes `point` the one a crash resumes from, then frees the archive
    /// bytes before it. Returns whether to keep punching.
    #[allow(clippy::too_many_arguments)]
    fn solid_point(
        &mut self,
        point: Resume,
        open: Option<&OpenFile>,
        links: &mut Vec<Link>,
        dirs: &mut HashSet<PathBuf>,
        out: &Path,
        punching: Option<u64>,
        punched: &mut u64,
    ) -> Result<Option<u64>> {
        if let Some(f) = open {
            f.file
                .sync_all()
                .context("flushing the extracted file to disk")?;
            dirs.extend(f.part.parent().map(Path::to_path_buf));
        }
        platform::sync_batch(out).context("flushing extracted files to disk")?;
        if platform::SYNC_EACH_FILE {
            // Files were flushed as they were finished; their names live in
            // the folders, which may be new themselves.
            let mut done = HashSet::new();
            for d in dirs.drain() {
                let mut dir = Some(d.as_path());
                while let Some(d) = dir {
                    if !done.insert(d.to_path_buf()) {
                        break;
                    }
                    fsutil::sync_dir(d)?;
                    if d == out {
                        break;
                    }
                    dir = d.parent();
                }
            }
        }
        dirs.clear();
        crash_point("stream-after-sync");

        let byte = point.bit / 8;
        self.journal.links.append(links);
        self.journal.resume = Some(point.clone());
        self.journal.save(&self.paths)?;
        self.job.resume = Some(point);
        crash_point("stream-after-journal");

        if punching.is_some() {
            if !self
                .job
                .storage
                .punch(*punched, byte)
                .context("freeing extracted archive bytes")?
            {
                return Ok(None);
            }
            *punched = byte;
            (self.progress.on_commit)(self.job.top - byte);
        }
        crash_point("stream-after-punch");
        Ok(punching)
    }

    /// Stops a stream at a resume point, before it writes what doesn't fit.
    /// `left` is the output still to write.
    fn ensure_room(&self, out: &Path, what: &str, left: u64, punching: Option<u64>) -> Result<()> {
        let avail = fsutil::available_space(out)?.saturating_sub(self.keep_free);
        let left = left.next_multiple_of(4096);
        let need = match punching {
            Some(granule) => (stream_step() + BUF_SIZE as u64 + 2 * granule).min(left),
            None => left,
        };
        if need > avail {
            bail!(
                "not enough disk space to continue: the next part of {} requires {} and {} is \
                 available ({} reserved).\n{} more is required. Progress is saved; free up space \
                 and resume the extraction.",
                what,
                human(need),
                human(avail),
                human(self.keep_free),
                human(need - avail),
            );
        }
        Ok(())
    }

    /// Pick units starting at `start` whose output fits in the space there is
    /// right now. Always takes at least one, or explains why it can't.
    fn next_batch(&self, start: usize, out: &Path, reclaims: bool) -> Result<usize> {
        let avail = fsutil::available_space(out)?.saturating_sub(self.keep_free);
        let units = &self.job.units;
        let need = on_disk;
        let first = need(&units[start]);
        if first > avail {
            // The upfront check makes this rare: it takes something else on
            // the machine filling the disk while we run.
            let what = match units[start].items.as_slice() {
                [one] => format!("'{}'", one.path.display()),
                many => format!("the next block of {} files", many.len()),
            };
            bail!(
                "not enough disk space to continue: {what} requires {} and {} is available \
                 ({} reserved).\n{} more is required. Progress is saved; free up space and \
                 resume the extraction.",
                human(first),
                human(avail),
                human(self.keep_free),
                human(first - avail),
            );
        }
        let budget = avail.min(BATCH_MAX_BYTES.max(first));
        let max_units = test_knob("HATCHZIP_BATCH_ENTRIES").map_or(BATCH_MAX_UNITS, |n| n as usize);
        let mut used = 0u64;
        let mut end = start;
        while end < units.len() && end - start < max_units {
            let size = need(&units[end]);
            if end > start && used.saturating_add(size) > budget {
                break;
            }
            if end > start && always_stream() && self.job.streams(end, reclaims).is_some() {
                break;
            }
            used += size;
            end += 1;
        }
        Ok(end)
    }

    /// Returns `None` if a stop request interrupted the batch. Nothing in the
    /// archive has been touched at that point, so the batch simply reruns.
    fn extract_batch(&self, batch: &[Unit], out: &Path) -> Result<Option<Vec<Link>>> {
        let on_bytes = &*self.progress.on_bytes;
        if self.job.format.serial() {
            let mut sink = DiskSink {
                out,
                stop: self.stop,
                on_bytes,
                buf: vec![0; BUF_SIZE],
                links: Vec::new(),
            };
            let mut worker = self
                .job
                .format
                .worker(1)
                .map_err(|e| anyhow!("opening the archive: {e:#}"))?;
            // Lowest first, so the worker reads the archive front to back.
            for unit in batch.iter().rev() {
                match worker.extract(unit, &mut sink) {
                    Ok(()) => {}
                    Err(e) if is_interrupt(&e) => return Ok(None),
                    Err(e) => return Err(e),
                }
            }
            return Ok(Some(sink.links));
        }
        let threads = threads_per_unit(batch.len());
        let results: Vec<Result<Vec<Link>>> = batch
            .par_iter()
            .map_init(
                || {
                    let sink = DiskSink {
                        out,
                        stop: self.stop,
                        on_bytes,
                        buf: vec![0; BUF_SIZE],
                        links: Vec::new(),
                    };
                    (self.job.format.worker(threads), sink)
                },
                |(worker, sink), unit| {
                    let w = worker
                        .as_mut()
                        .map_err(|e| anyhow!("opening the archive: {e:#}"))?;
                    w.extract(unit, sink)?;
                    Ok(std::mem::take(&mut sink.links))
                },
            )
            .collect();

        let mut links = Vec::new();
        let mut interrupted = false;
        for r in results {
            match r {
                Ok(l) => links.extend(l),
                Err(e) if is_interrupt(&e) => interrupted = true,
                Err(e) => return Err(e),
            }
        }
        Ok((!interrupted).then_some(links))
    }

    fn finish(self, out: &Path) -> Result<()> {
        make_final_links(&self.journal.links, out)?;
        // Before the journal goes, so a crash in between deletes the rest
        // on resume rather than leaving it.
        if let Some(r) = &self.journal.replace {
            replace::settle(out, r)?;
            replace::remove(out)?;
        }
        if self.journal.keep {
            // The archive stays as it was; only the progress files go.
            return self.paths.remove();
        }
        let Self { job, paths, .. } = self;
        let Job {
            storage, format, ..
        } = job;
        // The format holds its own handle on the storage; it has to go first.
        drop(format);
        let storage = Arc::try_unwrap(storage)
            .map_err(|_| anyhow!("archive still in use at the end of extraction"))?;
        // What is left is at most a self-extractor stub in front of the
        // first entry. The saved index goes with it.
        storage.remove()?;
        paths.remove()
    }
}

/// Checks that every file `batch` extracted is whole on disk under its final
/// name. The journal is about to claim them and free the archive bytes
/// behind them, so a file that another program removed or changed has to
/// stop the run here: its unit keeps its place in the journal and the
/// archive still holds it, and a rerun extracts it again.
///
/// Links are made at the very end, once the archive is gone, so link
/// entries have nothing on disk yet.
fn verify_extracted(batch: &[Unit], out: &Path) -> Result<()> {
    for item in batch.iter().flat_map(|u| &u.items) {
        if item.kind != Kind::File {
            continue;
        }
        let dest = out.join(&item.path);
        // Never follows a link: what matters is what sits at the name.
        let meta = match fs::symlink_metadata(&dest) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => bail!(
                "'{}' disappeared while it was extracted. Antivirus software sometimes removes \
                 files as they are written. The archive still holds it ({} bytes), so running \
                 the same command again extracts it",
                item.path.display(),
                item.size
            ),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("checking '{}' after extracting it", item.path.display())
                });
            }
        };
        if !meta.is_file() {
            bail!(
                "'{}' is no longer a file after it was extracted, but the archive has a file \
                 there. The archive still holds it, so move what is in its place away and run \
                 the same command again",
                item.path.display()
            );
        }
        if meta.len() != item.size {
            bail!(
                "'{}' changed while it was extracted: the archive records {} bytes, but the \
                 file has {}. The archive still holds it, so move it away and run the same \
                 command again",
                item.path.display(),
                item.size,
                meta.len()
            );
        }
    }
    Ok(())
}

/// Lets the test suite remove an extracted file between the batch reaching
/// the disk and the journal claiming it, as antivirus software might.
fn remove_after_extract(batch: &[Unit], out: &Path) {
    if !cfg!(debug_assertions) {
        return;
    }
    let Ok(rel) = std::env::var("HATCHZIP_REMOVE_AFTER_EXTRACT") else {
        return;
    };
    let rel = Path::new(&rel);
    let extracted = batch
        .iter()
        .flat_map(|u| &u.items)
        .any(|item| item.kind == Kind::File && item.path == rel);
    if extracted {
        let _ = fs::remove_file(out.join(rel));
    }
}

/// A run that crashed after deleting the archive but before cleaning up only
/// has its links and side files left to deal with.
pub fn finish_orphaned(journal: &Journal, paths: &Paths) -> Result<()> {
    make_final_links(&journal.links, &journal.output)?;
    if let Some(r) = &journal.replace {
        replace::settle(&journal.output, r)?;
        replace::remove(&journal.output)?;
    }
    // The final commit was interrupted before the archive was removed, and
    // all of it is free to go now. A kept archive stays.
    if journal.done && !journal.keep {
        for (part, _) in &journal.parts {
            match fs::remove_file(part) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("deleting {}", part.display()));
                }
                _ => {}
            }
        }
    }
    paths.remove()
}

/// The last step of a run, when every file is extracted and the archive is
/// gone: a link that can't be made is all that is left. The journal keeps
/// the links, so once the cause is fixed a resumed run makes them and
/// finishes.
fn make_final_links(links: &[Link], out: &Path) -> Result<()> {
    make_links(links, out).map_err(|e| {
        anyhow!(
            "every file is extracted, but {e:#}\nThe links are kept. Resolve the problem shown for \
             each one, for example by moving or renaming a folder that is in the way, then \
             resume the extraction to create them"
        )
    })
}

/// Makes `links`. Each one is tried, and the error lists every one that
/// failed and why. Links that were made are made again by the next call,
/// replacing themselves.
pub(crate) fn make_links(links: &[Link], out: &Path) -> Result<()> {
    let failed: Vec<(&Link, anyhow::Error)> = links
        .iter()
        .filter_map(|link| make_link(link, out).err().map(|e| (link, e)))
        .collect();
    if failed.is_empty() {
        return Ok(());
    }
    let mut msg = match failed.len() {
        1 => "1 link could not be created:".to_string(),
        n => format!("{n} links could not be created:"),
    };
    for (link, e) in failed.iter().take(10) {
        let kind = if link.hard {
            "hard link"
        } else {
            "symbolic link"
        };
        msg.push_str(&format!(
            "\n  '{}' ({kind} to '{}'): {e:#}",
            link.path.display(),
            link.target.display()
        ));
    }
    if failed.len() > 10 {
        msg.push_str(&format!("\n  and {} more", failed.len() - 10));
    }
    Err(anyhow!(msg))
}

fn make_link(link: &Link, out: &Path) -> Result<()> {
    // A run an older release started may hold a link inside another link.
    // Going through it would write outside the output folder.
    if let Some(via) = link_on_the_way(out, &link.path) {
        bail!(
            "'{}' is a link, and this one would be created through it",
            via.display()
        );
    }
    let path = out.join(&link.path);
    if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
        bail!("a folder already exists at this location");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if link.hard {
        if let Some(via) = link_on_the_way(out, &link.target) {
            bail!(
                "'{}' is a link, and the target would be reached through it",
                via.display()
            );
        }
        let target = out.join(&link.target);
        let _ = fs::remove_file(&path);
        // FAT32 and exFAT have no hard links; a copy is the next best thing.
        fs::hard_link(&target, &path).or_else(|_| fs::copy(&target, &path).map(|_| ()))?;
    } else {
        platform::create_symlink(&path, &link.target)?;
    }
    Ok(())
}

/// The first folder on the way to `rel` inside `out` that is really a link.
pub(crate) fn link_on_the_way(out: &Path, rel: &Path) -> Option<PathBuf> {
    let mut ancestors: Vec<&Path> = rel
        .ancestors()
        .skip(1)
        .filter(|p| !p.as_os_str().is_empty())
        .collect();
    ancestors.reverse();
    ancestors
        .into_iter()
        .find(|p| fs::symlink_metadata(out.join(p)).is_ok_and(|m| m.is_symlink()))
        .map(Path::to_path_buf)
}

struct DiskSink<'a> {
    out: &'a Path,
    stop: &'a AtomicBool,
    on_bytes: &'a (dyn Fn(u64) + Sync),
    buf: Vec<u8>,
    links: Vec<Link>,
}

impl Sink for DiskSink<'_> {
    fn item(&mut self, item: &Item, data: &mut dyn Read) -> Result<()> {
        self.write(item, data)
            .with_context(|| format!("extracting '{}'", item.path.display()))
    }
}

impl DiskSink<'_> {
    fn write(&mut self, item: &Item, data: &mut dyn Read) -> Result<()> {
        let dest = self.out.join(&item.path);
        match &item.kind {
            Kind::Dir => {
                fs::create_dir_all(&dest)?;
                return Ok(());
            }
            Kind::Symlink => {
                let mut target = Vec::new();
                data.take(MAX_SYMLINK).read_to_end(&mut target)?;
                let target = PathBuf::from(String::from_utf8(target)?);
                self.links.push(Link {
                    path: item.path.clone(),
                    target,
                    hard: false,
                });
                return Ok(());
            }
            Kind::HardLink(target) => {
                self.links.push(Link {
                    path: item.path.clone(),
                    target: target.clone(),
                    hard: true,
                });
                return Ok(());
            }
            Kind::File => {}
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let part = part_path(&dest);
        // Only a part file this call created is removed on failure.
        let dst = create_part(&part)?;
        let result = self.copy(item, data, dst);
        if result.is_err() {
            let _ = fs::remove_file(&part);
            return result;
        }
        fs::rename(&part, &dest)?;
        Ok(())
    }

    fn copy(&mut self, item: &Item, data: &mut dyn Read, mut dst: File) -> Result<()> {
        let mut written = 0u64;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err(io::Error::from(io::ErrorKind::Interrupted).into());
            }
            // Decoders check their CRC when the stream ends, so a read error
            // here is how corruption surfaces.
            let n = data.read(&mut self.buf)?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > item.size {
                bail!("it is larger than the archive records");
            }
            dst.write_all(&self.buf[..n])?;
            (self.on_bytes)(n as u64);
        }
        if written != item.size {
            bail!("it is smaller than the archive records");
        }
        if let Some(t) = item.mtime {
            dst.set_modified(t)?;
        }
        platform::apply_mode(&dst, item.mode)?;
        if platform::SYNC_EACH_FILE {
            dst.sync_all()?;
        }
        Ok(())
    }
}

/// Reads at most `left` bytes, then stops the decoder with [`ProbeDone`].
struct ProbeSink {
    left: u64,
    buf: Vec<u8>,
}

#[derive(Debug)]
struct ProbeDone;

impl std::fmt::Display for ProbeDone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("read enough to prove the password")
    }
}

impl std::error::Error for ProbeDone {}

impl Sink for ProbeSink {
    fn item(&mut self, _item: &Item, data: &mut dyn Read) -> Result<()> {
        loop {
            let n = data.read(&mut self.buf)?;
            if n == 0 {
                return Ok(());
            }
            self.left = self.left.saturating_sub(n as u64);
            if self.left == 0 {
                return Err(ProbeDone.into());
            }
        }
    }
}

struct VerifySink<'a> {
    on_bytes: &'a (dyn Fn(u64) + Sync),
    buf: Vec<u8>,
}

impl Sink for VerifySink<'_> {
    fn item(&mut self, item: &Item, data: &mut dyn Read) -> Result<()> {
        let mut read = 0;
        loop {
            let n = data
                .read(&mut self.buf)
                .with_context(|| format!("'{}'", item.path.display()))?;
            if n == 0 {
                break;
            }
            read += n as u64;
            (self.on_bytes)(n as u64);
        }
        if item.kind == Kind::File && read != item.size {
            bail!(
                "'{}' is shorter than the archive records",
                item.path.display()
            );
        }
        Ok(())
    }
}

fn is_interrupt(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<io::Error>())
        .any(|io| io.kind() == io::ErrorKind::Interrupted)
}

/// Settings only the test suite uses. Release builds ignore them.
fn test_knob(name: &str) -> Option<u32> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var(name).ok()?.parse().ok()
}

/// Lets the test suite kill the process at the two moments that matter for
/// crash safety: after a batch is on disk, and after it is journaled.
pub(crate) fn crash_point(name: &str) {
    if !cfg!(debug_assertions) || std::env::var("HATCHZIP_CRASH_AT").map_or(true, |v| v != name) {
        return;
    }
    static PASSED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if PASSED.fetch_add(1, Ordering::Relaxed) >= test_knob("HATCHZIP_CRASH_AFTER").unwrap_or(0) {
        std::process::abort();
    }
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{Opened, Worker};

    /// Stands in for a format: these tests only plan, they never decode.
    struct NoFormat;

    impl Format for NoFormat {
        fn worker(&self, _threads: u32) -> Result<Box<dyn Worker + '_>> {
            bail!("nothing is decoded in these tests")
        }
    }

    fn unit(start: u64, name: &str, size: u64) -> Unit {
        Unit {
            start,
            key: 0,
            items: vec![Item {
                path: name.into(),
                size,
                kind: Kind::File,
                mode: None,
                mtime: None,
            }],
            encrypted: false,
        }
    }

    /// A 64 byte file standing in for an archive, in its own folder.
    fn storage(test: &str) -> (PathBuf, Arc<Storage>) {
        let dir = std::env::temp_dir().join(format!("hatchzip-{test}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("archive.bin");
        fs::write(&path, [0u8; 64]).unwrap();
        let storage = Arc::new(Storage::open(&path, false).unwrap());
        (dir, storage)
    }

    fn opened(units: Vec<Unit>, tail_start: u64) -> Opened {
        Opened {
            name: "test",
            format: Box::new(NoFormat),
            units,
            tail_start,
        }
    }

    /// An empty folder of this test's own, under the system's temporary one.
    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hatchzip-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// What a batch claims must be on disk, whole, or the journal would
    /// truncate the archive behind a file that another program took away.
    #[test]
    fn a_batch_that_is_all_on_disk_verifies() {
        let out = scratch("verify-complete");
        fs::create_dir_all(out.join("game")).unwrap();
        fs::write(out.join("game/a"), b"hello").unwrap();
        fs::write(out.join("game/empty"), b"").unwrap();
        let batch = [unit(10, "game/a", 5), unit(20, "game/empty", 0)];
        assert!(verify_extracted(&batch, &out).is_ok());
        fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn a_missing_file_fails_verification() {
        let out = scratch("verify-missing");
        let err = verify_extracted(&[unit(0, "game/gone", 12)], &out)
            .err()
            .unwrap();
        let text = err.to_string();
        assert!(text.contains("game/gone"), "{text}");
        assert!(text.contains("disappeared"), "{text}");
        assert!(text.contains("12 bytes"), "{text}");
        fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn a_file_of_the_wrong_size_fails_verification() {
        let out = scratch("verify-size");
        fs::create_dir_all(out.join("game")).unwrap();
        fs::write(out.join("game/a"), b"abc").unwrap();
        let err = verify_extracted(&[unit(0, "game/a", 5)], &out)
            .err()
            .unwrap();
        let text = err.to_string();
        assert!(text.contains("game/a"), "{text}");
        assert!(text.contains("records 5 bytes"), "{text}");
        assert!(text.contains("has 3"), "{text}");
        fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn a_folder_at_the_name_of_a_file_fails_verification() {
        let out = scratch("verify-folder");
        fs::create_dir_all(out.join("game/a")).unwrap();
        let err = verify_extracted(&[unit(0, "game/a", 5)], &out)
            .err()
            .unwrap();
        assert!(err.to_string().contains("no longer a file"), "{err}");
        fs::remove_dir_all(&out).unwrap();
    }

    /// The link points at a file of exactly the recorded size, so only a
    /// check that never follows links can refuse it.
    #[cfg(unix)]
    #[test]
    fn a_link_at_the_name_of_a_file_fails_verification() {
        let out = scratch("verify-link");
        fs::write(out.join("real"), b"hello").unwrap();
        std::os::unix::fs::symlink("real", out.join("link")).unwrap();
        let err = verify_extracted(&[unit(0, "link", 5)], &out).err().unwrap();
        assert!(err.to_string().contains("no longer a file"), "{err}");
        fs::remove_dir_all(&out).unwrap();
    }

    /// Folders and links are not on disk yet when a batch commits; links are
    /// made at the very end.
    #[test]
    fn other_kinds_of_entries_are_not_verified() {
        let out = scratch("verify-kinds");
        let item = |kind: Kind| Item {
            path: "absent".into(),
            size: 0,
            kind,
            mode: None,
            mtime: None,
        };
        let batch = Unit {
            start: 0,
            key: 0,
            items: vec![
                item(Kind::Dir),
                item(Kind::Symlink),
                item(Kind::HardLink("absent".into())),
            ],
            encrypted: false,
        };
        assert!(verify_extracted(&[batch], &out).is_ok());
        fs::remove_dir_all(&out).unwrap();
    }

    /// Committing one of two units that start at the same offset would
    /// truncate the other before it is extracted.
    #[test]
    fn units_sharing_a_start_are_refused() {
        let (dir, storage) = storage("shared-start");
        let shared = vec![unit(10, "a", 1), unit(30, "b", 1), unit(10, "c", 1)];
        let err = Job::build(storage.clone(), opened(shared, 40), 64)
            .err()
            .unwrap();
        assert!(err.to_string().contains("same offset"), "{err:#}");
        let apart = vec![unit(10, "a", 1), unit(30, "b", 1)];
        assert!(Job::build(storage, opened(apart, 40), 64).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A unit at or past the end of the file used to be dropped as if an
    /// earlier run had extracted it, and a tail past the end can't be saved.
    #[test]
    fn data_past_the_end_of_the_file_is_refused() {
        let (dir, storage) = storage("past-the-end");
        for (starts, tail_start) in [(vec![10, 64], 40), (vec![10, 70], 40), (vec![10], 65)] {
            let units = starts
                .iter()
                .map(|&s| unit(s, &format!("f{s}"), 1))
                .collect();
            let err = Job::build(storage.clone(), opened(units, tail_start), 64)
                .err()
                .unwrap();
            assert!(err.is::<archive::Damaged>(), "{starts:?}: {err:#}");
        }
        // Resumed with the end already truncated away, it is simply done.
        let job = Job::build(
            storage,
            opened(vec![unit(10, "a", 1), unit(50, "b", 1)], 60),
            40,
        );
        assert_eq!(job.unwrap().units.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Sizes come from the archive's headers. Summed or rounded up to whole
    /// blocks in 64 bits they could wrap, and the space check would count
    /// next to nothing.
    #[test]
    fn sizes_too_large_to_add_up_are_refused() {
        let (dir, storage) = storage("huge-sizes");
        let half = 1u64 << 63;
        for sizes in [vec![half, half], vec![u64::MAX - 10], vec![(1 << 62) + 1]] {
            let units = sizes
                .iter()
                .enumerate()
                .map(|(i, &s)| unit(10 + i as u64, &format!("f{i}"), s))
                .collect();
            let err = Job::build(storage.clone(), opened(units, 40), 64)
                .err()
                .unwrap();
            assert!(err.is::<archive::Damaged>(), "{sizes:?}: {err:#}");
        }
        assert_eq!(on_disk(&unit(0, "f", u64::MAX - 10)), u64::MAX);
        let mut two = unit(0, "f", half);
        two.items.extend(unit(0, "g", half).items);
        assert_eq!(two.size(), u64::MAX);
        assert_eq!(on_disk(&two), u64::MAX);
        fs::remove_dir_all(&dir).unwrap();
    }
}
