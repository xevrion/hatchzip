//! `--only`: extracting some entries, always keeping the archive. Only the
//! entries chosen and everything inside the folders among them may land in
//! the output, byte for byte, the archive must come out exactly as it went
//! in, and anything that can't work is refused before a file is written.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::*;
use hatchzip_core::session::{self, State};
use tempfile::TempDir;

type Build = fn(&Path, &Files) -> PathBuf;

/// Every format family: one entry per unit, a solid block holding entries
/// chosen and left out, a compressed stream, and split sets, a native split
/// ZIP among them: kept, it is never cut, so a selection works as for any
/// other set.
fn cases() -> Vec<(&'static str, Build)> {
    vec![
        ("zip", |d, f| zip_of(d, "game.zip", f)),
        ("7z", |d, f| sevenz_of(d, "game.7z", f, false)),
        ("solid 7z", |d, f| sevenz_of(d, "game.7z", f, true)),
        ("tar", |d, f| tar_of(d, "game.tar", f)),
        ("tar.gz", |d, f| targz_of(d, "game.tar.gz", f)),
        ("tar.xz", |d, f| tarxz_of(d, "game.tar.xz", f)),
        ("split zip", |d, f| split_up(&zip_of(d, "game.zip", f), 3)),
        ("native split zip", |d, f| {
            split_zip_of(d, f, &Split::default())
        }),
        ("split solid 7z", |d, f| {
            split_up(&sevenz_of(d, "game.7z", f, true), 3)
        }),
    ]
}

/// A folder, a file nested two folders deep, a name outside ASCII and the
/// empty folder entry.
const CHOSEN: &[&str] = &[
    "game/config",
    "game/many/file07.txt",
    "game/naïve résumé/日本語.txt",
    EMPTY_DIR,
];

fn only(archive: &Path, out: &Path, chosen: &[&str], env: &[(&str, &str)]) -> Output {
    let mut args = vec![
        "extract".to_string(),
        archive.to_string_lossy().into_owned(),
        "-o".into(),
        out.to_string_lossy().into_owned(),
        "--keep-free".into(),
        "0".into(),
    ];
    for c in chosen {
        args.push("--only".into());
        args.push((*c).into());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // No --yes: nothing is removed, so there is nothing to confirm, even
    // without a terminal.
    hatchzip(&args, env)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Whether `name` is one of `chosen` or inside a folder among them.
fn picked(chosen: &[&str], name: &str) -> bool {
    chosen
        .iter()
        .any(|c| name == *c || name.starts_with(&format!("{c}/")))
}

/// Exactly the files of `files` that `chosen` picks are in `out`, as in the
/// archive, and nothing else is.
fn assert_selected(out: &Path, files: &Files, chosen: &[&str]) {
    let mut wanted = 0;
    for (name, data) in files {
        let path = out.join(name);
        if picked(chosen, name) {
            wanted += 1;
            let got = fs::read(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(got == *data, "{name} differs from the archive");
        } else {
            assert!(
                fs::symlink_metadata(&path).is_err(),
                "{name} was not chosen but was extracted"
            );
        }
    }
    assert!(wanted > 0, "the selection picks nothing in the sample");
    let mut found = Vec::new();
    walk_files(out, out, &mut found);
    for rel in found {
        assert!(
            files.contains_key(&rel) && picked(chosen, &rel),
            "unexpected file {rel}"
        );
    }
}

fn walk_files(root: &Path, dir: &Path, found: &mut Vec<String>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() && !p.is_symlink() {
            walk_files(root, &p, found);
        } else {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy();
            found.push(rel.replace('\\', "/"));
        }
    }
}

/// Where a native split ZIP's stub journal goes, next to its `.zip`; for
/// any other archive, a name nothing uses.
fn stub(archive: &Path) -> PathBuf {
    archive.with_extension("zip.hatchzip")
}

fn journal(archive: &Path) -> serde_json::Value {
    let [journal, _] = side_files(archive);
    serde_json::from_slice(&fs::read(journal).unwrap()).unwrap()
}

#[test]
fn every_format_extracts_only_the_selection_and_keeps_the_archive() {
    let files = sample_files();
    for (name, build) in cases() {
        let tmp = TempDir::new().unwrap();
        let archive = build(tmp.path(), &files);
        let before = snapshot(&archive);
        let out = tmp.path().join("out");

        // One unit per batch, so units with nothing chosen sit between
        // committed ones.
        let run = only(&archive, &out, CHOSEN, &[("HATCHZIP_BATCH_ENTRIES", "1")]);
        assert_ok(&run);
        let said = stdout(&run);
        assert!(
            said.contains("--only implies --keep-archive")
                && said.contains("The archive was kept."),
            "{name}: {said}"
        );
        assert_selected(&out, &files, CHOSEN);
        assert!(out.join(EMPTY_DIR).is_dir(), "{name}: {EMPTY_DIR}");
        assert!(!out.join("game/data").exists(), "{name}: game/data");
        assert_unchanged(&before);
        assert_no_side_files(&archive);
        assert!(!stub(&archive).exists(), "{name}: the stub was left");
    }
}

/// The plan counts the chosen files only: far less than every file.
#[test]
fn the_space_plan_counts_only_the_selection() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let archive = zip_of(tmp.path(), "game.zip", &files);
    let (primary, paths) = session::locate(&archive).unwrap();
    let reads = Default::default();
    let chosen = [PathBuf::from("game/config")];
    let State::Fresh(job, out) = session::open(
        &primary,
        &paths,
        session::Output::Default,
        Some(chosen.as_slice()),
        false,
        None,
        &reads,
    )
    .unwrap() else {
        panic!("a new archive opens fresh");
    };
    let plan = session::plan(&job, &out, false, 0, true).unwrap();
    let tail = job.storage.original_len() - job.tail_start;
    let config = files["game/config/settings.ini"].len() as u64;
    assert_eq!(plan.needed, config.next_multiple_of(4096) + tail);
    assert_eq!(job.item_count(), 1);
    assert_eq!(job.remaining_size(), config);
}

#[test]
fn a_path_not_in_the_archive_is_refused_before_anything() {
    let files = sample_files();
    for (name, build) in cases() {
        let tmp = TempDir::new().unwrap();
        let archive = build(tmp.path(), &files);
        let before = snapshot(&archive);
        let out = tmp.path().join("out");
        for wrong in [
            &["game/config", "game/nothing.txt"][..],
            // A prefix of a name is not a folder.
            &["game/conf"],
            &["../game"],
        ] {
            let run = only(&archive, &out, wrong, &[]);
            assert!(!run.status.success(), "{name}: {wrong:?}");
            assert_eq!(run.status.code(), Some(1), "{name}: {wrong:?}");
            let err = stderr(&run);
            assert!(err.contains("Nothing was modified"), "{name}: {err}");
            assert!(!out.exists(), "{name}: {wrong:?} created the output");
            assert_unchanged(&before);
            assert_no_side_files(&archive);
            assert!(!stub(&archive).exists(), "{name}: {wrong:?} left a stub");
        }
    }
}

/// Only the command line implies keeping the archive. Through the library,
/// a selection that removes the archive is refused before the journal is
/// written, and so is a run handed such a job.
#[test]
fn a_selection_without_keeping_the_archive_is_refused() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let archive = zip_of(tmp.path(), "game.zip", &files);
    let before = snapshot(&archive);
    let (primary, paths) = session::locate(&archive).unwrap();
    let reads = Default::default();
    let chosen = [PathBuf::from("game/many")];
    let State::Fresh(job, out) = session::open(
        &primary,
        &paths,
        session::Output::Default,
        Some(chosen.as_slice()),
        true,
        None,
        &reads,
    )
    .unwrap() else {
        panic!("a new archive opens fresh");
    };
    let err = session::begin(&job, &paths, out.clone(), false, None)
        .err()
        .expect("removing the archive with a selection is refused");
    assert!(format!("{err:#}").contains("kept"), "{err:#}");
    assert!(!paths.exists(), "no journal was written");
    assert!(!out.exists());
    drop(job);
    assert_unchanged(&before);
}

/// A crash leaves a version 9 journal with the selection; plain extract, or
/// a path inside one of the folders chosen, goes on with exactly those
/// entries.
#[test]
fn a_crashed_selection_resumes_with_the_same_entries() {
    let files = sample_files();
    for (name, build) in cases() {
        for point in ["after-extract", "after-journal"] {
            let tmp = TempDir::new().unwrap();
            let archive = build(tmp.path(), &files);
            let before = snapshot(&archive);
            let out = tmp.path().join("out");
            let crashed = only(
                &archive,
                &out,
                CHOSEN,
                &[
                    ("HATCHZIP_BATCH_ENTRIES", "1"),
                    ("HATCHZIP_CRASH_AT", point),
                    ("HATCHZIP_CRASH_AFTER", "1"),
                ],
            );
            let what = format!("{name} {point}");
            if crashed.status.success() {
                // A compressed stream is one unit and commits once.
                assert!(name.starts_with("tar."), "{what} should have crashed");
                continue;
            }
            let j = journal(&archive);
            assert_eq!(j["version"], 9, "{what}");
            assert_eq!(j["keep"], true, "{what}");
            if name == "native split zip" {
                assert_eq!(j["format"], "split-zip", "{what}");
                assert!(stub(&archive).exists(), "{what}: no stub");
            }
            assert!(
                j["only"].as_array().is_some_and(|o| o.len() == 4),
                "{what}: {j}"
            );
            assert_unchanged(&before);

            // Another selection is refused, untouched.
            let other = only(&archive, &out, &["game/data"], &[]);
            assert!(!other.status.success(), "{what}");
            assert!(stderr(&other).contains("other entries selected"), "{what}");
            assert!(!out.join("game/data").exists(), "{what}");

            if point == "after-journal" {
                // Part of the selection, so the run goes on with all of it.
                assert_ok(&only(&archive, &out, &["game/config/settings.ini"], &[]));
            } else {
                let resumed = hatchzip(
                    &[
                        "extract",
                        archive.to_str().unwrap(),
                        "-o",
                        out.to_str().unwrap(),
                        "--keep-free",
                        "0",
                    ],
                    &[],
                );
                assert_ok(&resumed);
                assert!(
                    stdout(&resumed).contains("started with --only"),
                    "{what}: {}",
                    stdout(&resumed)
                );
            }
            assert_selected(&out, &files, CHOSEN);
            assert_unchanged(&before);
            assert_no_side_files(&archive);
            assert!(!stub(&archive).exists(), "{what}: the stub was left");
        }
    }
}

/// A compressed stream is one unit: a crash before its commit starts it
/// over, and the files it wrote are deleted first, as for any batch.
#[test]
fn a_crashed_stream_selection_starts_over() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let archive = targz_of(tmp.path(), "game.tar.gz", &files);
    let before = snapshot(&archive);
    let out = tmp.path().join("out");
    let crashed = only(
        &archive,
        &out,
        CHOSEN,
        &[
            ("HATCHZIP_STREAM_STEP", "65536"),
            ("HATCHZIP_CRASH_AT", "after-extract"),
        ],
    );
    assert!(!crashed.status.success());
    let j = journal(&archive);
    assert!(j.get("resume").is_none_or(|r| r.is_null()), "{j}");
    assert_unchanged(&before);
    assert_ok(&only(&archive, &out, CHOSEN, &[]));
    assert_selected(&out, &files, CHOSEN);
    assert_unchanged(&before);
    assert_no_side_files(&archive);
}

/// A crash inside the stream, just past a file left out and after a chosen
/// one was written, leaves no resume point. The rerun deletes what the
/// first one wrote, starts the stream over and leaves only the selection,
/// with no part files, while the archive never changes.
#[test]
fn a_selection_crashed_inside_the_stream_starts_over() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let archive = targz_of(tmp.path(), "game.tar.gz", &files);
    let before = snapshot(&archive);
    let out = tmp.path().join("out");
    let crashed = only(
        &archive,
        &out,
        CHOSEN,
        &[("HATCHZIP_CRASH_AT", "stream-skipped")],
    );
    assert!(!crashed.status.success());
    // game/config sorts before game/data, the first file left out.
    assert!(
        out.join("game/config").exists(),
        "the crash came after a chosen file was written"
    );
    let j = journal(&archive);
    assert!(j.get("resume").is_none_or(|r| r.is_null()), "{j}");
    assert_unchanged(&before);
    assert_ok(&only(&archive, &out, CHOSEN, &[]));
    assert_selected(&out, &files, CHOSEN);
    assert_unchanged(&before);
    assert_no_side_files(&archive);
}

/// `--only` on an archive half extracted in place is refused with advice
/// about `--only`, not about the `--keep-archive` it implies.
#[test]
fn only_on_an_archive_extracted_in_place_is_refused() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let archive = zip_of(tmp.path(), "game.zip", &files);
    let out = tmp.path().join("out");
    let a = archive.to_str().unwrap();
    let o = out.to_str().unwrap();
    let crashed = hatchzip(
        &["extract", a, "-o", o, "--keep-free", "0", "--yes"],
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "1"),
        ],
    );
    assert!(!crashed.status.success());
    let before = snapshot(&archive);
    let refused = only(&archive, &out, &["game/config"], &[]);
    assert!(!refused.status.success());
    let err = stderr(&refused);
    assert!(
        err.contains("Resume without --only") && !err.contains("--keep-archive"),
        "{err}"
    );
    assert_unchanged(&before);
    // The run it refused to change goes on as it started.
    assert_ok(&hatchzip(
        &["extract", a, "-o", o, "--keep-free", "0", "--yes"],
        &[],
    ));
    assert!(!archive.exists());
}

/// A tar hard link is made from its extracted target, so choosing the link
/// brings the target along; a symbolic link is made as it is, pointing at
/// a file left out.
#[test]
fn links_bring_what_they_need() {
    let tmp = TempDir::new().unwrap();
    let mut b = tar::Builder::new(Vec::new());
    let data = b"the target\n";
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    b.append_data(&mut h, "game/a.txt", &data[..]).unwrap();
    let mut h = tar::Header::new_gnu();
    h.set_size(4);
    h.set_mode(0o644);
    b.append_data(&mut h, "game/other.txt", &b"left"[..])
        .unwrap();
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Link);
    h.set_size(0);
    b.append_link(&mut h, "game/hard.txt", "game/a.txt")
        .unwrap();
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_size(0);
    b.append_link(&mut h, "game/soft.txt", "other.txt").unwrap();
    let archive = tmp.path().join("links.tar");
    fs::write(&archive, b.into_inner().unwrap()).unwrap();
    let before = snapshot(&archive);
    let out = tmp.path().join("out");

    assert_ok(&only(
        &archive,
        &out,
        &["game/hard.txt", "game/soft.txt"],
        &[],
    ));
    assert_eq!(fs::read(out.join("game/hard.txt")).unwrap(), data);
    assert_eq!(fs::read(out.join("game/a.txt")).unwrap(), data);
    assert!(fs::symlink_metadata(out.join("game/soft.txt")).is_ok());
    assert!(!out.join("game/other.txt").exists());
    assert_unchanged(&before);
    assert_no_side_files(&archive);
}

#[test]
fn only_takes_one_archive() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let a = zip_of(tmp.path(), "a.zip", &files);
    let b = zip_of(tmp.path(), "b.zip", &files);
    let run = hatchzip(
        &[
            "extract",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--only",
            "game/config",
        ],
        &[],
    );
    assert!(!run.status.success());
    assert!(stderr(&run).contains("single archive"), "{}", stderr(&run));
    assert!(!tmp.path().join("a").exists() && !tmp.path().join("b").exists());
}

// -- RAR, from the fixtures rar.rs describes --------------------------------------

#[cfg(feature = "rar")]
fn copy_set(set: &str, to: &Path) -> PathBuf {
    let from = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rar")
        .join(set);
    let mut first = None;
    for e in fs::read_dir(&from).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().into_owned();
        fs::copy(e.path(), to.join(&name)).unwrap();
        if name == "set.rar" || name == "set.part1.rar" {
            first = Some(to.join(&name));
        }
    }
    first.unwrap_or_else(|| panic!("{set} has no first volume"))
}

#[cfg(feature = "rar")]
fn rar_volumes(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .map(|p| {
            let bytes = fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect();
    v.sort();
    v
}

/// A volume set, file by file, and a solid archive, decoded whole while
/// only the files chosen are written.
#[cfg(feature = "rar")]
#[test]
fn rar_sets_and_solid_archives_extract_only_the_selection() {
    let ini = b"volume=7\n".repeat(2_000);
    for (set, chosen) in [
        ("rar5", &["game/config", "game/many/file03.txt"][..]),
        ("rar4", &["game/config", "game/many/file03.txt"]),
        ("rar5-solid", &["game/config", "game/many/file03.txt"]),
    ] {
        let tmp = TempDir::new().unwrap();
        let archive = copy_set(set, tmp.path());
        let before = rar_volumes(tmp.path());
        let out = tmp.path().join("out");
        assert_ok(&only(&archive, &out, chosen, &[]));
        assert_eq!(
            fs::read(out.join("game/config/settings.ini")).unwrap(),
            ini,
            "{set}"
        );
        assert_eq!(
            fs::read(out.join("game/many/file03.txt")).unwrap(),
            b"file number 3\n",
            "{set}"
        );
        let mut found = Vec::new();
        walk_files(&out, &out, &mut found);
        found.sort();
        assert_eq!(
            found,
            ["game/config/settings.ini", "game/many/file03.txt"],
            "{set}"
        );
        assert_unchanged(&before);
        assert_no_side_files(&archive);
    }
}
