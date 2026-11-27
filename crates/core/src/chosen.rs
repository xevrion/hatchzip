//! The output folders of the extractions started on this computer, the
//! default one included, kept in the person's own profile rather than next
//! to the archive. A journal next to the archive may have been put there by
//! whoever supplied the archive; this record can't have been, so a journal
//! whose folder is found in it belongs to an extraction the person started,
//! and a resume may follow it without being told again.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::fsutil;

#[derive(Serialize, Deserialize)]
struct Record {
    archive: PathBuf,
    output: PathBuf,
}

/// `%LOCALAPPDATA%\HatchZip`, `~/Library/Application Support/HatchZip`, or
/// `$XDG_STATE_HOME/hatchzip`, which defaults to `~/.local/state/hatchzip`.
fn dir() -> Option<PathBuf> {
    // The test suites keep their records away from the person running them.
    if cfg!(test) {
        return Some(std::env::temp_dir().join("hatchzip-test-outputs"));
    }
    if cfg!(debug_assertions)
        && let Some(d) = std::env::var_os("HATCHZIP_STATE_DIR")
    {
        return Some(PathBuf::from(d));
    }
    let var = |name| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let base = if cfg!(windows) {
        var("LOCALAPPDATA")?.join("HatchZip")
    } else if cfg!(target_os = "macos") {
        var("HOME")?.join("Library/Application Support/HatchZip")
    } else {
        var("XDG_STATE_HOME")
            .or_else(|| Some(var("HOME")?.join(".local/state")))?
            .join("hatchzip")
    };
    Some(base.join("outputs"))
}

/// One file per archive, named after its path. Two paths sharing a name
/// only cost one of them its record, since each file says whose it is.
fn file(archive: &Path) -> Option<PathBuf> {
    let hash = crc32fast::hash(archive.as_os_str().as_encoded_bytes());
    Some(dir()?.join(format!("{hash:08x}.json")))
}

fn read(archive: &Path) -> Option<(PathBuf, Record)> {
    let file = file(archive)?;
    let record: Record = serde_json::from_slice(&fs::read(&file).ok()?).ok()?;
    fsutil::same_path(&record.archive, archive).then_some((file, record))
}

/// Notes that the person chose `output` for `archive`. An extraction that
/// can't be noted is not started: nothing could tell its journal from one
/// planted next to the archive, so it could never be resumed.
pub(crate) fn remember(archive: &Path, output: &Path) -> Result<()> {
    let file = file(archive).ok_or_else(|| {
        anyhow!(
            "HatchZip cannot find your profile folder to record this extraction in, which it \
             needs to resume it. Nothing was modified"
        )
    })?;
    let record = Record {
        archive: archive.to_path_buf(),
        output: output.to_path_buf(),
    };
    let bytes = serde_json::to_vec(&record)?;
    let parent = file.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .map_err(anyhow::Error::from)
        .and_then(|()| fsutil::write_atomic(&file, &bytes))
        .with_context(|| {
            format!(
                "recording this extraction in {}, which HatchZip needs to resume it. Nothing \
                 was modified",
                parent.display()
            )
        })
}

pub(crate) fn chose(archive: &Path, output: &Path) -> bool {
    read(archive).is_some_and(|(_, r)| fsutil::same_path(&r.output, output))
}

pub(crate) fn forget(archive: &Path) {
    if let Some((file, _)) = read(archive) {
        let _ = fs::remove_file(file);
    }
}
