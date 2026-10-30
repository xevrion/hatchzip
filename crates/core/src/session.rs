//! Opening an archive for extraction and working out whether it fits. Shared
//! by the command line and the GUI, so the two never disagree.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::archive::storage::{self, Reads, Storage};
use crate::engine::Job;
use crate::fsutil;
use crate::journal::{Journal, Paths, Plan};

/// The first volume of the archive and where its journal lives. Paths are
/// made absolute so a resume works from any directory.
pub fn locate(archive: &Path) -> Result<(PathBuf, Paths)> {
    let primary = std::path::absolute(storage::first_volume(archive))
        .with_context(|| format!("resolving {}", archive.display()))?;
    let paths = Paths::for_archive(&primary);
    Ok((primary, paths))
}

pub enum State {
    /// Nothing touched yet. Nothing changes on disk until [`begin`] writes
    /// the journal.
    Fresh(Job, PathBuf),
    Resume(Job, Journal),
    /// The archive is gone but the run died before cleaning up.
    Orphan(Journal),
}

/// `writable` is false for anything that only looks, so it works on
/// read-only media and never takes the lock. For a fresh extraction the
/// output folder may already hold other files, but no folder where the
/// archive has a file or the other way round; files the archive would
/// replace are listed by [`existing`]. `password` only matters for encrypted archives; see
/// [`open_encrypted`] for the ask-and-retry loop front ends want. What
/// opening reads is counted in `reads`, which a front end can show from
/// another thread: a compressed tar is read in full here.
pub fn open(
    primary: &Path,
    paths: &Paths,
    output: Option<PathBuf>,
    writable: bool,
    password: Option<&str>,
    reads: &Arc<Reads>,
) -> Result<State> {
    if paths.exists() {
        let (mut journal, tail) = Journal::load(paths)?;
        // Only a writable open deletes or writes anything in the folder.
        if writable || output.is_some() {
            check_output(&journal, primary, output.as_deref(), "extract")?;
        }
        if journal.restoring.is_some() {
            if !writable {
                bail!(
                    "a restore of this archive was interrupted. Run restore again to complete \
                     it, or run extract to resume the extraction"
                );
            }
            crate::restore::recover(&mut journal, paths)?;
        }
        if !journal.parts[0].0.exists() {
            if journal.keep {
                // Only the person could have removed an archive this run
                // promised to keep, and the rest of it is still to extract.
                bail!(
                    "{} was moved or deleted during an extraction that keeps it. Put it back \
                     to resume, or delete {} to discard the progress",
                    journal.parts[0].0.display(),
                    paths.journal.display()
                );
            }
            return Ok(State::Orphan(journal));
        }
        // The final commit was journaled: every unit is extracted, and only
        // the archive's removal and the links are left. Nothing is read from
        // it again; a compressed stream's start was punched away by then.
        if journal.done {
            return Ok(State::Orphan(journal));
        }
        let job = Job::resume(&journal, tail, writable, password, reads.clone())?;
        check_no_links_on_the_way(&job, &journal.output)?;
        return Ok(State::Resume(job, journal));
    }
    let job = Job::open(primary, writable, password, reads.clone())?;
    let output = match output {
        Some(o) => std::path::absolute(o)?,
        None => {
            let output = default_output(primary);
            refuse_linked_default(&output)?;
            output
        }
    };
    check_path_lengths(&job, &output)?;
    if writable {
        check_no_links_on_the_way(&job, &output)?;
        check_collisions(&job, &output)?;
    }
    Ok(State::Fresh(job, output))
}

/// The journal lies next to the archive, where anyone who supplied the
/// archive could have put one, so it is used only for an extraction this
/// computer recorded starting into that folder (see [`crate::chosen`]), or
/// when the person names the folder again (`given`). A resume deletes what
/// it finds at the paths of the entries left, so a planted journal could
/// otherwise have it delete and write files anywhere, and even in the
/// default folder it would replace the person's own files there.
/// `command` is the one to run again, with `-o`.
pub fn check_output(
    journal: &Journal,
    primary: &Path,
    given: Option<&Path>,
    command: &str,
) -> Result<()> {
    let default = default_output(primary);
    let in_default = fsutil::same_path(&default, &journal.output);
    match given {
        Some(o) => {
            if !fsutil::same_path(&std::path::absolute(o)?, &journal.output) {
                bail!(
                    "this archive is already partially extracted to {}. Complete or restore that \
                     extraction first",
                    journal.output.display()
                );
            }
        }
        None if crate::chosen::chose(primary, &journal.output) => {
            if in_default {
                refuse_linked_default(&default)?;
            }
        }
        // Releases before 0.1.6 did not record the default folder.
        None if in_default => bail!(
            "{} was partially extracted to {}, but this computer has no record of starting that \
             extraction. If you started it, continue it by naming the folder again: hatchzip \
             {command} \"{}\" -o \"{}\". Nothing was modified",
            primary.display(),
            journal.output.display(),
            primary.display(),
            journal.output.display()
        ),
        None => bail!(
            "{} was partially extracted to {}, a folder not chosen on this computer. If you \
             started that extraction, continue it by naming the folder again: hatchzip {command} \
             \"{}\" -o \"{}\". Nothing was modified",
            primary.display(),
            journal.output.display(),
            primary.display(),
            journal.output.display()
        ),
    }
    Ok(())
}

/// A link at the default output folder, next to the archive, would send the
/// extraction wherever it points. A folder the person names is theirs to
/// choose.
fn refuse_linked_default(output: &Path) -> Result<()> {
    if fs::symlink_metadata(output).is_ok_and(|m| m.is_symlink()) {
        bail!(
            "{} is a link, so files extracted there would land somewhere else. Remove the link, \
             or choose the destination folder. Nothing was modified",
            output.display()
        );
    }
    Ok(())
}

/// A resume deletes and writes the files of the entries left, and its own
/// links are only made once every entry is done. A link on the way to one of
/// those files was put there by someone else, and following it would leave
/// the output folder.
pub(crate) fn check_no_links_on_the_way(job: &Job, out: &Path) -> Result<()> {
    let mut checked = std::collections::HashSet::new();
    for item in job.units.iter().flat_map(|u| &u.items) {
        for parent in item
            .path
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
        {
            if !checked.insert(parent.to_path_buf()) {
                break;
            }
            if fs::symlink_metadata(out.join(parent)).is_ok_and(|m| m.is_symlink()) {
                bail!(
                    "{} is a link, and '{}' would be extracted through it. Nothing was modified",
                    out.join(parent).display(),
                    item.path.display()
                );
            }
        }
    }
    Ok(())
}

/// A path the system can't create would only fail halfway through a run,
/// after part of the archive is gone; macOS stops at 1024 bytes.
fn check_path_lengths(job: &Job, out: &Path) -> Result<()> {
    for item in job.units.iter().flat_map(|u| &u.items) {
        if crate::platform::path_len(&out.join(&item.path)) > crate::platform::MAX_PATH_LEN {
            bail!(
                "'{}' cannot be created in {}: the full path would be longer than this system \
                 allows. The archive was not modified. Extract to a folder with a shorter path",
                item.path.display(),
                out.display()
            );
        }
    }
    Ok(())
}

/// Opens an archive that may be encrypted. `ask` is called whenever a
/// password is needed or the last one was wrong, and returns `None` to give
/// up. Archives with a password are proven with [`Job::check_password`]
/// before this returns, so nothing is deleted on a wrong guess. Returns the
/// state and the password that worked.
pub fn open_encrypted(
    primary: &Path,
    paths: &Paths,
    output: Option<PathBuf>,
    writable: bool,
    password: Option<String>,
    reads: &Arc<Reads>,
    ask: impl FnMut(bool) -> Option<String>,
) -> Result<(State, Option<String>)> {
    open_encrypted_noting(
        primary,
        paths,
        output,
        writable,
        password,
        reads,
        ask,
        |_, _| {},
    )
}

/// [`open_encrypted`], calling `slow` with a file and its size before a
/// password check that decodes all of it (see [`Job::slow_password_check`]),
/// so a front end can say it may take a while.
#[allow(clippy::too_many_arguments)]
pub fn open_encrypted_noting(
    primary: &Path,
    paths: &Paths,
    output: Option<PathBuf>,
    writable: bool,
    mut password: Option<String>,
    reads: &Arc<Reads>,
    mut ask: impl FnMut(bool) -> Option<String>,
    mut slow: impl FnMut(&Path, u64),
) -> Result<(State, Option<String>)> {
    let mut wrong = false;
    loop {
        let attempt = open(
            primary,
            paths,
            output.clone(),
            writable,
            password.as_deref(),
            reads,
        );
        let failure = match attempt {
            Ok(State::Orphan(j)) => return Ok((State::Orphan(j), password)),
            Ok(state) => {
                let job = match &state {
                    State::Fresh(job, _) | State::Resume(job, _) => job,
                    State::Orphan(_) => unreachable!(),
                };
                if !job.encrypted() {
                    return Ok((state, password));
                }
                if password.is_some()
                    && let Some((file, size)) = job.slow_password_check()
                {
                    slow(file, size);
                }
                match job.check_password() {
                    Ok(()) => return Ok((state, password)),
                    Err(e) => e,
                }
            }
            Err(e) => e,
        };
        if failure.is::<crate::archive::WrongPassword>() {
            wrong = true;
        } else if !failure.is::<crate::archive::PasswordNeeded>() {
            return Err(failure);
        }
        // Drop the open archive before asking, so nothing stays locked while
        // a person types.
        match ask(wrong) {
            Some(p) => password = Some(p),
            None => return Err(failure),
        }
    }
}

/// Refuses what no extraction into `out` can get past: a folder where the
/// archive has a file, which is never deleted or replaced, a file where it
/// needs a folder, which would only fail halfway, and anything at the name a
/// file is first written under. That last one would be in the way of the
/// file, or would be deleted as the run's own by a resume.
fn check_collisions(job: &Job, out: &Path) -> Result<()> {
    if !out.exists() {
        return Ok(());
    }
    let mut checked = std::collections::HashSet::new();
    for item in job.units.iter().flat_map(|u| &u.items) {
        let dest = out.join(&item.path);
        let part = crate::engine::part_path(&dest);
        if item.kind == crate::archive::Kind::File && fs::symlink_metadata(&part).is_ok() {
            bail!(
                "{} already exists. HatchZip first writes '{}' under that name and does not \
                 replace a file it did not create. Move it or choose another destination. Nothing \
                 was modified",
                part.display(),
                item.path.display()
            );
        }
        if let Ok(meta) = fs::symlink_metadata(&dest) {
            let dir = item.kind == crate::archive::Kind::Dir;
            if dir && !meta.is_dir() {
                bail!(
                    "{} is a file, but the archive requires a folder at that location",
                    dest.display()
                );
            }
            if !dir && meta.is_dir() {
                bail!(
                    "{} is a folder, but the archive has a file at that location. Move the \
                     folder or choose another destination",
                    dest.display()
                );
            }
        }
        for parent in item
            .path
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
        {
            if !checked.insert(parent.to_path_buf()) {
                break;
            }
            if let Ok(meta) = fs::symlink_metadata(out.join(parent))
                && !meta.is_dir()
            {
                bail!(
                    "{} is a file, but the archive requires a folder at that location",
                    out.join(parent).display()
                );
            }
        }
    }
    Ok(())
}

/// The files and links already in `out` at paths the archive extracts to,
/// relative to `out`. A fresh extraction may only replace them when asked
/// to; see [`begin`]. When there are any, what would keep them from being
/// replaced is refused here, before a front end asks.
pub fn existing(job: &Job, out: &Path) -> Result<Vec<PathBuf>> {
    let found = crate::replace::found(job, out);
    if !found.is_empty() {
        crate::replace::check(job, out, &found)?;
    }
    Ok(found)
}

/// Whether everything in the archive sits under one name at its top, one
/// folder or a single file. "Extract here" puts such an archive next to
/// itself; anything else would scatter loose files, so it gets a folder.
pub fn single_root(job: &Job) -> bool {
    let mut roots = job
        .units
        .iter()
        .flat_map(|u| &u.items)
        .filter_map(|i| i.path.components().next());
    let Some(first) = roots.next() else {
        return false;
    };
    roots.all(|r| r == first)
}

/// Moves a fresh extraction opened writable by [`open`] to another folder,
/// checking it as [`open`] checks the one it was given.
pub fn retarget(job: &Job, out: &Path) -> Result<PathBuf> {
    let out = std::path::absolute(out)?;
    check_path_lengths(job, &out)?;
    check_no_links_on_the_way(job, &out)?;
    check_collisions(job, &out)?;
    Ok(out)
}

/// `game.zip`, `game.7z.001`, `game.tar.gz` and `game.part1.rar` all extract
/// into `game`, next
/// to them.
pub fn default_output(primary: &Path) -> PathBuf {
    let file_name = primary
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut name = file_name.clone();
    if let Some((stem, digits)) = name.rsplit_once('.')
        && digits.len() >= 3
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        name = stem.to_string();
    }
    let lower = name.to_ascii_lowercase();
    // `game.part2.rar` and `game.part02.rar` are volumes of `game`.
    if let Some(stem) = lower.strip_suffix(".rar")
        && let Some((_, digits)) = stem.rsplit_once(".part")
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        name.truncate(stem.len() - digits.len() - ".part".len());
        return primary.with_file_name(name);
    }
    for ext in [
        ".zip", ".7z", ".tar.xz", ".tar", ".tar.gz", ".tgz", ".rar", ".exe",
    ] {
        if lower.ends_with(ext) && name.len() > ext.len() {
            name.truncate(name.len() - ext.len());
            break;
        }
    }
    if name == file_name {
        name.push_str("-extracted");
    }
    primary.with_file_name(name)
}

/// `game.zip`, or `game.7z.001 (+4 more parts)` for a split archive.
pub fn display_name(job: &Job) -> String {
    let parts = job.storage.parts();
    let first = parts[0]
        .path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    match parts.len() {
        1 => first,
        n => format!("{first} (+{} more parts)", n - 1),
    }
}

/// Whether the extraction of a half-extracted archive keeps it, given what
/// the person asked for now. The journal decides: an extraction that keeps
/// the archive goes on keeping it, and one that has already removed part of
/// it can't start keeping it.
pub fn keeps_archive(journal: &Journal, asked: bool) -> Result<bool> {
    if asked && !journal.keep {
        bail!(
            "this archive is partially extracted in place, so part of it is already removed and \
             it cannot be kept. Resume without --keep-archive, or run restore first"
        );
    }
    Ok(journal.keep)
}

/// Whether an extraction fits, in the numbers a person needs to decide.
pub struct SpacePlan {
    /// Usable free space, counting files a crashed run left that will be
    /// deleted before continuing.
    pub free: u64,
    /// Free space the extraction needs at its busiest moment, in the mode
    /// asked for.
    pub needed: u64,
    pub keep_free: u64,
    pub fits: bool,
    /// Free space once finished, if it fits.
    pub free_after: u64,
    /// The plan is for an extraction that keeps the archive.
    pub keep_archive: bool,
    /// What an extraction that keeps the archive needs free: every
    /// extracted file, as for any other extractor, plus the saved index.
    pub normal_needs: u64,
    /// What extracting in place needs free. Equal to `needed` unless the
    /// archive is kept.
    pub in_place_needs: u64,
    /// Nothing is extracted yet, so either mode can still be chosen. A
    /// half-extracted archive continues in the mode it started in.
    pub either: bool,
    /// False when the output is on another disk, so freeing the archive
    /// doesn't help it.
    pub reclaims: bool,
    /// The shortfall comes from one solid block that can only be freed once
    /// all of it is out.
    pub solid_block_bound: bool,
    /// The archive is one compressed stream (a `.tar.gz`) on a filesystem
    /// that can't free space inside a file, such as FAT32 or exFAT, so it
    /// stays whole until the extraction completes: `needed` is room for
    /// every extracted file next to it.
    pub archive_kept_whole: bool,
}

impl SpacePlan {
    /// Whether an extraction that keeps the archive would fit too.
    pub fn normal_fits(&self) -> bool {
        self.free >= self.normal_needs + self.keep_free
    }

    pub fn in_place_fits(&self) -> bool {
        self.free >= self.in_place_needs + self.keep_free
    }
}

/// The space plan for extracting `job` into `out`, keeping the archive or
/// not. For a half-extracted archive, `keep_archive` must be what its
/// journal says; see [`keeps_archive`].
pub fn plan(
    job: &Job,
    out: &Path,
    resuming: bool,
    keep_free: u64,
    keep_archive: bool,
) -> Result<SpacePlan> {
    let reclaims = fsutil::same_filesystem(job.storage.primary(), out);
    let stale: u64 =
        job.uncommitted(out).iter().map(|(_, n)| n).sum::<u64>() + job.past_resume_point(out);
    let free = fsutil::available_space(out)? + stale;
    // Keeping the archive frees nothing, as if the output were on another
    // disk: the replay counts every file and gives nothing back.
    let (normal_needs, normal_end) = job.space_needed(!resuming, false);
    let (in_place_needs, in_place_end) = if keep_archive && resuming {
        (normal_needs, normal_end)
    } else {
        job.space_needed(!resuming, reclaims)
    };
    // The saved index is deleted at the end, when the archive is kept too.
    let tail = job.storage.original_len().saturating_sub(job.tail_start) as i128;
    let normal_end = normal_end - if reclaims { tail } else { 0 };
    let (needed, end) = if keep_archive {
        (normal_needs, normal_end)
    } else {
        (in_place_needs, in_place_end)
    };
    let fits = free >= needed + keep_free;
    Ok(SpacePlan {
        free,
        needed,
        keep_free,
        fits,
        free_after: (free as i128 - end).max(0) as u64,
        keep_archive,
        normal_needs,
        in_place_needs,
        either: !resuming,
        reclaims,
        solid_block_bound: !fits
            && !keep_archive
            && !job.is_stream()
            && job
                .units
                .iter()
                .any(|u| u.items.len() > 1 && u.size() > free),
        archive_kept_whole: !keep_archive && job.kept_whole(reclaims),
    })
}

/// Commits to a fresh extraction: saves the archive's tail and writes the
/// journal. From here on the archive may be truncated, unless `keep_archive`
/// is set, in which case nothing ever changes it. Files in the way (see
/// [`existing`]) are refused unless `replace` is given; then the run moves
/// them aside before it extracts anything, and once it is complete deletes
/// them or puts back the ones the choices keep.
pub fn begin(
    job: &Job,
    paths: &Paths,
    output: PathBuf,
    keep_archive: bool,
    replace: Option<&crate::replace::Choices>,
) -> Result<Journal> {
    // Front ends ask earlier; the folder may have changed since.
    let found = existing(job, &output)?;
    if let Some(first) = found.first()
        && replace.is_none()
    {
        let what = match found.len() {
            1 => format!("{} already exists", output.join(first).display()),
            n => format!(
                "{} and {} other {} already exist",
                output.join(first).display(),
                n - 1,
                if n == 2 { "file" } else { "files" }
            ),
        };
        bail!(
            "{what}. Existing files are only replaced when that is requested. Move the files or \
             choose another destination. Nothing was modified"
        );
    }
    if keep_archive {
        job.storage.protect();
    } else if let Some(why) = job.in_place_refusal() {
        // Front ends say this earlier; this is the last line.
        bail!("{why}. It can be extracted while keeping the archive");
    }
    crate::sfv::check_volumes(&job.storage)?;
    // The default folder too: a resume trusts the journal only when this
    // computer recorded starting the extraction (see [`check_output`]).
    crate::chosen::remember(&paths.archive, &output);
    // A choice for a file that has gone since it was made has nothing to
    // keep.
    let only_found = |paths: &[PathBuf]| -> Vec<PathBuf> {
        paths
            .iter()
            .filter(|p| found.contains(p))
            .cloned()
            .collect()
    };
    let replace = replace
        .filter(|_| !found.is_empty())
        .map(|c| crate::journal::Replace {
            moved: false,
            keep: only_found(&c.keep),
            both: only_found(&c.both),
        });
    let plan = Plan {
        format: job.format_name,
        tail_start: job.tail_start,
        output,
        total_size: job.remaining_size(),
        keep: keep_archive,
        replace,
    };
    Journal::create(paths, &job.storage, plan)
}

/// Checks the archive's volumes against an `.sfv` file next to it, if present.
pub fn check_sfv(storage: &Storage, cancelled: &dyn Fn() -> bool) -> Result<Option<PathBuf>> {
    crate::sfv::check_volumes_cancellable(storage, cancelled)
}

/// Before continuing a crashed run, deletes what it wrote for batches it
/// never committed.
pub fn clear_uncommitted(job: &Job, out: &Path) -> Result<()> {
    check_no_links_on_the_way(job, out)?;
    for (p, _) in job.uncommitted(out) {
        fs::remove_file(&p).with_context(|| format!("removing stale {}", p.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use ::zip::write::{SimpleFileOptions, ZipWriter};

    use super::*;

    #[test]
    fn output_folder_drops_the_volume_and_format_suffixes() {
        let name = |n: &str| default_output(Path::new(n));
        assert_eq!(name("game.zip"), Path::new("game"));
        assert_eq!(name("game.7z.001"), Path::new("game"));
        assert_eq!(name("game.part1.rar"), Path::new("game"));
        assert_eq!(name("game.PART02.RAR"), Path::new("game"));
        assert_eq!(name("game.rar"), Path::new("game"));
        assert_eq!(name("game.tar.xz"), Path::new("game"));
        assert_eq!(name("game.part.rar"), Path::new("game.part"));
        assert_eq!(name("notes.txt"), Path::new("notes.txt-extracted"));
    }

    /// Eight stored entries, so extracting in place frees the archive entry
    /// by entry and needs far less than keeping it.
    fn sample_zip(dir: &Path) -> PathBuf {
        let path = dir.join("plan.zip");
        let mut w = ZipWriter::new(fs::File::create(&path).unwrap());
        let opts =
            SimpleFileOptions::default().compression_method(::zip::CompressionMethod::Stored);
        for i in 0..8u8 {
            w.start_file(format!("data/{i}.bin"), opts).unwrap();
            w.write_all(&vec![i; 200_000 + usize::from(i)]).unwrap();
        }
        w.finish().unwrap();
        path
    }

    #[test]
    fn keeping_the_archive_plans_for_every_file() {
        let dir = std::env::temp_dir().join(format!("hatchzip-plan-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let archive = sample_zip(&dir);
        let out = dir.join("out");
        let job = Job::open(&archive, false, None, Default::default()).unwrap();
        let files: u64 = job
            .units
            .iter()
            .flat_map(|u| &u.items)
            .map(|i| i.size.next_multiple_of(4096))
            .sum();
        let tail = job.storage.original_len() - job.tail_start;

        let keep = plan(&job, &out, false, 0, true).unwrap();
        assert!(keep.keep_archive && keep.either);
        // Every file, plus the copy of the index a resume would need.
        assert_eq!(keep.needed, files + tail);
        assert_eq!(keep.normal_needs, keep.needed);
        assert!(!keep.solid_block_bound && !keep.archive_kept_whole);
        // Nothing of the archive comes back; the saved index does.
        assert_eq!(keep.free.saturating_sub(keep.free_after), files);

        let in_place = plan(&job, &out, false, 0, false).unwrap();
        assert!(!in_place.keep_archive);
        assert_eq!(in_place.normal_needs, keep.needed);
        assert_eq!(in_place.in_place_needs, in_place.needed);
        assert_eq!(keep.in_place_needs, in_place.needed);
        assert!(
            in_place.needed < keep.needed / 2,
            "in place {} vs kept {}",
            in_place.needed,
            keep.needed
        );
        drop(job);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Once an extraction that keeps the archive has begun, nothing can
    /// change it, whatever the caller does.
    #[test]
    fn a_kept_archive_refuses_every_change() {
        let dir = std::env::temp_dir().join(format!("hatchzip-protect-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let archive = sample_zip(&dir);
        let before = fs::read(&archive).unwrap();
        let (primary, paths) = locate(&archive).unwrap();
        let reads = Arc::default();
        let State::Fresh(job, out) = open(&primary, &paths, None, true, None, &reads).unwrap()
        else {
            panic!("a new archive opens fresh");
        };
        let journal = begin(&job, &paths, out, true, None).unwrap();
        assert!(journal.keep);
        let len = job.storage.original_len();
        assert!(job.storage.set_len(len / 2).is_err());
        assert!(job.storage.punch(0, len / 2).is_err());
        assert!(job.storage.write_at(0, b"PK").is_err());
        assert!(job.storage.cut_to(0).is_err());
        drop(job);
        assert_eq!(fs::read(&archive).unwrap(), before);

        // The journal says so, and a resumed job is protected as well.
        let (journal, tail) = Journal::load(&paths).unwrap();
        let job = Job::resume(&journal, tail, true, None, Default::default()).unwrap();
        assert!(job.storage.set_len(0).is_err());
        assert_eq!(job.units.len(), 8, "nothing is committed yet");
        drop(job);
        assert_eq!(fs::read(&archive).unwrap(), before);
        assert!(keeps_archive(&journal, false).unwrap());
        assert!(keeps_archive(&journal, true).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }
}
