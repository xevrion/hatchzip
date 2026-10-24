//! Opening an archive for extraction and working out whether it fits. Shared
//! by the command line and the GUI, so the two never disagree.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::archive::storage;
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
/// output folder may already hold other files, but nothing in the archive may
/// land on top of one. `password` only matters for encrypted archives; see
/// [`open_encrypted`] for the ask-and-retry loop front ends want.
pub fn open(
    primary: &Path,
    paths: &Paths,
    output: Option<PathBuf>,
    writable: bool,
    password: Option<&str>,
) -> Result<State> {
    if paths.exists() {
        let (mut journal, tail) = Journal::load(paths)?;
        if let Some(o) = &output
            && std::path::absolute(o)? != journal.output
        {
            bail!(
                "this archive is already partially extracted to {}. Complete or restore that extraction first",
                journal.output.display()
            );
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
            return Ok(State::Orphan(journal));
        }
        let job = Job::resume(&journal, tail, writable, password)?;
        return Ok(State::Resume(job, journal));
    }
    let job = Job::open(primary, writable, password)?;
    let output = match output {
        Some(o) => std::path::absolute(o)?,
        None => default_output(primary),
    };
    if writable {
        check_collisions(&job, &output)?;
    }
    Ok(State::Fresh(job, output))
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
    mut password: Option<String>,
    mut ask: impl FnMut(bool) -> Option<String>,
) -> Result<(State, Option<String>)> {
    let mut wrong = false;
    loop {
        let attempt = open(
            primary,
            paths,
            output.clone(),
            writable,
            password.as_deref(),
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

/// HatchZip never overwrites a file. That is also what lets a crashed run
/// delete whatever sits at the paths of its uncommitted entries: before the
/// run started, nothing did.
fn check_collisions(job: &Job, out: &Path) -> Result<()> {
    if !out.exists() {
        return Ok(());
    }
    let mut checked = std::collections::HashSet::new();
    for item in job.units.iter().flat_map(|u| &u.items) {
        let dest = out.join(&item.path);
        if let Ok(meta) = fs::symlink_metadata(&dest)
            && !(meta.is_dir() && item.kind == crate::archive::Kind::Dir)
        {
            bail!(
                "{} already exists. HatchZip does not overwrite existing files. \
                 Move the file or choose another destination",
                dest.display()
            );
        }
        // A file where the archive needs a folder would only fail halfway.
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
    for ext in [".zip", ".7z", ".tar", ".tar.gz", ".tgz", ".rar", ".exe"] {
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

/// Whether an extraction fits, in the numbers a person needs to decide.
pub struct SpacePlan {
    /// Usable free space, counting files a crashed run left that will be
    /// deleted before continuing.
    pub free: u64,
    /// Free space HatchZip needs at its busiest moment.
    pub needed: u64,
    pub keep_free: u64,
    pub fits: bool,
    /// Free space once finished, if it fits.
    pub free_after: u64,
    /// What a normal extractor would need free.
    pub normal_needs: u64,
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

pub fn plan(job: &Job, out: &Path, resuming: bool, keep_free: u64) -> Result<SpacePlan> {
    let reclaims = fsutil::same_filesystem(job.storage.primary(), out);
    let stale: u64 =
        job.uncommitted(out).iter().map(|(_, n)| n).sum::<u64>() + job.past_resume_point(out);
    let free = fsutil::available_space(out)? + stale;
    let (needed, end) = job.space_needed(!resuming, reclaims);
    let fits = free >= needed + keep_free;
    Ok(SpacePlan {
        free,
        needed,
        keep_free,
        fits,
        free_after: (free as i128 - end).max(0) as u64,
        normal_needs: job.remaining_size(),
        reclaims,
        solid_block_bound: !fits
            && !job.is_stream()
            && job
                .units
                .iter()
                .any(|u| u.items.len() > 1 && u.size() > free),
        archive_kept_whole: job.kept_whole(reclaims),
    })
}

/// Commits to a fresh extraction: saves the archive's tail and writes the
/// journal. From here on the archive may be truncated.
pub fn begin(job: &Job, paths: &Paths, output: PathBuf) -> Result<Journal> {
    let plan = Plan {
        format: job.format_name,
        tail_start: job.tail_start,
        output,
        total_size: job.remaining_size(),
    };
    Journal::create(paths, &job.storage, plan)
}

/// Before continuing a crashed run, deletes what it wrote for batches it
/// never committed.
pub fn clear_uncommitted(job: &Job, out: &Path) -> Result<()> {
    for (p, _) in job.uncommitted(out) {
        fs::remove_file(&p).with_context(|| format!("removing stale {}", p.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_folder_drops_the_volume_and_format_suffixes() {
        let name = |n: &str| default_output(Path::new(n));
        assert_eq!(name("game.zip"), Path::new("game"));
        assert_eq!(name("game.7z.001"), Path::new("game"));
        assert_eq!(name("game.part1.rar"), Path::new("game"));
        assert_eq!(name("game.PART02.RAR"), Path::new("game"));
        assert_eq!(name("game.rar"), Path::new("game"));
        assert_eq!(name("game.part.rar"), Path::new("game.part"));
        assert_eq!(name("notes.txt"), Path::new("notes.txt-extracted"));
    }
}
