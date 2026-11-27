//! Dragging entries out of the contents tree into Explorer or the Finder.
//!
//! The entries are extracted the way "Extract selected…" extracts them: by
//! the engine, with the archive kept, so nothing here can change the archive
//! or write outside the folder it is given. What differs is where they go
//! and where the run keeps its journal: a drag never has a journal next to
//! the archive, so it can't be mistaken for a run to resume, and it never
//! starts while one is there.
//!
//! The platform code decides when the extraction runs. On Windows it is
//! when the button is released over a folder that takes the files, into a
//! folder of this process under the system's temporary folder, which the
//! file manager then copies from; those folders go when the app closes. On
//! macOS the Finder asks for each entry once it knows the folder, and the
//! entry is extracted next to it and moved into place.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use hatchzip_core::archive::storage::Reads;
use hatchzip_core::engine::{Outcome, Progress, Run};
use hatchzip_core::journal::Paths;
use hatchzip_core::session::{self, State};

/// Every folder a drag writes has a name starting with this, so a drop of
/// one of them back onto the window can be told apart from a real archive.
const PREFIX: &str = "hatchzip-drag-";

/// What the entries are extracted from.
#[derive(Clone)]
pub struct Source {
    pub archive: PathBuf,
    /// The password the window opened the archive with, if it needed one.
    pub password: Option<String>,
    /// Free space the extraction leaves alone, as for any other.
    pub keep_free: u64,
}

/// One row being dragged: its path inside the archive.
pub struct Dragged {
    pub path: PathBuf,
    /// For the icon and the file type the Finder is promised.
    #[allow(dead_code)]
    pub dir: bool,
}

pub struct Drag {
    pub source: Source,
    pub entries: Vec<Dragged>,
}

/// The disk the entries would go to has no room for them.
#[derive(Debug)]
pub struct NoSpace;

impl std::fmt::Display for NoSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("there is not enough free space for the dragged entries")
    }
}

impl std::error::Error for NoSpace {}

static BUSY: AtomicUsize = AtomicUsize::new(0);

/// Held while entries are being extracted for a drop.
struct Busy;

impl Busy {
    fn start() -> Self {
        BUSY.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Entries are being extracted for a drop right now. The extraction only
/// reads the archive, so a run that changes it must wait: on macOS the
/// Finder's request is served on a thread of its own, after the drag ended.
pub fn busy() -> bool {
    BUSY.load(Ordering::SeqCst) > 0
}

/// The folder this process extracts dragged entries into, under the system's
/// temporary folder.
fn root() -> PathBuf {
    std::env::temp_dir().join(format!("{PREFIX}{}", std::process::id()))
}

/// Whether `path` lies in a folder a drag wrote.
pub fn is_ours(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(name) => name
            .to_str()
            .is_some_and(|n| n.trim_start_matches('.').starts_with(PREFIX)),
        _ => false,
    })
}

/// A new, empty folder for one drag, in this process's folder.
#[allow(dead_code)] // Only the platforms that extract before the drop use it.
pub fn new_work() -> Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let work = root().join(NEXT.fetch_add(1, Ordering::Relaxed).to_string());
    if work.exists() {
        fs::remove_dir_all(&work).with_context(|| format!("emptying {}", work.display()))?;
    }
    fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    Ok(work)
}

/// Where [`extract`] into `work` puts the entries.
pub fn output(work: &Path) -> PathBuf {
    work.join("out")
}

/// Where `entry` is once [`extract`] into `work` is done, with the
/// system's separators: Explorer takes `a/b` for a name, not a path.
pub fn extracted(work: &Path, entry: &Path) -> PathBuf {
    let mut path = output(work);
    path.extend(entry.components().filter_map(|c| match c {
        Component::Normal(n) => Some(n),
        _ => None,
    }));
    path
}

/// Extracts `entries` from the archive, and everything inside the folders
/// among them, into [`output`] of `work`, keeping the archive. The run's
/// journal goes in `work` too and is gone once it is done.
pub fn extract(source: &Source, entries: &[PathBuf], work: &Path) -> Result<PathBuf> {
    let _busy = Busy::start();
    let (primary, beside) = session::locate(&source.archive)?;
    // That run may be shortening the archive, and its journal is what it
    // trusts; a drag must not read the one or touch the other.
    if beside.exists() {
        bail!(
            "this archive is partially extracted. Resume or restore it first, then drag the \
             entries again. Nothing was modified"
        );
    }
    fs::create_dir_all(work).with_context(|| format!("creating {}", work.display()))?;
    let paths = Paths {
        archive: primary.clone(),
        journal: work.join("journal"),
        tail: work.join("tail"),
        folders: work.join("folders"),
        stub: None,
    };
    let reads = Arc::new(Reads::default());
    // Read-only: no lock, so the window's own handle on it stays valid,
    // and nothing in it can be written.
    let state = session::open(
        &primary,
        &paths,
        Some(output(work)),
        Some(entries),
        false,
        source.password.as_deref(),
        &reads,
    )?;
    let State::Fresh(job, out) = state else {
        bail!(
            "this archive is partially extracted. Resume or restore it first. Nothing was modified"
        );
    };
    job.check_password()?;
    let plan = session::plan(&job, &out, false, source.keep_free, true)?;
    if !plan.fits {
        return Err(NoSpace.into());
    }
    let journal = session::begin(&job, &paths, out.clone(), true, None)?;
    let stop = AtomicBool::new(false);
    let run = Run {
        job,
        journal,
        paths,
        stop: &stop,
        progress: Progress {
            on_bytes: Box::new(|_| {}),
            on_commit: Box::new(|_| {}),
        },
        keep_free: source.keep_free,
    };
    match run.execute()? {
        Outcome::Finished(_) => Ok(out),
        Outcome::Paused => bail!("the extraction of the dragged entries stopped"),
    }
}

/// Extracts the dragged entries into `work` and checks that each is where
/// the file manager was told it would be.
#[allow(dead_code)] // Only the platforms that extract before the drop use it.
pub fn extract_all(drag: &Drag, work: &Path) -> Result<()> {
    let entries: Vec<PathBuf> = drag.entries.iter().map(|e| e.path.clone()).collect();
    extract(&drag.source, &entries, work)?;
    for e in &drag.entries {
        let path = extracted(work, &e.path);
        if !path.exists() {
            bail!("{} was not extracted", e.path.display());
        }
    }
    Ok(())
}

/// Writes `entry` to `dest`, a path that does not exist yet, as the Finder
/// asks for a file promise: extracts it into a hidden folder next to `dest`,
/// on the same disk, then moves it into place and removes the folder.
#[allow(dead_code)] // Only the platforms with file promises use it.
pub fn write_promise(source: &Source, entry: &Path, dest: &Path) -> Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if dest.symlink_metadata().is_ok() {
        bail!("{} already exists", dest.display());
    }
    let Some(parent) = dest.parent() else {
        bail!("{} has no folder to go in", dest.display());
    };
    let work = parent.join(format!(
        ".{PREFIX}{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let result = extract(source, &[entry.to_path_buf()], &work).and_then(|_| {
        let from = extracted(&work, entry);
        fs::rename(&from, dest)
            .with_context(|| format!("moving {} to {}", from.display(), dest.display()))
    });
    let cleaned = fs::remove_dir_all(&work);
    result?;
    cleaned.with_context(|| format!("removing {}", work.display()))
}

/// Removes this process's folder of dragged entries. Called when the app
/// closes: the file manager may still be copying from it until then.
pub fn cleanup() {
    let _ = fs::remove_dir_all(root());
}

/// Removes the folders of dragged entries an earlier run of the app could
/// not, after a crash. A day old is long past any copy from them.
pub fn clean_stale() {
    clean_stale_in(&std::env::temp_dir(), Duration::from_secs(24 * 60 * 60));
}

fn clean_stale_in(dir: &Path, age: Duration) {
    let Ok(read) = fs::read_dir(dir) else { return };
    let ours = root();
    for entry in read.flatten() {
        let path = entry.path();
        let named = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(PREFIX));
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() >= age);
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if named && old && is_dir && path != ours {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_zip(dir: &Path) -> PathBuf {
        let path = dir.join("game.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.add_directory("game/", opts).unwrap();
        w.add_directory("game/data/", opts).unwrap();
        w.start_file("game/readme.txt", opts).unwrap();
        w.write_all(b"hello\n").unwrap();
        for i in 0..3u8 {
            w.start_file(format!("game/data/{i}.bin"), opts).unwrap();
            w.write_all(&vec![i; 10_000]).unwrap();
        }
        w.finish().unwrap();
        path
    }

    fn source(archive: &Path) -> Source {
        Source {
            archive: archive.to_path_buf(),
            password: None,
            keep_free: 0,
        }
    }

    #[test]
    fn extracts_only_the_dragged_entries_and_keeps_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let archive = make_zip(dir.path());
        let before = fs::read(&archive).unwrap();
        let work = dir.path().join(format!("{PREFIX}test"));
        let drag = Drag {
            source: source(&archive),
            entries: vec![
                Dragged {
                    path: "game/readme.txt".into(),
                    dir: false,
                },
                Dragged {
                    path: "game/data".into(),
                    dir: true,
                },
            ],
        };
        extract_all(&drag, &work).unwrap();
        assert_eq!(
            fs::read(extracted(&work, Path::new("game/readme.txt"))).unwrap(),
            b"hello\n"
        );
        for i in 0..3u8 {
            assert_eq!(
                fs::read(extracted(&work, Path::new(&format!("game/data/{i}.bin")))).unwrap(),
                vec![i; 10_000]
            );
        }
        assert_eq!(
            fs::read(&archive).unwrap(),
            before,
            "the archive is untouched"
        );
        let (_, beside) = session::locate(&archive).unwrap();
        assert!(!beside.exists(), "no journal next to the archive");
        for side in ["journal", "tail", "folders"] {
            assert!(!work.join(side).exists(), "{side} is removed");
        }
        assert!(is_ours(&extracted(&work, Path::new("game/readme.txt"))));
        assert!(!is_ours(&archive));
    }

    #[test]
    fn a_promise_moves_the_entry_into_place_and_leaves_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let archive = make_zip(dir.path());
        let before = fs::read(&archive).unwrap();
        let dest_dir = dir.path().join("Desktop");
        fs::create_dir(&dest_dir).unwrap();
        let dest = dest_dir.join("data");
        write_promise(&source(&archive), Path::new("game/data"), &dest).unwrap();
        assert_eq!(fs::read(dest.join("1.bin")).unwrap(), vec![1; 10_000]);
        let names: Vec<_> = fs::read_dir(&dest_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["data"], "the hidden folder is gone");
        assert_eq!(fs::read(&archive).unwrap(), before);
        // Never over something already there.
        let err = write_promise(&source(&archive), Path::new("game/data"), &dest).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err:#}");
        assert_eq!(fs::read(dest.join("1.bin")).unwrap(), vec![1; 10_000]);
    }

    #[test]
    fn an_entry_not_in_the_archive_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let archive = make_zip(dir.path());
        let work = dir.path().join("work");
        assert!(extract(&source(&archive), &["../outside".into()], &work).is_err());
        assert!(extract(&source(&archive), &["missing".into()], &work).is_err());
        assert!(!dir.path().join("outside").exists());
    }

    #[test]
    fn a_partially_extracted_archive_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let archive = make_zip(dir.path());
        let (_, beside) = session::locate(&archive).unwrap();
        fs::write(&beside.journal, b"{}").unwrap();
        let err = extract(&source(&archive), &["game".into()], &dir.path().join("w")).unwrap_err();
        assert!(err.to_string().contains("partially extracted"), "{err:#}");
        assert_eq!(fs::read(&beside.journal).unwrap(), b"{}");
    }

    #[test]
    fn stale_folders_go_and_recent_ones_stay() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join(format!("{PREFIX}1"));
        let other = dir.path().join("unrelated");
        fs::create_dir(&stale).unwrap();
        fs::create_dir(&other).unwrap();
        clean_stale_in(dir.path(), Duration::from_secs(3600));
        assert!(stale.exists(), "recent");
        assert!(other.exists());
        clean_stale_in(dir.path(), Duration::ZERO);
        assert!(!stale.exists());
        assert!(other.exists(), "only the app's folders go");
    }

    #[test]
    fn extracted_paths_use_the_systems_separators() {
        let work = Path::new("w");
        let mut expected = output(work);
        expected.push("a");
        expected.push("b.txt");
        assert_eq!(extracted(work, Path::new("a/b.txt")), expected);
    }
}
