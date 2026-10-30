//! The archive's bytes, whether one file or a split set (`.001`, `.002`, ...),
//! and the only code allowed to shrink them.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};

use crate::platform;

#[derive(Clone, Debug)]
pub struct Part {
    pub path: PathBuf,
    /// Where this part begins in the joined archive.
    pub start: u64,
    /// The part's length before extraction started.
    pub len: u64,
}

/// Bytes from the end of the archive saved to a side file, served back at
/// their original offsets once the real ones are truncated away.
pub struct Tail {
    pub start: u64,
    pub bytes: Vec<u8>,
}

pub struct Storage {
    parts: Arc<[Part]>,
    /// The first part stays open and locked for the whole run, so two runs
    /// can't eat the same archive.
    first: File,
    /// Set for an extraction that keeps the archive. Every method that
    /// changes the archive's bytes refuses from then on, so a bug elsewhere
    /// can't cost the user an archive they asked to keep.
    whole: AtomicBool,
    reads: Arc<Reads>,
    sfv_checked: AtomicBool,
}

/// How much of the archive its readers have read. Opening a compressed tar
/// decodes all of it before anything else, which takes minutes for a large
/// one, so front ends show this from another thread while an archive opens.
#[derive(Debug, Default)]
pub struct Reads {
    pub done: AtomicU64,
    /// How much the current pass reads in all: the archive's length, or
    /// less when a resumed compressed tar is read from its resume point.
    pub total: AtomicU64,
}

impl Reads {
    pub fn reset(&self) {
        self.done.store(0, Ordering::Relaxed);
        self.total.store(0, Ordering::Relaxed);
    }

    /// The share of the pass read so far, if a pass is under way.
    pub fn fraction(&self) -> Option<f64> {
        let total = self.total.load(Ordering::Relaxed);
        let done = self.done.load(Ordering::Relaxed);
        (total > 0).then(|| (done as f64 / total as f64).min(1.0))
    }
}

impl Storage {
    /// Opens `path`, plus its sibling volumes if it is part of a split set.
    pub fn open(path: &Path, writable: bool) -> Result<Self> {
        let paths = volume_set(path)?;
        let mut parts = Vec::with_capacity(paths.len());
        let mut start = 0;
        for p in paths {
            let len = fs::metadata(&p)
                .with_context(|| format!("reading {}", p.display()))?
                .len();
            parts.push(Part {
                path: p,
                start,
                len,
            });
            start += len;
        }
        Self::with_parts(parts, writable)
    }

    /// Reopens a set whose original layout was recorded in the journal.
    pub fn from_parts(recorded: &[(PathBuf, u64)], writable: bool) -> Result<Self> {
        let mut parts = Vec::with_capacity(recorded.len());
        let mut start = 0;
        for (path, len) in recorded {
            parts.push(Part {
                path: path.clone(),
                start,
                len: *len,
            });
            start += len;
        }
        Self::with_parts(parts, writable)
    }

    fn with_parts(parts: Vec<Part>, writable: bool) -> Result<Self> {
        // Extracting in place shrinks and deletes the volumes. Through a
        // link, that would happen to whatever file it points to, which need
        // not be an archive at all.
        if writable {
            for p in &parts {
                if fs::symlink_metadata(&p.path).is_ok_and(|m| m.is_symlink()) {
                    bail!(
                        "{} is a link. HatchZip changes the archive file itself, so open the file \
                         the link points to instead. Nothing was modified",
                        p.path.display()
                    );
                }
            }
        }
        let path = &parts[0].path;
        let first = if writable {
            platform::open_exclusive(path).map_err(|e| match e.kind() {
                io::ErrorKind::WouldBlock => {
                    anyhow!(
                        "{} is in use by another program or HatchZip process",
                        path.display()
                    )
                }
                _ => anyhow!(e).context(format!(
                    "opening {} for reading and writing",
                    path.display()
                )),
            })?
        } else {
            File::open(path).with_context(|| format!("opening {}", path.display()))?
        };
        Ok(Self {
            parts: parts.into(),
            first,
            whole: AtomicBool::new(false),
            reads: Arc::default(),
            sfv_checked: AtomicBool::new(false),
        })
    }

    /// Whether this storage has already been verified against an .sfv file.
    pub fn sfv_checked(&self) -> bool {
        self.sfv_checked.load(Ordering::Relaxed)
    }

    pub fn mark_sfv_checked(&self) {
        self.sfv_checked.store(true, Ordering::Relaxed);
    }

    /// From now on nothing may shrink, punch, write to or delete the
    /// archive.
    pub fn protect(&self) {
        self.whole.store(true, Ordering::Relaxed);
    }

    pub fn protected(&self) -> bool {
        self.whole.load(Ordering::Relaxed)
    }

    fn may_change(&self) -> Result<()> {
        if self.protected() {
            bail!(
                "refusing to modify {}: this extraction keeps the archive",
                self.primary().display()
            );
        }
        Ok(())
    }

    /// The first volume that has another name, if any. Shrinking a file
    /// shrinks it under every name it has, so extracting in place would
    /// empty a hard link that may be the only copy someone meant to keep.
    pub fn hard_linked(&self) -> Option<&Path> {
        self.parts
            .iter()
            .find(|p| platform::link_count(&p.path).is_ok_and(|n| n > 1))
            .map(|p| p.path.as_path())
    }

    pub fn primary(&self) -> &Path {
        &self.parts[0].path
    }

    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    pub fn record(&self) -> Vec<(PathBuf, u64)> {
        self.parts.iter().map(|p| (p.path.clone(), p.len)).collect()
    }

    /// The archive's original length.
    pub fn original_len(&self) -> u64 {
        self.parts.last().map_or(0, |p| p.start + p.len)
    }

    /// How much of the archive is still on disk, checking that what is left
    /// is an untouched prefix of the original.
    pub fn current_len(&self) -> Result<u64> {
        let mut end = 0;
        for (i, p) in self.parts.iter().enumerate() {
            let actual = match fs::metadata(&p.path) {
                Ok(m) => m.len(),
                Err(e) if e.kind() == io::ErrorKind::NotFound && i > 0 => 0,
                Err(e) => return Err(e).with_context(|| format!("reading {}", p.path.display())),
            };
            if actual > p.len {
                bail!(
                    "{} has grown since extraction started and will not be modified",
                    p.path.display()
                );
            }
            if actual == 0 && i > 0 {
                break;
            }
            if end != p.start {
                bail!(
                    "{} is missing a piece before {}",
                    self.primary().display(),
                    p.path.display()
                );
            }
            end = p.start + actual;
            if actual < p.len {
                break;
            }
        }
        Ok(end)
    }

    /// Shrinks the archive to its first `len` bytes: whole volumes past that
    /// point are deleted, the one it falls in is truncated.
    pub fn set_len(&self, len: u64) -> Result<()> {
        self.may_change()?;
        for (i, p) in self.parts.iter().enumerate().rev() {
            if p.start + p.len <= len {
                break;
            }
            if i == 0 {
                self.first.set_len(len)?;
            } else if p.start >= len {
                match fs::remove_file(&p.path) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => {
                        return Err(e).with_context(|| format!("deleting {}", p.path.display()));
                    }
                    _ => {}
                }
            } else {
                let f = OpenOptions::new().write(true).open(&p.path)?;
                if f.metadata()?.len() > len - p.start {
                    f.set_len(len - p.start)?;
                }
            }
        }
        Ok(())
    }

    /// Gives back the disk space of the bytes in `[from, to)` while every
    /// offset stays where it was; they read as zeros afterwards. Returns
    /// false if the filesystem can't. Filesystems free whole blocks only, so
    /// up to a block at each end may stay allocated until truncation.
    pub fn punch(&self, from: u64, to: u64) -> Result<bool> {
        self.may_change()?;
        for (i, p) in self.parts.iter().enumerate() {
            let (start, end) = (from.max(p.start), to.min(p.start + p.len));
            if start >= end {
                continue;
            }
            let (offset, len) = (start - p.start, end - start);
            let punched = if i == 0 {
                platform::punch_hole(&self.first, offset, len)
            } else {
                OpenOptions::new()
                    .write(true)
                    .open(&p.path)
                    .and_then(|f| platform::punch_hole(&f, offset, len))
            };
            if !punched.with_context(|| format!("freeing space in {}", p.path.display()))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Like [`set_len`](Self::set_len), but also cuts volumes that grew past
    /// their original length. Only for undoing an interrupted restore, which
    /// is the one thing that appends to an archive: the volume it appended to
    /// may have been whole, and `set_len` never touches whole volumes.
    pub fn cut_to(&self, len: u64) -> Result<()> {
        self.may_change()?;
        for (i, p) in self.parts.iter().enumerate().rev() {
            let keep = len.saturating_sub(p.start).min(p.len);
            if i > 0 && keep == 0 {
                match fs::remove_file(&p.path) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => {
                        return Err(e).with_context(|| format!("deleting {}", p.path.display()));
                    }
                    _ => {}
                }
                continue;
            }
            let shrink = |f: &File| -> io::Result<()> {
                if f.metadata()?.len() > keep {
                    f.set_len(keep)?;
                    f.sync_all()?;
                }
                Ok(())
            };
            let done = if i == 0 {
                shrink(&self.first)
            } else {
                match OpenOptions::new().write(true).open(&p.path) {
                    Ok(f) => shrink(&f),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e),
                }
            };
            done.with_context(|| format!("shortening {}", p.path.display()))?;
        }
        Ok(())
    }

    /// Writes `bytes` at `offset` of the joined archive and flushes them to
    /// disk. Bytes inside the archive go to the volume that holds them; bytes
    /// past its current end go to the last volume still there, which grows.
    pub fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.may_change()?;
        let end = self.current_len()?;
        if offset > end {
            bail!(
                "{} is shorter than expected; refusing to write past its end",
                self.primary().display()
            );
        }
        let last = self.parts.partition_point(|p| p.start < end).max(1) - 1;
        let mut at = offset;
        let mut rest = bytes;
        while !rest.is_empty() {
            let i = if at >= end {
                last
            } else {
                self.parts.partition_point(|p| p.start <= at) - 1
            };
            let p = &self.parts[i];
            let room = if i == last {
                rest.len()
            } else {
                ((p.start + p.len - at) as usize).min(rest.len())
            };
            let (now, later) = rest.split_at(room);
            self.write_part(i, at - p.start, now)
                .with_context(|| format!("writing to {}", p.path.display()))?;
            at += room as u64;
            rest = later;
        }
        Ok(())
    }

    fn write_part(&self, i: usize, at: u64, bytes: &[u8]) -> io::Result<()> {
        let write = |mut f: &File| -> io::Result<()> {
            f.seek(SeekFrom::Start(at))?;
            io::Write::write_all(&mut f, bytes)?;
            f.sync_all()
        };
        if i == 0 {
            // On Windows the locked handle is the only one allowed to write.
            write(&self.first)
        } else {
            write(&OpenOptions::new().write(true).open(&self.parts[i].path)?)
        }
    }

    /// Deletes whatever is left of every part.
    pub fn remove(self) -> Result<()> {
        self.may_change()?;
        let Self { parts, first, .. } = self;
        drop(first);
        for p in parts.iter() {
            match fs::remove_file(&p.path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("deleting {}", p.path.display()));
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn reader(&self, tail: Option<Arc<Tail>>) -> Reader {
        Reader {
            parts: self.parts.clone(),
            open: None,
            tail,
            len: self.original_len(),
            pos: 0,
            reads: self.reads.clone(),
        }
    }

    /// Counts what every reader made from now on reads into `reads`, a pass
    /// over the whole archive by default.
    pub fn count_reads(&mut self, reads: Arc<Reads>) {
        reads.done.store(0, Ordering::Relaxed);
        reads.total.store(self.original_len(), Ordering::Relaxed);
        self.reads = reads;
    }

    /// Shares the byte counter used by archive reads with another reader.
    pub fn reads(&self) -> Arc<Reads> {
        self.reads.clone()
    }

    /// Starts the count over for a pass that reads `total` bytes.
    pub fn expect_reads(&self, total: u64) {
        self.reads.done.store(0, Ordering::Relaxed);
        self.reads.total.store(total, Ordering::Relaxed);
    }
}

/// The path of the first volume, if `path` names any volume of a split set.
pub fn first_volume(path: &Path) -> PathBuf {
    if let Some(rar) = rar_volume(path) {
        return rar.first();
    }
    match split_suffix(path) {
        Some((base, width, _)) => numbered(&base, width, 1),
        None => path.to_path_buf(),
    }
}

/// The path volume `index` of the archive whose first volume is `primary`
/// has, whether or not it exists. `None` past the only volume of an archive
/// that is not split.
pub fn volume_path(primary: &Path, index: usize) -> Option<PathBuf> {
    if index == 0 {
        return Some(primary.to_path_buf());
    }
    if let Some(rar) = rar_naming(primary) {
        return rar.volume(index);
    }
    match split_suffix(primary) {
        Some((base, width, _)) => Some(numbered(&base, width, index as u64 + 1)),
        None => (index == 0).then(|| primary.to_path_buf()),
    }
}

fn volume_set(path: &Path) -> Result<Vec<PathBuf>> {
    if let Some(rar) = rar_volume(path) {
        if rar.index != 0 {
            bail!(
                "{} is not the first volume. Open {} instead",
                path.display(),
                rar.first().display()
            );
        }
        let mut volumes = vec![path.to_path_buf()];
        while let Some(next) = rar.volume(volumes.len()).filter(|p| p.exists()) {
            volumes.push(next);
        }
        return Ok(volumes);
    }
    let Some((base, width, n)) = split_suffix(path) else {
        return Ok(vec![path.to_path_buf()]);
    };
    if n != 1 {
        bail!(
            "{} is not the first part. Open {} instead",
            path.display(),
            numbered(&base, width, 1).display()
        );
    }
    let mut parts = vec![path.to_path_buf()];
    loop {
        let next = numbered(&base, width, parts.len() as u64 + 1);
        if !next.exists() {
            return Ok(parts);
        }
        parts.push(next);
    }
}

/// `game.7z.001` is `("game.7z", 3, 1)`. Only names with a real extension
/// before the number count, so a file that just happens to end in digits
/// isn't mistaken for a volume.
fn split_suffix(path: &Path) -> Option<(PathBuf, usize, u64)> {
    let name = path.file_name()?.to_str()?;
    let (stem, digits) = name.rsplit_once('.')?;
    if digits.len() < 3 || !digits.bytes().all(|b| b.is_ascii_digit()) || !stem.contains('.') {
        return None;
    }
    Some((
        path.with_file_name(stem),
        digits.len(),
        digits.parse().ok()?,
    ))
}

fn numbered(base: &Path, width: usize, n: u64) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(format!(".{n:0width$}"));
    PathBuf::from(s)
}

/// Reads the joined archive. Parts or regions that were already truncated
/// away read as zeros: format parsers scan backwards over them looking for
/// end records, and zeros never match a signature.
pub struct Reader {
    parts: Arc<[Part]>,
    open: Option<(usize, File)>,
    tail: Option<Arc<Tail>>,
    len: u64,
    pos: u64,
    reads: Arc<Reads>,
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.read_at_pos(buf)?;
        self.reads.done.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl Reader {
    fn read_at_pos(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let live_end = match &self.tail {
            Some(t) if self.pos >= t.start => {
                let at = (self.pos - t.start) as usize;
                let src = t.bytes.get(at..).unwrap_or(&[]);
                let n = src.len().min(buf.len());
                buf[..n].copy_from_slice(&src[..n]);
                self.pos += n as u64;
                return Ok(n);
            }
            Some(t) => t.start,
            None => self.len,
        };
        let i = self.parts.partition_point(|p| p.start <= self.pos) - 1;
        let part = &self.parts[i];
        let room = (part.start + part.len).min(live_end) - self.pos;
        let room = room.min(buf.len() as u64) as usize;
        if self.open.as_ref().is_none_or(|(j, _)| *j != i) {
            self.open = File::open(&part.path).ok().map(|f| (i, f));
        }
        let n = match &mut self.open {
            Some((_, f)) => {
                f.seek(SeekFrom::Start(self.pos - part.start))?;
                f.read(&mut buf[..room])?
            }
            None => 0,
        };
        let n = if n == 0 {
            buf[..room].fill(0);
            room
        } else {
            n
        };
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Reader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = pos.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad seek"))?;
        Ok(self.pos)
    }
}

/// How the volumes of a RAR set are named, and which one a path is.
struct RarSet {
    /// The path without the part of the name that changes.
    stem: PathBuf,
    style: RarStyle,
    /// 0 for the first volume.
    index: usize,
    /// Upper case extensions stay upper case.
    upper: bool,
}

enum RarStyle {
    /// `name.part1.rar`, `name.part01.rar`: the number's width is kept.
    Part { width: usize },
    /// `name.rar`, `name.r00`, `name.r01`, ..., `name.r99`, `name.s00`, ...
    Old,
}

impl RarSet {
    fn first(&self) -> PathBuf {
        self.volume(0).expect("the first volume always has a name")
    }

    /// `None` past `name.z99`, the last name the old style has.
    fn volume(&self, index: usize) -> Option<PathBuf> {
        let mut name = self.stem.as_os_str().to_owned();
        let case = |s: String| if self.upper { s.to_uppercase() } else { s };
        match self.style {
            RarStyle::Part { width } => {
                name.push(case(format!(".part{:0width$}.rar", index.checked_add(1)?)));
            }
            RarStyle::Old if index == 0 => name.push(case(".rar".into())),
            RarStyle::Old => {
                let n = index - 1;
                let letter = u8::try_from(n / 100)
                    .ok()
                    .and_then(|l| b'r'.checked_add(l))
                    .filter(|l| *l <= b'z')?;
                name.push(case(format!(".{}{:02}", letter as char, n % 100)));
            }
        }
        Some(PathBuf::from(name))
    }

    fn has_volume(&self, index: usize) -> bool {
        self.volume(index).is_some_and(|p| p.exists())
    }
}

/// Recognises a volume of a RAR set by its name. A bare `name.rar` counts
/// only if `name.r00` sits next to it, so a single RAR file is not a set.
fn rar_volume(path: &Path) -> Option<RarSet> {
    let set = rar_naming(path)?;
    if matches!(set.style, RarStyle::Old) && set.index == 0 && !set.has_volume(1) {
        return None;
    }
    Some(set)
}

/// [`rar_volume`] by the name alone: a bare `name.rar` is the first volume
/// of a set whether or not `name.r00` is there, as it may already be gone.
fn rar_naming(path: &Path) -> Option<RarSet> {
    let name = path.file_name()?.to_str()?;
    let lower = name.to_ascii_lowercase();
    let upper = name.chars().rev().take(3).all(|c| !c.is_ascii_lowercase());
    let (stem_len, ext) = lower.rsplit_once('.').map(|(s, e)| (s.len(), e))?;
    let stem = &name[..stem_len];
    if ext == "rar" {
        if let Some((before, digits)) = stem.to_ascii_lowercase().rsplit_once(".part")
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            let n: usize = digits.parse().ok()?;
            return Some(RarSet {
                stem: path.with_file_name(&name[..before.len()]),
                style: RarStyle::Part {
                    width: digits.len(),
                },
                index: n.checked_sub(1)?,
                upper,
            });
        }
        return Some(RarSet {
            stem: path.with_file_name(stem),
            style: RarStyle::Old,
            index: 0,
            upper,
        });
    }
    let bytes = ext.as_bytes();
    if bytes.len() == 3
        && (b'r'..=b'z').contains(&bytes[0])
        && bytes[1].is_ascii_digit()
        && bytes[2].is_ascii_digit()
    {
        let n = usize::from(bytes[0] - b'r') * 100
            + usize::from(bytes[1] - b'0') * 10
            + usize::from(bytes[2] - b'0');
        let set = RarSet {
            stem: path.with_file_name(stem),
            style: RarStyle::Old,
            index: n + 1,
            upper,
        };
        // Other formats number their volumes the same way (`.z01` for a
        // split ZIP), so it is a RAR volume only next to its first one.
        return set.has_volume(0).then_some(set);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The letter of `.r00` counts up to `z`; a set that needs more names is
    /// not followed past `.z99`, where the letter would leave the alphabet.
    #[test]
    fn old_style_rar_volume_names_end_at_z99() {
        let set = RarSet {
            stem: PathBuf::from("set"),
            style: RarStyle::Old,
            index: 0,
            upper: false,
        };
        let name = |i| set.volume(i).map(|p| p.to_string_lossy().into_owned());
        assert_eq!(name(0).as_deref(), Some("set.rar"));
        assert_eq!(name(1).as_deref(), Some("set.r00"));
        assert_eq!(name(101).as_deref(), Some("set.s00"));
        assert_eq!(name(900).as_deref(), Some("set.z99"));
        assert_eq!(name(901), None);
        assert_eq!(name(14_301), None);
    }

    /// What front ends show while an archive opens: every reader counts, a
    /// split set is one pass, and a pass can start part way in.
    #[test]
    fn reads_are_counted_across_volumes() {
        let dir = std::env::temp_dir().join(format!("hatchzip-reads-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("game.7z.001"), vec![1u8; 3000]).unwrap();
        fs::write(dir.join("game.7z.002"), vec![2u8; 1000]).unwrap();

        let mut storage = Storage::open(&dir.join("game.7z.001"), false).unwrap();
        let reads = Arc::new(Reads::default());
        storage.count_reads(reads.clone());
        assert_eq!(reads.fraction(), Some(0.0));

        let mut half = vec![0; 2000];
        storage.reader(None).read_exact(&mut half).unwrap();
        assert_eq!(reads.fraction(), Some(0.5));
        let mut rest = Vec::new();
        let mut r = storage.reader(None);
        r.seek(SeekFrom::Start(2000)).unwrap();
        r.read_to_end(&mut rest).unwrap();
        assert_eq!(reads.fraction(), Some(1.0));

        // A resumed pass that starts at byte 3000 has 1000 bytes to read.
        storage.expect_reads(1000);
        let mut last = vec![0; 500];
        r.seek(SeekFrom::Start(3000)).unwrap();
        r.read_exact(&mut last).unwrap();
        assert_eq!(reads.fraction(), Some(0.5));

        reads.reset();
        assert_eq!(reads.fraction(), None);
        drop(storage);
        fs::remove_dir_all(&dir).unwrap();
    }
}
