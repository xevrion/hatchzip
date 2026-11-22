//! Extracting over files that are already in the output folder.
//!
//! A resumed run deletes whatever sits at the paths of its uncommitted
//! entries, which is only safe because the run put it there. So before an
//! extraction that replaces files extracts anything, it moves every file at
//! the path of an entry into [`FOLDER`] inside the output, keeping its path.
//! A rename on the same filesystem takes no space and leaves the file whole,
//! so a restore can put it back. Only a completed extraction deletes them.
//!
//! The journal records `replace` before the first rename, and `moved` only
//! once every rename is flushed. Until then nothing is extracted, so a
//! resume deletes nothing at those paths and finishes the moves first.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::archive::Kind;
use crate::engine::{self, Job};
use crate::journal::Replace;
use crate::{fsutil, platform};

/// Where replaced files wait inside the output folder until the extraction
/// completes. An archive with an entry of this name at its root can't
/// replace files, or that entry would be extracted among them.
pub const FOLDER: &str = ".hatchzip-replaced";

/// What happens to each existing file an extraction finds at the path of
/// an entry. Every one not listed is replaced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Choices {
    /// Stay as they are. The archive's copy is extracted, then dropped.
    pub keep: Vec<PathBuf>,
    /// Stay as they are, with the archive's copy beside them under a new
    /// name, such as `report (2).pdf`.
    pub both: Vec<PathBuf>,
}

pub fn folder(out: &Path) -> PathBuf {
    out.join(FOLDER)
}

/// Files and links at the paths entries of `job` are extracted to, relative
/// to `out`. A folder at one of them is refused when the archive is opened.
pub(crate) fn found(job: &Job, out: &Path) -> Vec<PathBuf> {
    // Nothing can be under a folder that isn't there, and looking for every
    // entry below it costs a failing stat each on Windows.
    if !out.exists() {
        return Vec::new();
    }
    job.units
        .iter()
        .flat_map(|u| &u.items)
        .filter(|i| i.kind != Kind::Dir)
        .filter(|i| fs::symlink_metadata(out.join(&i.path)).is_ok_and(|m| !m.is_dir()))
        .map(|i| i.path.clone())
        .collect()
}

/// Refuses, before anything is touched, what would keep the files `found`
/// from being moved aside.
pub(crate) fn check(job: &Job, out: &Path, found: &[PathBuf]) -> Result<()> {
    let named = |p: &Path| {
        p.components()
            .next()
            .is_some_and(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(FOLDER))
    };
    if let Some(item) = job
        .units
        .iter()
        .flat_map(|u| &u.items)
        .find(|i| named(&i.path))
    {
        bail!(
            "the archive contains '{}', but HatchZip uses the name {FOLDER} for the files it \
             replaces, so existing files cannot be replaced in this extraction. Extract to an \
             empty folder instead. Nothing was modified",
            item.path.display()
        );
    }
    let aside = folder(out);
    if fs::symlink_metadata(&aside).is_ok() {
        bail!(
            "{} already exists, so existing files cannot be moved there. It may hold files \
             replaced by an earlier extraction; move its contents elsewhere, delete it, then try \
             again. Nothing was modified",
            aside.display()
        );
    }
    for rel in found {
        if platform::path_len(&aside.join(rel)) > platform::MAX_PATH_LEN {
            bail!(
                "{} cannot be replaced: moving it into {} would make its path longer than this \
                 system allows. Nothing was modified. Extract to a folder with a shorter path",
                out.join(rel).display(),
                aside.display()
            );
        }
    }
    // The folder isn't there yet, so it will take after the output, which can
    // ignore case where a folder inside doesn't. The second of a pair would
    // fail to move, after the journal is written and the first has moved.
    if let Some((a, b)) = case_pair(found)
        && !engine::keeps_case_apart(out)
    {
        bail!(
            "'{}' and '{}' differ only in case, and {} is not known to keep such names apart, so \
             they cannot be moved aside to be replaced. Nothing was modified. Extract to an empty \
             folder or move them first",
            a.display(),
            b.display(),
            out.display()
        );
    }
    Ok(())
}

/// Two of `paths`, or of the folders they are in, that differ only in case.
/// Folders count here: merged in the folder of replaced files, what a restore
/// puts back would land in the wrong one.
fn case_pair(paths: &[PathBuf]) -> Option<(&Path, &Path)> {
    let mut names: Vec<(String, &Path)> = paths
        .iter()
        .flat_map(|p| p.ancestors())
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| (crate::fsutil::fold_name(&p.to_string_lossy()), p))
        .collect();
    names.sort_unstable();
    names.dedup();
    names
        .windows(2)
        .find(|w| w[0].0 == w[1].0)
        .map(|w| (w[0].1, w[1].1))
}

/// Moves every file at the path of an entry of `job` into the folder, then
/// flushes the moves. A resume runs it again until the journal says it is
/// done; a file already moved is no longer at its path, so it moves once.
pub(crate) fn set_aside(job: &Job, out: &Path) -> Result<()> {
    let aside = folder(out);
    if let Err(e) = fs::create_dir(&aside)
        && e.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(e).with_context(|| format!("creating {}", aside.display()));
    }
    if !fs::symlink_metadata(&aside).is_ok_and(|m| m.is_dir()) {
        bail!(
            "{} is not a folder, so existing files cannot be moved there. Nothing was extracted",
            aside.display()
        );
    }
    for rel in found(job, out) {
        let from = out.join(&rel);
        let to = aside.join(&rel);
        // The open checked the way to every entry; this is the way into
        // the folder, which a crashed run left for anyone to change.
        if let Some(via) = engine::link_on_the_way(&aside, &rel) {
            bail!(
                "{} is a link, and {} would be moved through it. Nothing was extracted",
                aside.join(via).display(),
                from.display()
            );
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        if fs::symlink_metadata(&to).is_ok() {
            bail!(
                "{} cannot be moved aside: {} already exists. Nothing was extracted",
                from.display(),
                to.display()
            );
        }
        // A rename moves a link itself, never what it points to.
        fs::rename(&from, &to)
            .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        engine::crash_point("set-aside");
    }
    let rels: Vec<&Path> = job
        .units
        .iter()
        .flat_map(|u| &u.items)
        .filter(|i| i.kind != Kind::Dir)
        .map(|i| i.path.as_path())
        .collect();
    flush(out, &rels)
}

/// Moves the files set aside back to their paths, for a restore. A file
/// whose path is taken stays in the folder, unless `over`: in place, what
/// was extracted there may be the only copy of that entry. Returns how many
/// stay; the folder is deleted when none do.
pub(crate) fn put_back(out: &Path, over: bool) -> Result<usize> {
    let aside = folder(out);
    if !fs::symlink_metadata(&aside).is_ok_and(|m| m.is_dir()) {
        return Ok(0);
    }
    let mut files = Vec::new();
    walk(&aside, &aside, &mut files)?;
    let mut stay = 0;
    for rel in &files {
        let from = aside.join(rel);
        let to = out.join(rel);
        if engine::link_on_the_way(out, rel).is_some() {
            stay += 1;
            continue;
        }
        match fs::symlink_metadata(&to) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
            }
            Ok(m) if over && !m.is_dir() => {}
            _ => {
                stay += 1;
                continue;
            }
        }
        fs::rename(&from, &to)
            .with_context(|| format!("moving {} back to {}", from.display(), to.display()))?;
    }
    let rels: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
    flush(out, &rels)?;
    if stay == 0 {
        remove(out)?;
    }
    Ok(stay)
}

/// Once every entry is extracted, puts back the existing files the person
/// chose to keep: over what was extracted at their paths, or beside it with
/// the extracted copy renamed. Each step is a rename a rerun finds done or
/// not, so after a crash this runs again until it completes. Nothing is
/// deleted here; whatever can't go back stays in the folder with an error,
/// before [`remove`] could take it.
pub(crate) fn settle(out: &Path, r: &Replace) -> Result<()> {
    let aside = folder(out);
    let chosen = r.keep.iter().map(|p| (p, false));
    let mut done: Vec<&Path> = Vec::new();
    for (rel, both) in chosen.chain(r.both.iter().map(|p| (p, true))) {
        let from = aside.join(rel);
        if fs::symlink_metadata(&from).is_err() {
            // Put back by an earlier run that stopped before finishing.
            continue;
        }
        let to = out.join(rel);
        if let Some(via) = engine::link_on_the_way(out, rel) {
            bail!(
                "{} could not be put back: {} is a link. It is kept in {}. Remove the link, then \
                 resume the extraction to finish",
                to.display(),
                out.join(via).display(),
                aside.display()
            );
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        if both && fs::symlink_metadata(&to).is_ok() {
            let beside = free_name(&to);
            fs::rename(&to, &beside)
                .with_context(|| format!("moving {} to {}", to.display(), beside.display()))?;
        }
        // A rename over the extracted file replaces it in one step.
        fs::rename(&from, &to)
            .with_context(|| format!("moving {} back to {}", from.display(), to.display()))?;
        engine::crash_point("settle");
        done.push(rel);
    }
    flush(out, &done)
}

/// `report (2).pdf`, or the first such name not taken, beside `path`.
fn free_name(path: &Path) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default();
    (2u32..)
        .map(|n| {
            let mut name = stem.to_os_string();
            name.push(format!(" ({n})"));
            if let Some(ext) = path.extension() {
                name.push(".");
                name.push(ext);
            }
            path.with_file_name(name)
        })
        .find(|p| fs::symlink_metadata(p).is_err())
        .unwrap_or_else(|| path.to_path_buf())
}

/// Deletes the files set aside, once the extraction is complete.
pub(crate) fn remove(out: &Path) -> Result<()> {
    let aside = folder(out);
    // Never follows a link: one in the folder is removed, not its target.
    match fs::remove_dir_all(&aside) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", aside.display()))
        }
        _ => Ok(()),
    }
}

/// The files and links under `dir`, relative to `root`.
fn walk(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if fs::symlink_metadata(&path)?.is_dir() {
            walk(root, &path, files)?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            files.push(rel.to_path_buf());
        }
    }
    Ok(())
}

/// Makes the renames of `rels` between the output and the folder durable.
/// The journal is about to say they happened, and a resume that believed it
/// while a power cut had undone one would delete a file of the person's.
fn flush(out: &Path, rels: &[&Path]) -> Result<()> {
    platform::sync_batch(out).context("flushing moved files to disk")?;
    if !platform::SYNC_EACH_FILE {
        return Ok(());
    }
    let aside = folder(out);
    let mut dirs: HashSet<PathBuf> = HashSet::from([out.to_path_buf(), aside.clone()]);
    for rel in rels {
        for parent in rel.ancestors().skip(1) {
            dirs.insert(out.join(parent));
            dirs.insert(aside.join(parent));
        }
    }
    for d in dirs {
        if fs::symlink_metadata(&d).is_ok_and(|m| m.is_dir()) {
            fsutil::sync_dir(&d)?;
        }
    }
    Ok(())
}
