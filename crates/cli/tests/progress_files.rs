//! A folder the app works out from the archive's name, as "Extract to
//! folder" does, is one a journal planted next to the archive can name just
//! as well. Such a journal is only followed with this computer's record of
//! starting the extraction, the same as for the default folder.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;

use common::*;
use hatchzip_core::session::{self, Output, State};
use tempfile::TempDir;

/// The records go where the binary's runs put theirs, so a run begun here
/// resumes from the command line. Every test calls this before anything
/// else, so no thread reads the environment while it changes.
fn isolate() {
    static ONCE: Once = Once::new();
    // SAFETY: see above; the tests of this file only start their work once
    // this has returned.
    ONCE.call_once(|| unsafe { std::env::set_var("HATCHZIP_STATE_DIR", state_dir()) });
}

fn files() -> Files {
    let mut files = Files::new();
    files.insert("one.txt".into(), noise(20_000, 1));
    files.insert("sub/two.txt".into(), noise(20_000, 2));
    files
}

fn refusal(primary: &Path, output: Output) -> String {
    let paths = journal_paths(primary);
    let reads = Default::default();
    match session::open(primary, &paths, output, None, true, None, &reads) {
        Ok(_) => panic!("the journal should have been refused"),
        Err(e) => format!("{e:#}"),
    }
}

fn journal_paths(primary: &Path) -> hatchzip_core::journal::Paths {
    session::locate(primary).unwrap().1
}

/// A journal that came with the archive names the folder "Extract to
/// folder" works out, the archive's own name as its part and nothing
/// extracted yet, so a resume would cut the whole archive away and delete
/// the file already in that folder. Nothing on this computer recorded it.
#[test]
fn a_planted_journal_for_the_computed_folder_is_refused() {
    isolate();
    let tmp = TempDir::new().unwrap();
    let archive = tar_of(tmp.path(), "game.tar", &files());
    let (primary, _) = session::locate(&archive).unwrap();
    let out = session::default_output(&primary);
    assert_eq!(out, tmp.path().join("game"));
    fs::create_dir(&out).unwrap();
    fs::write(out.join("one.txt"), b"a file of the person's").unwrap();
    let before = fs::read(&archive).unwrap();
    let len = before.len() as u64;
    let [journal, tail] = side_files(&archive);
    fs::write(&tail, b"").unwrap();
    fs::write(
        &journal,
        serde_json::json!({
            "version": 3, "format": "tar", "parts": [[&primary, len]], "tail_start": len,
            "tail_crc": 0, "output": &out, "total_size": 0, "committed_len": 0, "links": []
        })
        .to_string(),
    )
    .unwrap();

    for output in [Output::Computed(out.clone()), Output::Default] {
        let why = refusal(&primary, output);
        assert!(why.contains("no record of starting it"), "{why}");
        assert!(why.contains("Nothing was modified"), "{why}");
    }
    let run = hatchzip(
        &[
            "extract",
            archive.to_str().unwrap(),
            "--keep-free",
            "0",
            "--yes",
        ],
        &[],
    );
    assert!(!run.status.success());
    assert!(
        !String::from_utf8_lossy(&run.stderr).contains(&format!("-o \"{}\"", out.display())),
        "the refusal must not suggest naming the journal's folder"
    );

    assert_eq!(fs::read(&archive).unwrap(), before);
    assert_eq!(
        fs::read(out.join("one.txt")).unwrap(),
        b"a file of the person's"
    );
    assert!(journal.exists() && tail.exists());
}

/// A run started the way "Extract to folder" starts one, into the folder
/// next to the archive or into the default destination from Settings, is
/// recorded like any other: interrupted, it resumes from that menu and from
/// the main window, and finishes.
#[test]
fn an_interrupted_extract_to_folder_run_resumes() {
    isolate();
    for settings_folder in [false, true] {
        let tmp = TempDir::new().unwrap();
        let archive = tar_of(tmp.path(), "game.tar", &files());
        let (primary, paths) = session::locate(&archive).unwrap();
        let computed: PathBuf = if settings_folder {
            tmp.path().join("extracted").join("game")
        } else {
            session::default_output(&primary)
        };
        let reads = Default::default();
        let State::Fresh(job, out) = session::open(
            &primary,
            &paths,
            Output::Computed(computed.clone()),
            None,
            true,
            None,
            &reads,
        )
        .unwrap() else {
            panic!("a new archive opens fresh");
        };
        assert_eq!(out, computed);
        session::begin(&job, &paths, out.clone(), false, None).unwrap();
        // Stopped before the first batch.
        drop(job);

        for output in [Output::Computed(computed.clone()), Output::Default] {
            let State::Resume(job, journal) =
                session::open(&primary, &paths, output, None, true, None, &reads).unwrap()
            else {
                panic!("the run resumes");
            };
            assert_eq!(journal.output, computed);
            drop(job);
        }

        let a = archive.to_str().unwrap();
        assert_ok(&hatchzip(&["extract", a, "--keep-free", "0", "--yes"], &[]));
        for (name, data) in files() {
            assert_eq!(fs::read(computed.join(&name)).unwrap(), data, "{name}");
        }
        assert_consumed(&archive);
    }
}
