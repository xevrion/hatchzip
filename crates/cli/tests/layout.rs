//! Archives whose entries can't all be created where they say: two entries
//! with one name, a link where other entries need a folder, a link other
//! entries would be written through. Each must be refused before anything is
//! touched, since links are only made once the archive is gone.

mod common;

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use common::*;
use hatchzip_core::session;
use tempfile::TempDir;
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

fn stored() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(CompressionMethod::Stored)
}

fn write(dir: &Path, name: &str, bytes: Vec<u8>) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

enum Tar<'a> {
    File(&'a str, &'a [u8]),
    Dir(&'a str),
    Symlink(&'a str, &'a str),
    HardLink(&'a str, &'a str),
}

fn tar_bytes(entries: &[Tar]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for e in entries {
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        match *e {
            Tar::File(name, data) => {
                h.set_size(data.len() as u64);
                b.append_data(&mut h, name, data).unwrap();
            }
            Tar::Dir(name) => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(0o755);
                h.set_size(0);
                b.append_data(&mut h, name, &[][..]).unwrap();
            }
            Tar::Symlink(name, target) => {
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_size(0);
                b.append_link(&mut h, name, target).unwrap();
            }
            Tar::HardLink(name, target) => {
                h.set_entry_type(tar::EntryType::Link);
                h.set_size(0);
                b.append_link(&mut h, name, target).unwrap();
            }
        }
    }
    b.into_inner().unwrap()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

/// `check` and `extract` both refuse, with and without `--keep-archive`,
/// saying `why`, and leave the archive exactly as it was, with no output
/// and no side files.
fn assert_refused(archive: &Path, why: &str) {
    let before = fs::read(archive).unwrap();
    let out = archive.with_file_name("out");
    let (a, o) = (archive.to_str().unwrap(), out.to_str().unwrap());
    let check = hatchzip(&["check", a], &[]);
    let run = extract(archive, &out, &[]);
    let keep_check = hatchzip(&["check", a, "--keep-archive"], &[]);
    let keep_run = hatchzip(
        &["extract", a, "-o", o, "--keep-free", "0", "--keep-archive"],
        &[],
    );
    for (what, o) in [
        ("check", &check),
        ("extract", &run),
        ("check --keep-archive", &keep_check),
        ("extract --keep-archive", &keep_run),
    ] {
        // Windows prints paths with backslashes.
        let stderr = String::from_utf8_lossy(&o.stderr).replace('\\', "/");
        assert!(!o.status.success(), "{what} should refuse");
        assert!(stderr.contains(why), "{what}: {stderr}");
        assert!(
            stderr.contains("refused") || stderr.contains("was not modified"),
            "{what}: {stderr}"
        );
    }
    assert_eq!(fs::read(archive).unwrap(), before, "the archive changed");
    for p in side_files(archive) {
        assert!(!p.exists(), "{} was created", p.display());
    }
    assert!(!out.exists(), "output was written");
}

/// The reporter's symlink.zip: a file below `d`, and a link at `d`.
#[test]
fn zip_link_where_a_file_needs_a_folder_is_refused() {
    let tmp = TempDir::new().unwrap();
    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file("d/inner.txt", stored()).unwrap();
    z.write_all(b"inner\n").unwrap();
    z.add_symlink("d", "elsewhere", stored()).unwrap();
    let archive = write(tmp.path(), "symlink.zip", z.finish().unwrap().into_inner());
    assert_refused(
        &archive,
        "'d' is a symbolic link, but 'd/inner.txt' requires a folder",
    );
}

/// The reporter's dircollide.zip: the folder has an entry of its own.
#[test]
fn zip_link_at_a_folder_entry_is_refused() {
    let tmp = TempDir::new().unwrap();
    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    z.add_directory("d/", stored()).unwrap();
    z.start_file("d/inner.txt", stored()).unwrap();
    z.write_all(b"inner\n").unwrap();
    z.add_symlink("d", "elsewhere", stored()).unwrap();
    let archive = write(
        tmp.path(),
        "dircollide.zip",
        z.finish().unwrap().into_inner(),
    );
    assert_refused(&archive, "'d' is both a symbolic link and a folder");
}

/// Extracts `archive` where its folder keeps names apart that differ only in
/// case. Anywhere else, such as the macOS and Windows runners, `check` and
/// `extract` refuse it untouched. Both are what the archive should get on
/// that filesystem.
fn assert_extracts_where_case_is_kept(archive: &Path, extracted: impl Fn(&Path)) {
    if session::case_sensitive(archive.parent().unwrap()) {
        let out = archive.with_file_name("out");
        assert_ok(&extract(archive, &out, &[]));
        extracted(&out);
        assert_consumed(archive);
    } else {
        assert_refused(archive, "differ only in case");
    }
}

fn case_files() -> Files {
    let mut files = Files::new();
    // Over two xz blocks, so a `.tar.xz` killed at its resume points still
    // has the other name to do.
    files.insert("game/Makefile".into(), noise(12_000, 1));
    files.insert("game/makefile".into(), b"lower\n".to_vec());
    files.insert("game/other.txt".into(), b"other\n".to_vec());
    files
}

type Build = fn(&Path, &Files) -> PathBuf;

fn case_builders() -> Vec<(&'static str, Build)> {
    vec![
        ("zip", |d, f| zip_of(d, "case.zip", f)),
        ("7z", |d, f| sevenz_of(d, "case.7z", f, false)),
        ("solid 7z", |d, f| sevenz_of(d, "case.7z", f, true)),
        ("tar", |d, f| tar_of(d, "case.tar", f)),
        ("tar.gz", |d, f| targz_of(d, "case.tar.gz", f)),
        ("tar.xz", |d, f| tarxz_of(d, "case.tar.xz", f)),
    ]
}

/// What kills a run after its second commit. A `.tar.xz` commits at resume
/// points, which come at the ends of its blocks once they are due this often.
fn crash_env(archive: &Path) -> [(&'static str, &'static str); 3] {
    let (often, at) = if archive.to_string_lossy().ends_with(".tar.xz") {
        (("HATCHZIP_STREAM_STEP", "1024"), "stream-after-journal")
    } else {
        (("HATCHZIP_BATCH_ENTRIES", "1"), "after-journal")
    };
    [
        often,
        ("HATCHZIP_CRASH_AT", at),
        ("HATCHZIP_CRASH_AFTER", "1"),
    ]
}

/// Two files in every format, as a Linux source tree has them.
#[test]
fn names_differing_only_in_case_depend_on_the_destination() {
    for (name, build) in case_builders() {
        let files = case_files();
        let tmp = TempDir::new().unwrap();
        let archive = build(tmp.path(), &files);
        eprintln!("{name}");
        assert_extracts_where_case_is_kept(&archive, |out| assert_tree(out, &files));
    }
}

/// Nothing is written, so there is no destination to ask about.
#[test]
fn listing_and_testing_names_differing_only_in_case_always_work() {
    for (name, build) in case_builders() {
        let tmp = TempDir::new().unwrap();
        let archive = build(tmp.path(), &case_files());
        let a = archive.to_str().unwrap();
        for command in ["list", "test"] {
            let run = hatchzip(&[command, a], &[]);
            assert!(run.status.success(), "{name}: {command}: {run:?}");
        }
    }
}

/// A kept archive would race for one file just the same, so it gets the
/// same answer, and a run killed halfway decides the same way on resuming.
#[test]
fn keeping_and_resuming_names_differing_only_in_case() {
    let files = case_files();
    let tmp = TempDir::new().unwrap();
    if !session::case_sensitive(tmp.path()) {
        return;
    }
    for (name, build) in case_builders() {
        let tmp = TempDir::new().unwrap();
        let archive = build(tmp.path(), &files);
        let kept = tmp.path().join("kept");
        let (a, k) = (archive.to_str().unwrap(), kept.to_str().unwrap());
        let before = snapshot(&archive);
        let run = hatchzip(
            &["extract", a, "-o", k, "--keep-free", "0", "--keep-archive"],
            &[],
        );
        assert_ok(&run);
        assert_tree(&kept, &files);
        assert_unchanged(&before);

        let out = tmp.path().join("out");
        let crashed = extract(&archive, &out, &crash_env(&archive));
        // A `.tar.gz` this small is one unit, with no point to crash at.
        if name != "tar.gz" {
            assert!(!crashed.status.success(), "{name} should have crashed");
            assert_ok(&extract(&archive, &out, &[]));
        }
        assert_tree(&out, &files);
        assert_consumed(&archive);
    }
}

/// Every file below `dir`, for showing that a refusal left them alone.
fn files_below(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files_below(&path));
        } else {
            found.push((path.clone(), fs::read(&path).unwrap()));
        }
    }
    found.sort();
    found
}

/// Crashes a run after its first batch, then has it find a folder that may
/// ignore case, as if the output had moved to another disk. An entry left to
/// do would resolve to its twin that is already out, so a resume and a
/// restore must both stop before they touch anything, and a tar no longer
/// lists that twin, so only the journal can say so. Back on a disk that keeps
/// case, the run finishes.
fn assert_refused_where_case_may_be_ignored(archive: &Path, finished: impl Fn(&Path)) {
    assert_refused_where_case_may_be_ignored_from(archive, 1, finished);
}

/// `check` exits with `check_status`: 0 when only the cleanup is left, since
/// there is nothing to plan or list, though a run is still refused.
fn assert_refused_where_case_may_be_ignored_from(
    archive: &Path,
    check_status: i32,
    finished: impl Fn(&Path),
) {
    let out = archive.with_file_name("out");
    let crashed = extract(archive, &out, &crash_env(archive));
    assert!(!crashed.status.success(), "it should have crashed");
    let (before, written) = (snapshot(archive), files_below(&out));
    let (a, o) = (archive.to_str().unwrap(), out.to_str().unwrap());
    let ignore = [("HATCHZIP_IGNORE_CASE", "1")];
    let check = hatchzip(&["check", a], &ignore);
    assert_eq!(check.status.code(), Some(check_status), "{check:?}");
    for run in [
        extract(archive, &out, &ignore),
        hatchzip(&["restore", a, "-o", o], &ignore),
    ] {
        assert_eq!(run.status.code(), Some(1), "{run:?}");
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(stderr.contains("differ only in case"), "{stderr}");
        assert_unchanged(&before);
        assert_eq!(files_below(&out), written);
    }
    // Only looking stays possible, while there is something to look at.
    if check_status != 0 {
        assert_ok(&hatchzip(&["list", a], &ignore));
    }
    assert_ok(&extract(archive, &out, &[]));
    finished(&out);
    assert_consumed(archive);
}

#[test]
fn a_resume_or_restore_where_case_may_be_ignored_is_refused() {
    let files = case_files();
    let tmp = TempDir::new().unwrap();
    if !session::case_sensitive(tmp.path()) {
        return;
    }
    for (name, build) in case_builders() {
        // A `.tar.gz` this small has no point to crash at.
        if name != "tar.gz" {
            let tmp = TempDir::new().unwrap();
            let archive = build(tmp.path(), &files);
            eprintln!("{name}");
            assert_refused_where_case_may_be_ignored(&archive, |out| assert_tree(out, &files));
        }
    }
}

/// A file and a folder clash too, and only a fresh archive lists both. With
/// two entries the crash leaves only the cleanup to do, with no archive to
/// list anything, and with a third there is a resume.
#[test]
fn a_run_with_a_file_differing_only_in_case_from_a_folder_is_refused() {
    let tmp = TempDir::new().unwrap();
    if !session::case_sensitive(tmp.path()) {
        return;
    }
    let (first, second) = (
        Tar::File("D", b"file\n"),
        Tar::File("d/inner.txt", b"inner\n"),
    );
    for (bytes, check_status) in [
        (tar_bytes(&[first, second]), 0),
        (
            tar_bytes(&[
                Tar::File("before.txt", b"before\n"),
                Tar::File("D", b"file\n"),
                Tar::File("d/inner.txt", b"inner\n"),
            ]),
            1,
        ),
    ] {
        let tmp = TempDir::new().unwrap();
        let archive = write(tmp.path(), "case.tar", bytes);
        assert_refused_where_case_may_be_ignored_from(&archive, check_status, |out| {
            assert_eq!(fs::read(out.join("D")).unwrap(), b"file\n");
        });
    }
}

fn kelvin_files() -> Files {
    let mut files = Files::new();
    files.insert("K.txt".into(), b"ascii\n".to_vec());
    files.insert("\u{212a}.txt".into(), b"kelvin\n".to_vec());
    files
}

/// U+212A KELVIN SIGN lowercases to `k`, and where the system normalizes
/// names it is `K`, whatever the filesystem says about case.
#[test]
fn names_outside_ascii_are_refused_where_the_system_normalizes_names() {
    let tmp = TempDir::new().unwrap();
    let archive = zip_of(tmp.path(), "case.zip", &kelvin_files());
    let before = snapshot(&archive);
    let run = extract(
        &archive,
        &tmp.path().join("out"),
        &[("HATCHZIP_NORMALIZE_NAMES", "1")],
    );
    assert_eq!(run.status.code(), Some(1), "{run:?}");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("outside ASCII"), "{stderr}");
    assert_unchanged(&before);
}

/// A run can start where names aren't normalized and go on where they are,
/// so it records that it has such names. A system that normalizes them never
/// starts one, so there this has nothing to resume.
#[test]
fn a_run_with_names_outside_ascii_stops_where_the_system_normalizes_names() {
    let mut files = kelvin_files();
    // Sorts between the two, so a run that crashes has both kinds left to do.
    files.insert("a.txt".into(), b"other\n".to_vec());
    let tmp = TempDir::new().unwrap();
    if !session::case_sensitive(tmp.path()) {
        return;
    }
    let archive = tar_of(tmp.path(), "case.tar", &files);
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
    if !side_files(&archive)[0].exists() {
        assert!(!crashed.status.success());
        return;
    }
    let (before, written) = (snapshot(&archive), files_below(&out));
    let (a, o) = (archive.to_str().unwrap(), out.to_str().unwrap());
    let normalize = [("HATCHZIP_NORMALIZE_NAMES", "1")];
    for run in [
        hatchzip(&["check", a], &normalize),
        extract(&archive, &out, &normalize),
        hatchzip(&["restore", a, "-o", o], &normalize),
    ] {
        assert_eq!(run.status.code(), Some(1), "{run:?}");
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(stderr.contains("outside ASCII"), "{stderr}");
        assert_unchanged(&before);
        assert_eq!(files_below(&out), written);
    }
    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &files);
}

/// A resume asks about the recorded folders before anything else, and a FIFO
/// at one would keep a plain open waiting for a writer forever.
#[cfg(unix)]
#[test]
fn a_fifo_where_a_folder_was_recorded_cannot_hang_a_resume() {
    let tmp = TempDir::new().unwrap();
    if !session::case_sensitive(tmp.path()) {
        return;
    }
    let archive = zip_of(tmp.path(), "case.zip", &case_files());
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
    let recorded = out.join("game");
    fs::rename(&recorded, tmp.path().join("aside")).unwrap();
    let made = std::process::Command::new("mkfifo").arg(&recorded).status();
    assert!(made.unwrap().success());
    let mut run = std::process::Command::new(env!("CARGO_BIN_EXE_hatchzip"))
        .args(["extract", "--keep-free", "0", "--yes", "-o"])
        .args([&out, &archive])
        .env("HATCHZIP_STATE_DIR", state_dir())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if let Some(status) = run.try_wait().unwrap() {
            assert!(!status.success());
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    run.kill().unwrap();
    panic!("the run hung on the FIFO");
}

/// The journal lists these folders and is rewritten at every commit, so
/// their number has a limit. Nothing is written by looking.
#[test]
fn too_many_folders_with_names_differing_only_in_case_are_refused() {
    let tmp = TempDir::new().unwrap();
    let names: Vec<(String, String)> = (0..1001)
        .map(|i| (format!("d{i}/A"), format!("d{i}/a")))
        .collect();
    let entries: Vec<Tar> = names
        .iter()
        .flat_map(|(a, b)| [Tar::File(a, b"a\n"), Tar::File(b, b"b\n")])
        .collect();
    let archive = write(tmp.path(), "case.tar", tar_bytes(&entries));
    assert_ok(&hatchzip(&["list", archive.to_str().unwrap()], &[]));
    assert_refused(&archive, "more than HatchZip can keep track of");
}

/// An older build never asks, so it must refuse to touch such a run.
#[test]
fn a_run_with_names_differing_only_in_case_records_their_folders() {
    let tmp = TempDir::new().unwrap();
    if !session::case_sensitive(tmp.path()) {
        return;
    }
    let archive = zip_of(tmp.path(), "case.zip", &case_files());
    let crashed = extract(
        &archive,
        &tmp.path().join("out"),
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "1"),
        ],
    );
    assert!(!crashed.status.success());
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(&side_files(&archive)[0]).unwrap()).unwrap();
    assert_eq!(journal["version"], 7);
    assert_eq!(journal["case_folders"], serde_json::json!(["game"]));
}

/// A case-insensitive filesystem would put both in one place: a link where
/// the other entry needs a folder.
#[test]
fn zip_link_differing_only_in_case_depends_on_the_destination() {
    let tmp = TempDir::new().unwrap();
    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file("D/inner.txt", stored()).unwrap();
    z.write_all(b"inner\n").unwrap();
    z.add_symlink("d", "elsewhere", stored()).unwrap();
    let archive = write(tmp.path(), "case.zip", z.finish().unwrap().into_inner());
    assert_extracts_where_case_is_kept(&archive, |out| {
        assert_eq!(fs::read(out.join("D/inner.txt")).unwrap(), b"inner\n");
        assert!(fs::symlink_metadata(out.join("d")).unwrap().is_symlink());
    });
}

/// The same with a file where the other entry needs a folder.
#[test]
fn tar_file_differing_only_in_case_from_a_folder_depends_on_the_destination() {
    let tmp = TempDir::new().unwrap();
    let bytes = tar_bytes(&[
        Tar::File("D", b"file\n"),
        Tar::File("d/inner.txt", b"inner\n"),
    ]);
    let archive = write(tmp.path(), "case.tar", bytes);
    assert_extracts_where_case_is_kept(&archive, |out| {
        assert_eq!(fs::read(out.join("D")).unwrap(), b"file\n");
        assert_eq!(fs::read(out.join("d/inner.txt")).unwrap(), b"inner\n");
    });
}

/// Folders that differ only in case merge on a filesystem that ignores case,
/// which is fine while no file inside them clashes, so this always extracts.
#[test]
fn folders_differing_only_in_case_extract_everywhere() {
    let tmp = TempDir::new().unwrap();
    let bytes = tar_bytes(&[Tar::File("D/a.txt", b"a\n"), Tar::File("d/b.txt", b"b\n")]);
    let archive = write(tmp.path(), "case.tar", bytes);
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &[]));
    assert_eq!(fs::read(out.join("D/a.txt")).unwrap(), b"a\n");
    assert_eq!(fs::read(out.join("d/b.txt")).unwrap(), b"b\n");
    assert_consumed(&archive);
}

/// Case or no case, the same path twice is refused everywhere.
#[test]
fn tar_duplicate_names_are_refused() {
    let tmp = TempDir::new().unwrap();
    let bytes = tar_bytes(&[
        Tar::File("Makefile", b"one\n"),
        Tar::File("makefile", b"two\n"),
        Tar::File("Makefile", b"three\n"),
    ]);
    let archive = write(tmp.path(), "dup.tar", bytes);
    assert_refused(&archive, "'Makefile' appears more than once in the archive");
}

/// The reporter's symlink.tar.
#[test]
fn tar_symlink_where_a_file_needs_a_folder_is_refused() {
    let tmp = TempDir::new().unwrap();
    let bytes = tar_bytes(&[
        Tar::File("d/inner.txt", b"inner\n"),
        Tar::Symlink("d", "elsewhere"),
    ]);
    let archive = write(tmp.path(), "symlink.tar", bytes.clone());
    assert_refused(
        &archive,
        "'d' is a symbolic link, but 'd/inner.txt' requires a folder",
    );
    let archive = write(tmp.path(), "symlink.tar.gz", gzip(&bytes));
    assert_refused(
        &archive,
        "'d' is a symbolic link, but 'd/inner.txt' requires a folder",
    );
}

#[test]
fn tar_hard_link_where_a_file_needs_a_folder_is_refused() {
    let tmp = TempDir::new().unwrap();
    let bytes = tar_bytes(&[
        Tar::File("a.txt", b"a\n"),
        Tar::File("d/inner.txt", b"inner\n"),
        Tar::HardLink("d", "a.txt"),
    ]);
    let archive = write(tmp.path(), "hardlink.tar", bytes.clone());
    assert_refused(
        &archive,
        "'d' is a hard link, but 'd/inner.txt' requires a folder",
    );
    let archive = write(tmp.path(), "hardlink.tar.gz", gzip(&bytes));
    assert_refused(
        &archive,
        "'d' is a hard link, but 'd/inner.txt' requires a folder",
    );
}

#[test]
fn tar_link_at_a_folder_entry_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = write(
        tmp.path(),
        "dir.tar",
        tar_bytes(&[Tar::Dir("d"), Tar::Symlink("d", "elsewhere")]),
    );
    assert_refused(&archive, "'d' is both a symbolic link and a folder");
}

#[test]
fn tar_file_where_another_file_needs_a_folder_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = write(
        tmp.path(),
        "file.tar",
        tar_bytes(&[
            Tar::File("d", b"plain\n"),
            Tar::File("d/inner.txt", b"inner\n"),
        ]),
    );
    assert_refused(
        &archive,
        "'d' is a file, but 'd/inner.txt' requires a folder",
    );
}

/// Made in order, `a/b` would be created inside whatever `a` points to.
#[test]
fn tar_link_inside_a_link_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = write(
        tmp.path(),
        "through.tar",
        tar_bytes(&[Tar::Symlink("a", "/tmp"), Tar::Symlink("a/b", "x")]),
    );
    assert_refused(
        &archive,
        "'a' is a symbolic link, but 'a/b' requires a folder",
    );
}

#[test]
fn tar_hard_link_to_anything_but_a_file_in_the_archive_is_refused() {
    let tmp = TempDir::new().unwrap();
    for (name, entries) in [
        (
            "to-folder.tar",
            vec![
                Tar::File("d/inner.txt", b"inner\n"),
                Tar::HardLink("x", "d"),
            ],
        ),
        ("to-missing.tar", vec![Tar::HardLink("x", "missing.txt")]),
        (
            "to-link.tar",
            vec![
                Tar::File("a.txt", b"a\n"),
                Tar::Symlink("s", "a.txt"),
                Tar::HardLink("x", "s"),
            ],
        ),
    ] {
        let archive = write(tmp.path(), name, tar_bytes(&entries));
        assert_refused(&archive, "which is not a file in the archive");
    }
}

/// Links that don't collide with anything still extract.
#[cfg(unix)]
#[test]
fn links_next_to_folders_still_extract() {
    let tmp = TempDir::new().unwrap();
    let archive = write(
        tmp.path(),
        "fine.tar",
        tar_bytes(&[
            Tar::File("d/inner.txt", b"inner\n"),
            Tar::Symlink("e", "d"),
            Tar::HardLink("d/hard.txt", "d/inner.txt"),
            Tar::Symlink("d/soft", "inner.txt"),
        ]),
    );
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &[("HATCHZIP_BATCH_ENTRIES", "1")]));
    assert_eq!(fs::read(out.join("e/inner.txt")).unwrap(), b"inner\n");
    assert_eq!(fs::read(out.join("d/hard.txt")).unwrap(), b"inner\n");
    assert_eq!(fs::read(out.join("d/soft")).unwrap(), b"inner\n");
    assert_consumed(&archive);
}

/// The reporter's collision.zip: two entries named `dup.txt`. The zip crate
/// keys entries by name and keeps the last, so this is checked on the raw
/// central directory.
fn duplicate_zip() -> Vec<u8> {
    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file("dup.txt", stored()).unwrap();
    z.write_all(b"one").unwrap();
    z.start_file("dux.txt", stored()).unwrap();
    z.write_all(b"two").unwrap();
    let mut bytes = z.finish().unwrap().into_inner();
    // The writer refuses a second `dup.txt`, so the name is patched in, in
    // the local header and the central directory alike.
    let mut patched = 0;
    for i in 0..bytes.len() - 7 {
        if &bytes[i..i + 7] == b"dux.txt" {
            bytes[i..i + 7].copy_from_slice(b"dup.txt");
            patched += 1;
        }
    }
    assert_eq!(patched, 2);
    bytes
}

#[test]
fn zip_duplicate_names_are_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = write(tmp.path(), "collision.zip", duplicate_zip());
    let list = hatchzip(&["list", archive.to_str().unwrap()], &[]);
    assert!(!list.status.success());
    assert_refused(&archive, "'dup.txt' appears more than once in the archive");
}

#[test]
fn zip_duplicate_names_in_a_self_extractor_are_refused() {
    let tmp = TempDir::new().unwrap();
    let mut bytes = vec![0x90; 4096];
    bytes.extend(duplicate_zip());
    let archive = write(tmp.path(), "collision.exe", bytes);
    assert_refused(&archive, "'dup.txt' appears more than once in the archive");
}

/// The journal a run left next to `archive`.
#[cfg(unix)]
fn journal(archive: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(&side_files(archive)[0]).unwrap()).unwrap()
}

/// An extraction an older release started on such an archive runs until
/// only its links are left, and then can't make them. That state must say
/// what failed and what to do, keep the links until then, and finish once
/// the way is clear; restore must not send the person back to extract in a
/// loop.
#[cfg(unix)]
#[test]
fn links_that_cannot_be_made_at_the_end_can_be_fixed_and_resumed() {
    let tmp = TempDir::new().unwrap();
    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file("a.txt", stored()).unwrap();
    z.write_all(b"a\n").unwrap();
    z.add_symlink("d", "a.txt", stored()).unwrap();
    z.add_symlink("sub/e", "a.txt", stored()).unwrap();
    let archive = write(tmp.path(), "late.zip", z.finish().unwrap().into_inner());
    let out = tmp.path().join("out");

    // Stop right after the last entry is committed, then put a folder where
    // one link goes and a link where the other one's folder goes, as an
    // archive with colliding entries would have.
    let crashed = extract(
        &archive,
        &out,
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "2"),
        ],
    );
    assert!(!crashed.status.success());
    assert_eq!(journal(&archive)["committed_len"], 0);
    fs::create_dir(out.join("d")).unwrap();
    fs::write(out.join("d/mine.txt"), b"mine\n").unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, out.join("sub")).unwrap();

    for _ in 0..2 {
        let run = extract(&archive, &out, &[]);
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(!run.status.success());
        assert!(
            stderr.contains("every file is extracted, but 2 links could not be created"),
            "{stderr}"
        );
        assert!(
            stderr.contains("'d' (symbolic link to 'a.txt'): a folder already exists"),
            "{stderr}"
        );
        assert!(
            stderr.contains("'sub' is a link, and this one would be created through it"),
            "{stderr}"
        );
        assert!(stderr.contains("resume the extraction"), "{stderr}");
        // Nothing is lost or written anywhere it shouldn't be, however
        // often it is retried.
        assert_eq!(fs::read(out.join("d/mine.txt")).unwrap(), b"mine\n");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        assert_eq!(journal(&archive)["links"].as_array().unwrap().len(), 2);
    }

    let restore = hatchzip(
        &[
            "restore",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ],
        &[],
    );
    let stderr = String::from_utf8_lossy(&restore.stderr);
    assert!(!restore.status.success());
    assert!(stderr.contains("nothing to restore"), "{stderr}");
    assert!(stderr.contains("only the links remain"), "{stderr}");

    fs::rename(out.join("d"), out.join("d-folder")).unwrap();
    fs::remove_file(out.join("sub")).unwrap();
    assert_ok(&extract(&archive, &out, &[]));
    assert_eq!(fs::read_link(out.join("d")).unwrap(), Path::new("a.txt"));
    assert!(fs::symlink_metadata(out.join("sub")).unwrap().is_dir());
    assert_eq!(
        fs::read_link(out.join("sub/e")).unwrap(),
        Path::new("a.txt")
    );
    assert_consumed(&archive);
}

/// In a run over several archives, a colliding one stops the run before it
/// is touched, after the ones before it are done.
#[test]
fn a_colliding_archive_among_several_is_refused_untouched() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let a = zip_of(tmp.path(), "a.zip", &files);
    let b = write(
        tmp.path(),
        "b.tar",
        tar_bytes(&[
            Tar::File("d/inner.txt", b"inner\n"),
            Tar::Symlink("d", "elsewhere"),
        ]),
    );
    let c = zip_of(tmp.path(), "c.zip", &files);
    let (b_before, c_before) = (snapshot(&b), snapshot(&c));
    let arg = |p: &Path| p.to_str().unwrap().to_string();

    let run = hatchzip(
        &[
            "extract",
            &arg(&a),
            &arg(&b),
            &arg(&c),
            "--keep-free",
            "0",
            "-y",
        ],
        &[],
    );
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    )
    .replace('\\', "/");
    assert!(!run.status.success(), "{out}");
    assert!(
        out.contains("'d' is a symbolic link, but 'd/inner.txt' requires a folder"),
        "{out}"
    );
    assert!(out.contains("Stopped at archive 2 of 3."), "{out}");
    assert_tree(&tmp.path().join("a"), &files);
    assert_consumed(&a);
    assert_unchanged(&b_before);
    assert_no_side_files(&b);
    assert!(!tmp.path().join("b").exists());
    assert_unchanged(&c_before);
    assert!(!tmp.path().join("c").exists());
}

/// A path the system can't create used to fail only when its turn came,
/// after the files behind it had been cut off the archive (macOS stops at
/// 1024 bytes). It is refused up front instead. Windows takes paths this
/// long through the `\\?\` form, so there it extracts.
#[test]
fn a_path_longer_than_the_system_allows_is_refused_up_front() {
    let tmp = TempDir::new().unwrap();
    let deep = format!("{}/deep.txt", vec!["d".repeat(200); 21].join("/"));
    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file(deep.as_str(), stored()).unwrap();
    z.write_all(b"deep\n").unwrap();
    for name in ["a.pak", "b.pak"] {
        z.start_file(name, stored()).unwrap();
        z.write_all(&[7u8; 30_000]).unwrap();
    }
    let archive = write(tmp.path(), "deep.zip", z.finish().unwrap().into_inner());
    if cfg!(windows) {
        assert_ok(&extract(&archive, &tmp.path().join("out"), &[]));
    } else {
        assert_refused(&archive, "longer than this system allows");
    }
}

/// Every file is first written under its name plus `.hatchzip-part`. An
/// entry named that way would share the name with another entry's file, or
/// stay in the folder as a link for a later extraction to write through.
#[test]
fn an_entry_named_like_a_part_file_is_refused() {
    let tmp = TempDir::new().unwrap();
    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file("x", stored()).unwrap();
    z.write_all(b"first\n").unwrap();
    z.start_file("x.hatchzip-part", stored()).unwrap();
    z.write_all(b"second\n").unwrap();
    let archive = write(tmp.path(), "collide.zip", z.finish().unwrap().into_inner());
    assert_refused(
        &archive,
        "'x.hatchzip-part' uses a name ending in .hatchzip-part",
    );

    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    z.add_symlink("report.pdf.hatchzip-part", "../outside.txt", stored())
        .unwrap();
    let archive = write(tmp.path(), "link.zip", z.finish().unwrap().into_inner());
    assert_refused(&archive, "'report.pdf.hatchzip-part' uses a name ending");

    // In a folder name too, and in any case.
    let bytes = tar_bytes(&[Tar::File("docs.HATCHZIP-PART/a.txt", b"a\n")]);
    let archive = write(tmp.path(), "folder.tar", bytes);
    assert_refused(&archive, "'docs.HATCHZIP-PART/a.txt' uses a name ending");
}

/// One name past 255 bytes, counting the `.hatchzip-part` it is written
/// under first, can't be created anywhere.
#[test]
fn a_name_longer_than_the_file_system_allows_is_refused_up_front() {
    let tmp = TempDir::new().unwrap();
    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file(format!("game/{}.txt", "n".repeat(240)), stored())
        .unwrap();
    z.write_all(b"long\n").unwrap();
    z.start_file("game/a.pak", stored()).unwrap();
    z.write_all(&[7u8; 30_000]).unwrap();
    let archive = write(tmp.path(), "long.zip", z.finish().unwrap().into_inner());
    assert_refused(&archive, "the name is longer than");
}
