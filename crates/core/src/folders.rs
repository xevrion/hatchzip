//! Folder permissions and modified times. Every file and link made inside a
//! folder changes its time, and a folder without write permission takes no
//! new files, so they are set once, at the very end, deepest folder first. A
//! ZIP or 7z folder entry's header can be freed long before then, so the list
//! is saved next to the journal before anything is extracted.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::archive::{Kind, Unit};
use crate::{fsutil, platform};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// Seconds and nanoseconds from the Unix epoch, the seconds negative
    /// before 1970.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<(i64, u32)>,
}

/// What the journal records about the saved list, so a resume uses only the
/// file this run wrote.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Saved {
    pub count: u64,
    pub crc: u32,
}

/// The folder entries of `units` that carry a mode or a time. A folder listed
/// twice takes its last entry in the archive, as a file does.
pub fn of(units: &[Unit]) -> Vec<Folder> {
    // A job holds its units last first.
    let mut in_order: Vec<&Unit> = units.iter().collect();
    in_order.sort_by_key(|u| u.start);
    let mut folders: Vec<Folder> = Vec::new();
    let mut at: HashMap<&Path, usize> = HashMap::new();
    for item in in_order.into_iter().flat_map(|u| &u.items) {
        if item.kind != Kind::Dir || (item.mode.is_none() && item.mtime.is_none()) {
            continue;
        }
        let folder = Folder {
            path: item.path.clone(),
            mode: item.mode,
            mtime: item.mtime.map(to_epoch),
        };
        match at.get(item.path.as_path()) {
            Some(&i) => folders[i] = folder,
            None => {
                at.insert(&item.path, folders.len());
                folders.push(folder);
            }
        }
    }
    folders
}

/// Writes `folders` to `path`. Nothing is written when there are none.
pub fn save(path: &Path, folders: &[Folder]) -> Result<Option<Saved>> {
    if folders.is_empty() {
        return Ok(None);
    }
    let bytes = serde_json::to_vec(folders)?;
    fsutil::write_atomic(path, &bytes)?;
    Ok(Some(Saved {
        count: folders.len() as u64,
        crc: crc32fast::hash(&bytes),
    }))
}

/// Reads the list [`save`] wrote. Each path is joined to the output folder,
/// so each must stay inside it.
pub fn load(path: &Path, saved: Saved) -> Result<Vec<Folder>> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mismatch = || anyhow!("{} does not match its journal", path.display());
    if crc32fast::hash(&bytes) != saved.crc {
        return Err(mismatch());
    }
    let folders: Vec<Folder> = serde_json::from_slice(&bytes).map_err(|_| mismatch())?;
    let inside = |p: &Path| {
        p.components().next().is_some() && p.components().all(|c| matches!(c, Component::Normal(_)))
    };
    if folders.len() as u64 != saved.count || !folders.iter().all(|f| inside(&f.path)) {
        return Err(mismatch());
    }
    Ok(folders)
}

/// Gives each folder in `out` its mode and time, deepest first, so setting
/// one never touches its parent's time again. A folder that is gone is
/// skipped; one reached through a link is refused.
pub fn apply(folders: &[Folder], out: &Path) -> Result<()> {
    let mut order: Vec<&Folder> = folders.iter().collect();
    order.sort_by_key(|f| std::cmp::Reverse(f.path.components().count()));
    for f in order {
        apply_one(f, out).map_err(|e| {
            anyhow!(
                "every file is extracted, but the permissions and time of folder '{}' could not \
                 be set: {e:#}\nResolve the problem, then resume the extraction to set them",
                f.path.display()
            )
        })?;
    }
    Ok(())
}

fn apply_one(f: &Folder, out: &Path) -> Result<()> {
    // Never follows a link, the folder's own name included.
    for p in f.path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        match fs::symlink_metadata(out.join(p)) {
            Ok(m) if m.is_symlink() => bail!("{} is a link", out.join(p).display()),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    let dir = platform::open_folder_no_follow(&out.join(&f.path))?;
    platform::apply_mode(&dir, f.mode)?;
    if let Some(t) = f.mtime.and_then(from_epoch) {
        dir.set_modified(t)?;
    }
    Ok(())
}

fn to_epoch(t: SystemTime) -> (i64, u32) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            match d.subsec_nanos() {
                0 => (-(d.as_secs() as i64), 0),
                n => (-(d.as_secs() as i64) - 1, 1_000_000_000 - n),
            }
        }
    }
}

fn from_epoch((secs, nanos): (i64, u32)) -> Option<SystemTime> {
    let base = if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))?
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))?
    };
    base.checked_add(Duration::from_nanos(u64::from(nanos)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_before_1970_round_trip() {
        for t in [
            UNIX_EPOCH - Duration::from_secs(152_391_233),
            UNIX_EPOCH - Duration::new(5, 250_000_000),
            UNIX_EPOCH + Duration::new(1_700_000_000, 999),
            UNIX_EPOCH,
        ] {
            assert_eq!(from_epoch(to_epoch(t)), Some(t));
        }
    }
}
