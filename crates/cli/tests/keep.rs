//! `--keep-archive`: a standard extraction through the same engine. The
//! archive must come out byte for byte as it went in, for every format and
//! split, the output must be the same as an in-place run's, an interrupted
//! run must resume without touching the archive, and the space plan must
//! ask for room for every file.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::*;
use hatchzip_core::engine::Job;
use hatchzip_core::human;
use hatchzip_core::session;
use tempfile::TempDir;

/// Shrinks the step between `.tar.gz` resume points to 1 MiB, so the 3 MB
/// file in the sample crosses a few.
const STEP: (&str, &str) = ("HATCHZIP_STREAM_STEP", "1048576");

type Build = fn(&Path, &Files) -> PathBuf;

/// Every format, and split sets of three of them. Each returns the path to
/// extract: the archive, or the first volume.
fn cases() -> Vec<(&'static str, Build)> {
    vec![
        ("zip", |d, f| zip_of(d, "game.zip", f)),
        ("7z", |d, f| sevenz_of(d, "game.7z", f, false)),
        ("solid 7z", |d, f| sevenz_of(d, "game.7z", f, true)),
        ("tar", |d, f| tar_of(d, "game.tar", f)),
        ("tar.gz", |d, f| targz_of(d, "game.tar.gz", f)),
        ("split zip", |d, f| split_up(&zip_of(d, "game.zip", f), 3)),
        ("split solid 7z", |d, f| {
            split_up(&sevenz_of(d, "game.7z", f, true), 3)
        }),
        ("split tar.gz", |d, f| {
            split_up(&targz_of(d, "game.tar.gz", f), 2)
        }),
    ]
}

fn keep(archive: &Path, out: &Path, env: &[(&str, &str)]) -> Output {
    let (a, o) = (archive.to_str().unwrap(), out.to_str().unwrap());
    // No --yes: keeping the archive needs no confirmation, even without a
    // terminal.
    hatchzip(
        &["extract", a, "-o", o, "--keep-free", "0", "--keep-archive"],
        env,
    )
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn journal(archive: &Path) -> serde_json::Value {
    let [journal, _] = side_files(archive);
    serde_json::from_slice(&std::fs::read(journal).unwrap()).unwrap()
}

#[test]
fn every_format_keeps_its_archive_byte_for_byte() {
    let files = sample_files();
    for (name, build) in cases() {
        let tmp = TempDir::new().unwrap();
        let archive = build(tmp.path(), &files);
        let before = snapshot(&archive);
        let out = tmp.path().join("out");

        // One unit per batch, so the run commits many times.
        let run = keep(&archive, &out, &[("HATCHZIP_BATCH_ENTRIES", "1"), STEP]);
        assert_ok(&run);
        assert!(
            stdout(&run).contains("The archive was kept."),
            "{name}: {}",
            stdout(&run)
        );
        assert_tree(&out, &files);
        assert_unchanged(&before);
        assert_no_side_files(&archive);

        // The same archive then extracts in place to the same files, so
        // both modes write identical output.
        let again = tmp.path().join("again");
        assert_ok(&extract(&archive, &again, &[]));
        assert_tree(&again, &files);
        assert_consumed(&archive);
        for (p, _) in &before {
            assert!(!p.exists(), "{name}: {} should be gone", p.display());
        }
    }
}

/// Killed at every commit point (and, for a compressed tar, at every step
/// of a resume point), then continued by a plain `extract`, which keeps the
/// archive because the journal says so.
#[test]
fn an_interrupted_run_resumes_and_still_keeps_the_archive() {
    let files = sample_files();
    let batches = ["after-extract", "after-journal"];
    let stream = [
        "stream-after-sync",
        "stream-after-journal",
        "stream-after-punch",
    ];
    // A compressed tar is one unit, so it commits once, at the very end.
    let runs: Vec<(&str, Build, &[&str], &[&str])> = vec![
        (
            "zip",
            |d, f| zip_of(d, "game.zip", f),
            &batches,
            &["0", "2"],
        ),
        (
            "split solid 7z",
            |d, f| split_up(&sevenz_of(d, "game.7z", f, true), 3),
            &batches,
            &["0", "2"],
        ),
        (
            "tar",
            |d, f| tar_of(d, "game.tar", f),
            &batches,
            &["0", "2"],
        ),
        (
            "tar.gz",
            |d, f| targz_of(d, "game.tar.gz", f),
            &stream,
            &["0", "2"],
        ),
        (
            "tar.gz",
            |d, f| targz_of(d, "game.tar.gz", f),
            &batches,
            &["0"],
        ),
    ];
    for (name, build, points, afters) in runs {
        for point in points {
            for &after in afters {
                let tmp = TempDir::new().unwrap();
                let archive = build(tmp.path(), &files);
                let before = snapshot(&archive);
                let out = tmp.path().join("out");

                let crashed = keep(
                    &archive,
                    &out,
                    &[
                        ("HATCHZIP_BATCH_ENTRIES", "1"),
                        STEP,
                        ("HATCHZIP_CRASH_AT", point),
                        ("HATCHZIP_CRASH_AFTER", after),
                    ],
                );
                let what = format!("{name} {point}#{after}");
                assert!(!crashed.status.success(), "{what} should have crashed");
                assert_unchanged(&before);
                let j = journal(&archive);
                assert_eq!(j["keep"], true, "{what}");
                // Saved folder entries take version 7, which older builds
                // refuse as well.
                let version = if j["folders"].is_null() { 4 } else { 7 };
                assert_eq!(j["version"], version, "{what}");

                let resumed = extract(&archive, &out, &[STEP]);
                assert_ok(&resumed);
                assert!(
                    stdout(&resumed).contains("the archive is kept"),
                    "{what}: {}",
                    stdout(&resumed)
                );
                assert_tree(&out, &files);
                assert_unchanged(&before);
                assert_no_side_files(&archive);
            }
        }
    }
}

#[test]
fn restore_discards_the_progress_of_a_kept_run() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let archive = zip_of(tmp.path(), "game.zip", &files);
    let before = snapshot(&archive);
    let out = tmp.path().join("out");
    let crashed = keep(
        &archive,
        &out,
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-extract"),
            ("HATCHZIP_CRASH_AFTER", "3"),
        ],
    );
    assert!(!crashed.status.success());

    let a = archive.to_str().unwrap();
    let restored = hatchzip(&["restore", a, "-o", out.to_str().unwrap()], &[]);
    assert_ok(&restored);
    assert!(
        stdout(&restored).contains("Extraction progress discarded."),
        "{}",
        stdout(&restored)
    );
    assert_unchanged(&before);
    assert_no_side_files(&archive);
    // The batch the crash cut short is gone with the progress; what was
    // committed stays, as after any restore.
    let parts = walk_files(&out)
        .into_iter()
        .filter(|p| p.to_string_lossy().ends_with(".hatchzip-part"))
        .count();
    assert_eq!(parts, 0);

    // The archive works as any other: a new run extracts all of it.
    let fresh = tmp.path().join("fresh");
    assert_ok(&keep(&archive, &fresh, &[]));
    assert_tree(&fresh, &files);
    assert_unchanged(&before);
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut all = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return all;
    };
    for e in entries {
        let p = e.unwrap().path();
        if p.is_dir() {
            all.extend(walk_files(&p));
        } else {
            all.push(p);
        }
    }
    all
}

/// Part of an archive extracted in place is gone, so it can't be kept.
#[test]
fn an_in_place_run_cannot_switch_to_keeping() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let archive = zip_of(tmp.path(), "game.zip", &files);
    let out = tmp.path().join("out");
    let crashed = extract(
        &archive,
        &out,
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "1"),
        ],
    );
    assert!(!crashed.status.success());
    let after_crash = snapshot(&archive);
    let a = archive.to_str().unwrap();

    for command in ["extract", "check"] {
        let refused = hatchzip(&[command, a, "--keep-archive", "--keep-free", "0"], &[]);
        assert_eq!(refused.status.code(), Some(1), "{command}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("cannot be kept"),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
        assert_unchanged(&after_crash);
    }
    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

/// Shrinking a file shrinks it under every name it has, so another hard link
/// to the archive, or to one volume of a set, would be emptied without a
/// word. Refused before anything changes; keeping the archive is still fine.
#[test]
fn a_hard_linked_archive_is_only_extracted_while_kept() {
    let files = sample_files();
    for (name, build) in cases() {
        let tmp = TempDir::new().unwrap();
        let archive = build(tmp.path(), &files);
        let before = snapshot(&archive);
        let volume = &before.last().unwrap().0;
        let other = tmp.path().join("other name");
        fs::hard_link(volume, &other).unwrap();
        let a = archive.to_str().unwrap();

        for args in [
            vec!["check", a, "--keep-free", "0"],
            vec!["extract", a, "--keep-free", "0", "--yes"],
        ] {
            let refused = hatchzip(&args, &[]);
            assert_eq!(refused.status.code(), Some(1), "{name}: {args:?}");
            assert!(
                String::from_utf8_lossy(&refused.stderr).contains("another hard link"),
                "{name}: {}",
                String::from_utf8_lossy(&refused.stderr)
            );
            assert_unchanged(&before);
        }
        assert_no_side_files(&archive);

        let out = tmp.path().join("out");
        assert_ok(&keep(&archive, &out, &[]));
        assert_tree(&out, &files);
        assert_unchanged(&before);
        assert_eq!(fs::read(&other).unwrap(), before.last().unwrap().1);
    }
}

/// With the free space set between what extracting in place needs and what
/// keeping the archive needs, keeping is refused and says so, and in place
/// runs.
#[test]
fn the_space_plan_for_keeping_counts_every_file() {
    let tmp = TempDir::new().unwrap();
    // Stored entries that don't compress: in place needs about one of them
    // at a time, keeping needs all of them.
    let mut files = Files::new();
    for i in 0..10 {
        files.insert(format!("game/data/{i}.pak"), noise(400_000, i));
    }
    let archive = zip_of(tmp.path(), "game.zip", &files);
    let out = tmp.path().join("out");

    let (normal, in_place) = {
        let job = Job::open(&archive, false, None, Default::default()).unwrap();
        let plan = session::plan(&job, &out, false, 0, true).unwrap();
        (plan.normal_needs, plan.in_place_needs)
    };
    let files_on_disk: u64 = files
        .values()
        .map(|d| (d.len() as u64).next_multiple_of(4096))
        .sum();
    assert!(normal >= files_on_disk, "{normal} < {files_on_disk}");
    assert!(in_place < normal / 2, "in place {in_place}, kept {normal}");

    let a = archive.to_str().unwrap();
    let o = out.to_str().unwrap();
    let before = snapshot(&archive);
    let between = ((normal + in_place) / 2).to_string();
    let tight = [("HATCHZIP_FREE", between.as_str())];

    let check = hatchzip(&["check", a, "--keep-archive", "--keep-free", "0"], &tight);
    assert_eq!(check.status.code(), Some(3));
    let text = stdout(&check);
    assert!(
        text.contains(&format!("required                     {}", human(normal))),
        "{text}"
    );
    assert!(text.contains("Insufficient disk space:"), "{text}");
    assert!(
        text.contains(&format!(
            "Without --keep-archive, the archive is removed during extraction and only {} is required.",
            human(in_place)
        )),
        "{text}"
    );

    let check = hatchzip(&["check", a, "--keep-free", "0"], &tight);
    assert_ok(&check);
    assert!(!stdout(&check).contains("enough space to keep"));

    let refused = hatchzip(
        &["extract", a, "-o", o, "--keep-archive", "--keep-free", "0"],
        &tight,
    );
    assert_eq!(refused.status.code(), Some(3));
    assert_unchanged(&before);
    assert_no_side_files(&archive);
    assert!(!out.exists());

    // With room for every file, the in-place plan points out that keeping
    // the archive fits too.
    let roomy = (normal + 1_000_000).to_string();
    let check = hatchzip(
        &["check", a, "--keep-free", "0"],
        &[("HATCHZIP_FREE", roomy.as_str())],
    );
    assert_ok(&check);
    assert!(
        stdout(&check).contains("there is also enough space to keep the archive"),
        "{}",
        stdout(&check)
    );

    // In place fits where keeping didn't.
    assert_ok(&hatchzip(
        &["extract", a, "-o", o, "--keep-free", "0", "-y"],
        &tight,
    ));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}
