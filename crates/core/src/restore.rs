//! Turning a half-extracted archive back into a normal one.
//!
//! After an interrupted extraction the archive is a prefix of the original
//! that holds exactly the entries still to do, and its index lives in the
//! saved tail. Restore appends a new index for those entries, so any tool
//! opens the archive again. Nothing below `committed_len` moves, which is why
//! it needs no more free space than the new index takes.
//!
//! The order, and why each step is safe to interrupt:
//!
//! 1. Undo any earlier restore that was interrupted (see [`recover`]), and
//!    finish a pending truncation, so the archive is exactly what extraction
//!    left: `committed_len` bytes.
//! 2. Build the new index in memory. Anything restore refuses, it refuses
//!    here, before writing anything.
//! 3. Delete output files of batches that were never committed, as a resume
//!    does: those entries are still in the archive. Make the links the
//!    journal holds for entries already cut off.
//! 4. Save the journal with `restoring` set to the bytes step 6 overwrites.
//!    From here on, any HatchZip run that finds the journal first cuts the
//!    archive back to `committed_len` and puts those bytes back.
//! 5. Append the new index at `committed_len` and flush it.
//! 6. Overwrite what has to change below it (only the 7z start header) and
//!    flush it.
//! 7. Open the result as a fresh archive with the same parsers extraction
//!    uses, and check it lists exactly the entries left. If not, undo.
//! 8. If the extraction replaced files, move them back to the paths step 3
//!    cleared, where nothing extracted holds them.
//! 9. Delete the journal, then the saved tail. A journal without its tail
//!    can't be loaded, so the journal goes first; a tail left behind by a
//!    crash in between is never read again.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::archive::storage::{Storage, Tail};
use crate::archive::{self, Kind};
use crate::engine::{self, Job};
use crate::fsutil;
use crate::journal::{Journal, Link, Paths};
use crate::{replace, session};

pub struct Restored {
    /// The first volume of the restored archive.
    pub archive: PathBuf,
    pub format: &'static str,
    /// Entries the restored archive holds: the ones not yet extracted.
    pub entries: usize,
    /// Where the extracted entries stay.
    pub output: PathBuf,
    /// The extraction kept the archive, so it was never changed and only
    /// the progress was discarded. The archive still holds every entry.
    pub kept: bool,
    /// Files the extraction replaced that stay in the replaced files folder
    /// (see [`crate::replace`]): the file extracted at their path is no
    /// longer in the archive, so it was not removed to make room.
    pub set_aside: usize,
}

/// Restores the half-extracted archive `archive` names (any volume of a
/// split set works). `output` is the folder the extraction was started
/// with, needed only when that was not the default one (see
/// [`session::check_output`]).
pub fn restore(archive: &Path, output: Option<&Path>) -> Result<Restored> {
    let (primary, paths) = session::locate(archive)?;
    if !paths.exists() {
        bail!(
            "{} is not partially extracted; there is nothing to restore",
            primary.display()
        );
    }
    let (mut journal, tail) = Journal::load(&paths)?;
    session::check_output(&journal, &primary, output, "restore")?;
    if journal.keep {
        return discard(&mut journal, tail, &paths);
    }
    // The final commit was journaled: every entry is extracted and the
    // archive only waits to be removed. Truncating it back would leave a
    // file that is no longer the original and no longer an archive.
    if journal.done {
        if !journal.links.is_empty() {
            bail!(
                "every entry is already extracted, so there is nothing to restore; only the \
                 links remain to be created. Run extract to create them"
            );
        }
        bail!("every entry is already extracted; run extract to complete the cleanup");
    }
    if !journal.parts[0].0.exists() {
        bail!("all entries are already extracted. Run extract to complete the cleanup");
    }
    // A streamed entry gives its archive bytes back while it is extracted,
    // and the journal holds a resume point exactly as long as some are gone.
    // An entry with holes in it can't go back into an archive.
    if let Some(r) = &journal.resume {
        if r.stream.is_some() {
            bail!(
                "{} cannot be restored: part of its compressed data may already be released, \
                 and a compressed tar cannot be rebuilt from what remains. Run extract to \
                 complete the extraction instead",
                primary.display()
            );
        }
        bail!(
            "{} cannot be restored: a large entry was being extracted, and the archive space \
             of its first {} may already be released. Run extract to complete the \
             extraction instead",
            primary.display(),
            engine::human(r.written)
        );
    }
    recover(&mut journal, &paths)?;
    // Restoring rewrites the index only and never decrypts file data, so no
    // password is needed. An archive whose file names are encrypted (a 7z
    // with an encrypted header, a RAR made with "Encrypt file names") can't
    // even be listed without one, and a new index for it would have to be
    // encrypted too, so it is refused.
    let job = Job::resume(&journal, tail.clone(), true, None, Default::default())
        .map_err(|e| names_encrypted(e, &primary, "restored"))?;
    if job.units.is_empty() {
        if !journal.links.is_empty() {
            bail!(
                "every entry is already extracted, so there is nothing to restore; only the \
                 links remain to be created. Run extract to create them"
            );
        }
        bail!("all entries are already extracted. Run extract to complete the extraction");
    }
    let out = journal.output.clone();
    let keep = journal.committed_len;

    let remaining: HashMap<&Path, &archive::Item> = job
        .units
        .iter()
        .flat_map(|u| &u.items)
        .map(|i| (i.path.as_path(), i))
        .collect();
    // Links are made at the very end of an extraction, so the journal holds
    // every link whose entry is already cut off. A tar hard link whose
    // target is still in the archive can't be made yet; it goes back in.
    let (back_in, make): (Vec<&Link>, Vec<&Link>) = journal
        .links
        .iter()
        .partition(|l| l.hard && remaining.contains_key(l.target.as_path()));
    let mut expected: Vec<(PathBuf, Kind, u64)> = job
        .units
        .iter()
        .flat_map(|u| &u.items)
        .map(|i| (i.path.clone(), i.kind.clone(), i.size))
        .collect();
    expected.extend(back_in.iter().map(|l| {
        let size = remaining[l.target.as_path()].size;
        (l.path.clone(), Kind::HardLink(l.target.clone()), size)
    }));
    expected.sort_by(|a, b| a.0.cmp(&b.0));

    // Nothing was cut off yet, so the archive is still the original, index
    // and all.
    let untouched = keep == job.storage.original_len();
    let index = if untouched {
        None
    } else {
        let hard: Vec<(&Path, &Path)> = back_in
            .iter()
            .map(|l| (l.path.as_path(), l.target.as_path()))
            .collect();
        Some(archive::rebuild_index(
            job.format_name,
            &job.storage,
            tail,
            keep,
            &hard,
        )?)
    };

    session::clear_uncommitted(&job, &out)?;
    let make: Vec<Link> = make.into_iter().cloned().collect();
    engine::make_links(&make, &out).map_err(|e| {
        anyhow::anyhow!(
            "{e:#}\nThe archive was not modified. Resolve the problem shown for each link, then \
             run restore again"
        )
    })?;

    if let Some(index) = index {
        let mut undo = Vec::with_capacity(index.patches.len());
        for (at, bytes) in &index.patches {
            let mut old = vec![0; bytes.len()];
            let mut r = job.storage.reader(None);
            r.seek(SeekFrom::Start(*at))?;
            r.read_exact(&mut old)
                .with_context(|| format!("reading {}", primary.display()))?;
            undo.push((*at, old));
        }
        journal.restoring = Some(undo);
        journal.save(&paths)?;

        job.storage
            .write_at(keep, &index.append)
            .context("writing the new index")?;
        engine::crash_point("restore-appended");
        for (at, bytes) in &index.patches {
            job.storage
                .write_at(*at, bytes)
                .context("writing the new index")?;
        }
        engine::crash_point("restore-written");

        let found = reopen(&primary);
        if !matches!(&found, Ok(f) if *f == expected) {
            drop(job);
            recover(&mut journal, &paths)?;
            let why = match found {
                Ok(_) => "it does not list the same entries as the remaining archive".to_string(),
                Err(e) => format!("{e:#}"),
            };
            bail!(
                "the restored archive failed verification ({why}) and the restore was reverted. \
                 No data was lost. Run extract to complete the extraction, and report this issue"
            );
        }
    }
    let format = job.format_name;
    drop(job);
    // Files extracted in place are the only copy of their entries now, so
    // a replaced file whose path one of them holds stays aside.
    let set_aside = put_back(&mut journal, &paths, false)?;

    remove(&paths.journal)?;
    fsutil::sync_dir(paths.journal.parent().unwrap_or(Path::new(".")))?;
    remove(&paths.tail)?;
    // Left behind, the record would vouch for whatever journal next turns up
    // next to an archive at this path.
    crate::chosen::forget(&paths.archive);
    Ok(Restored {
        archive: primary,
        format,
        entries: expected.len(),
        output: out,
        kept: false,
        set_aside,
    })
}

/// An extraction that keeps the archive never changed it, so there is
/// nothing to rebuild: what a crashed batch left half written is deleted, as
/// a resume would, and the progress files go. Finished files stay, except
/// where they replaced a file: the archive still holds them, so the
/// replaced file goes back.
fn discard(journal: &mut Journal, tail: Arc<Tail>, paths: &Paths) -> Result<Restored> {
    let primary = journal.parts[0].0.clone();
    let mut restored = Restored {
        archive: primary.clone(),
        format: "",
        entries: 0,
        output: journal.output.clone(),
        kept: true,
        set_aside: 0,
    };
    // Moved away by the person: nothing says which files are half written,
    // and nothing else is left to do.
    if primary.exists() {
        let job = Job::resume(journal, tail, true, None, Default::default())
            .map_err(|e| names_encrypted(e, &primary, "discarded"))?;
        session::clear_uncommitted(&job, &journal.output)?;
        restored.format = job.format_name;
        restored.entries = job.item_count();
    }
    // Without the archive, an extracted file may be the last copy.
    restored.set_aside = put_back(journal, paths, primary.exists())?;
    paths.remove()?;
    Ok(restored)
}

/// Moves the files the extraction replaced back where they were (see
/// [`replace::put_back`]) and returns how many stay aside. The journal
/// first says the move aside is undone, so if this is interrupted, no run
/// deletes a file at the path of an entry left to do as its own: a restore
/// carries on putting files back, and an extraction moves them aside again.
fn put_back(journal: &mut Journal, paths: &Paths, over: bool) -> Result<usize> {
    let Some(r) = journal.replace.as_mut() else {
        return Ok(0);
    };
    r.moved = false;
    journal.save(paths)?;
    replace::put_back(&journal.output, over)
}

/// Restore takes no password, so an archive that can't be read without one
/// is refused with what to do instead, rather than asked for a password the
/// command has no way to take.
fn names_encrypted(e: anyhow::Error, primary: &Path, what: &str) -> anyhow::Error {
    if !e.is::<archive::PasswordNeeded>() {
        return e;
    }
    anyhow::anyhow!(
        "{} has encrypted file names, so this extraction cannot be {what}: that needs the \
         password, which restore does not take. Run extract with the password to complete \
         the extraction instead",
        primary.display()
    )
}

/// Opens the archive as a fresh one, the way a later extraction would, and
/// lists what is in it.
fn reopen(primary: &Path) -> Result<Vec<(PathBuf, Kind, u64)>> {
    let job = Job::open_as_is(primary, false, None, Default::default())?;
    let mut found: Vec<_> = job
        .units
        .iter()
        .flat_map(|u| &u.items)
        .map(|i| (i.path.clone(), i.kind.clone(), i.size))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Puts an archive whose restore was interrupted back the way extraction
/// left it: cuts off whatever restore appended past `committed_len` and puts
/// back the bytes it overwrote below. Every run that opens a half-extracted
/// archive for writing does this first, so `extract` continues and `restore`
/// starts over.
pub(crate) fn recover(journal: &mut Journal, paths: &Paths) -> Result<()> {
    let Some(undo) = journal.restoring.take() else {
        return Ok(());
    };
    let storage = Storage::from_parts(&journal.parts, true)?;
    storage
        .cut_to(journal.committed_len)
        .context("undoing an interrupted restore")?;
    for (at, bytes) in &undo {
        storage
            .write_at(*at, bytes)
            .context("undoing an interrupted restore")?;
    }
    drop(storage);
    journal.save(paths)
}

fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}
