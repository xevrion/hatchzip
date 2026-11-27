//! `extract --nested`: after the archives given, the archives among the files
//! they wrote are extracted too, by name and signature, one level deep.

mod common;

use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;

use common::*;
use tempfile::TempDir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

fn zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, data) in files {
        zip.start_file(*name, SimpleFileOptions::default()).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/nested")
            .join(name),
    )
    .unwrap()
}

fn extract_nested(archive: &Path, out: &Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec![
        "extract",
        archive.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--nested",
        "--keep-free",
        "0",
        "--yes",
    ];
    args.extend_from_slice(extra);
    hatchzip(&args, &[])
}

#[test]
fn the_archives_inside_are_extracted_one_level() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("out");
    fs::create_dir_all(&out).unwrap();
    // An archive the person already had in the destination, and one the
    // archive itself carries under an extension that is not an archive.
    let existing = zip_bytes(&[("user.txt", b"mine")]);
    fs::write(out.join("existing.zip"), &existing).unwrap();
    let mods = zip_bytes(&[("mod.class", b"x")]);
    let disguised = zip_bytes(&[("payload.txt", b"inside")]);

    let deeper = zip_bytes(&[("deeper.txt", b"three")]);
    let first = zip_bytes(&[("first.txt", b"one"), ("deeper.zip", &deeper)]);
    let mut files = Files::new();
    files.insert("second.txt".into(), b"two".to_vec());
    let second = targz_of(tmp.path(), "second.tar.gz", &files);
    let second = fs::read(second).unwrap();
    let mut mods_files = Files::new();
    mods_files.insert("mods/inner.txt".into(), b"zstd".to_vec());
    let nested_zst = tarzst_of(tmp.path(), "mods.tar.zst", &mods_files);
    let nested_zst = fs::read(nested_zst).unwrap();
    let outer = zip_bytes(&[
        ("first.zip", &first),
        ("second.tar.gz", &second),
        ("third.tar.bz2", &fixture("third.tar.bz2")),
        ("mods.tar.zst", &nested_zst),
        ("fake.tar.xz", &fixture("fake.tar.xz")),
        ("notes.zip", b"not an archive at all"),
        ("patch.dat", &disguised),
        ("mod.jar", &mods),
    ]);
    let archive = tmp.path().join("outer.zip");
    fs::write(&archive, outer).unwrap();

    assert_ok(&extract_nested(&archive, &out, &[]));
    assert!(!archive.exists(), "the outer archive is removed");
    assert_eq!(fs::read(out.join("first/first.txt")).unwrap(), b"one");
    assert_eq!(fs::read(out.join("second/second.txt")).unwrap(), b"two");
    assert_eq!(
        fs::read(out.join("third/third.txt")).unwrap(),
        b"three-bz\n"
    );
    assert_eq!(fs::read(out.join("mods/mods/inner.txt")).unwrap(), b"zstd");
    assert!(
        !out.join("first.zip").exists()
            && !out.join("second.tar.gz").exists()
            && !out.join("third.tar.bz2").exists()
            && !out.join("mods.tar.zst").exists(),
        "the nested archives are removed in place"
    );
    assert!(
        out.join("notes.zip").exists() && out.join("fake.tar.xz").exists(),
        "a name alone is not enough"
    );
    assert!(
        out.join("patch.dat").exists() && out.join("mod.jar").exists(),
        "only archive extensions are considered, not every ZIP"
    );
    assert!(
        out.join("existing.zip").exists() && !out.join("existing").exists(),
        "only what this run wrote is a candidate"
    );
    assert_eq!(
        fs::read(out.join("existing.zip")).unwrap(),
        existing,
        "the file that was already there is untouched"
    );
    assert!(
        out.join("first/deeper.zip").exists() && !out.join("first/deeper").exists(),
        "one level only"
    );
}

#[test]
fn without_the_flag_the_archives_stay_where_they_landed() {
    let tmp = TempDir::new().unwrap();
    let inner = zip_bytes(&[("first.txt", b"one")]);
    let outer = zip_bytes(&[("first.zip", &inner)]);
    let archive = tmp.path().join("outer.zip");
    fs::write(&archive, outer).unwrap();
    let out = tmp.path().join("out");

    let run = hatchzip(
        &[
            "extract",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--keep-free",
            "0",
            "--yes",
        ],
        &[],
    );
    assert_ok(&run);
    assert!(out.join("first.zip").exists());
    assert!(!out.join("first").exists());
}

/// With `--skip-existing`, the file at a nested archive's path is the
/// person's, not the one the run wrote, so it is not a candidate.
#[test]
fn a_skipped_file_is_not_extracted() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("out");
    fs::create_dir_all(&out).unwrap();
    let user = zip_bytes(&[("user.txt", b"mine")]);
    fs::write(out.join("first.zip"), &user).unwrap();

    let inner = zip_bytes(&[("first.txt", b"one")]);
    let outer = zip_bytes(&[("first.zip", &inner)]);
    let archive = tmp.path().join("outer.zip");
    fs::write(&archive, outer).unwrap();

    let run = hatchzip(
        &[
            "extract",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--nested",
            "--skip-existing",
            "--keep-free",
            "0",
            "--yes",
        ],
        &[],
    );
    assert_ok(&run);
    assert!(!out.join("first").exists(), "nothing was extracted");
    assert_eq!(fs::read(out.join("first.zip")).unwrap(), user);
}

/// A `.zip` this run wrote next to the person's own `.z01` names a split set
/// whose first part is theirs. Extracting it in place would remove their
/// file, so it is not offered.
#[test]
fn a_set_whose_first_part_was_already_there_is_not_extracted() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("out");
    fs::create_dir_all(&out).unwrap();
    let theirs = b"the first part of someone else's set".to_vec();
    fs::write(out.join("set.z01"), &theirs).unwrap();

    let inner = zip_bytes(&[("inside.txt", b"one")]);
    let outer = zip_bytes(&[("set.zip", &inner)]);
    let archive = tmp.path().join("outer.zip");
    fs::write(&archive, outer).unwrap();

    let run = extract_nested(&archive, &out, &[]);
    assert_ok(&run);
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("No archives among the extracted files."),
        "{}",
        String::from_utf8_lossy(&run.stdout)
    );
    assert_eq!(fs::read(out.join("set.z01")).unwrap(), theirs);
    assert_eq!(fs::read(out.join("set.zip")).unwrap(), inner);
}

fn split_files() -> Files {
    let mut files = Files::new();
    files.insert("data/big.bin".into(), noise(60_000, 5));
    files.insert("readme.txt".into(), b"split inside".to_vec());
    files
}

/// `files` as the native split ZIP `set.z01`, `set.z02`, ..., `set.zip`,
/// named and in order, with enough segments that one in the middle can go
/// missing and the set still has a `.z01` and a `.zip`.
fn split_set(files: &Files) -> Vec<(String, Vec<u8>)> {
    let tmp = TempDir::new().unwrap();
    let whole = fs::read(zip_of(tmp.path(), "whole.zip", files)).unwrap();
    let s = Split {
        size: 10_000,
        ..Split::default()
    };
    let segments = split_zip_segments(&whole, &s);
    let n = segments.len();
    assert!(n >= 4, "only {n} segments");
    segments
        .into_iter()
        .enumerate()
        .map(|(i, bytes)| {
            let name = split_zip_name(Path::new(""), "set", i, n, false);
            (name.to_str().unwrap().to_string(), bytes)
        })
        .collect()
}

fn outer_of(parts: &[&(String, Vec<u8>)]) -> Vec<u8> {
    let entries: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    zip_bytes(&entries)
}

/// Every one of `parts` is in `out`, as it was written.
fn assert_parts(out: &Path, parts: &[&(String, Vec<u8>)]) {
    for (name, bytes) in parts {
        assert_eq!(&fs::read(out.join(name)).unwrap(), bytes, "{name}");
    }
}

fn stdout_of(run: &std::process::Output) -> String {
    String::from_utf8_lossy(&run.stdout).into_owned()
}

/// A native split ZIP the run wrote whole is one archive, found by its
/// `.z01`, whose split marker is no ZIP signature, and extracted as a set
/// in place: every segment is gone afterwards.
#[test]
fn a_split_zip_inside_is_extracted_once_as_a_set() {
    let tmp = TempDir::new().unwrap();
    let files = split_files();
    let parts = split_set(&files);
    let archive = tmp.path().join("outer.zip");
    fs::write(&archive, outer_of(&parts.iter().collect::<Vec<_>>())).unwrap();
    let out = tmp.path().join("out");

    let run = extract_nested(&archive, &out, &[]);
    assert_ok(&run);
    let stdout = stdout_of(&run);
    assert!(
        stdout.contains("Nested archive:") && stdout.contains("The nested archive was extracted."),
        "the set is offered once: {stdout}"
    );
    assert_tree(&out.join("set"), &files);
    for (name, _) in &parts {
        assert!(!out.join(name).exists(), "{name} is left");
    }
}

/// The rest of a set next to the person's own `.z01` is not the run's set:
/// extracting it in place would remove their file.
#[test]
fn a_split_zip_whose_first_segment_was_already_there_is_not_extracted() {
    let tmp = TempDir::new().unwrap();
    let parts = split_set(&split_files());
    let out = tmp.path().join("out");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join(&parts[0].0), &parts[0].1).unwrap();
    let archive = tmp.path().join("outer.zip");
    fs::write(&archive, outer_of(&parts[1..].iter().collect::<Vec<_>>())).unwrap();

    let run = extract_nested(&archive, &out, &[]);
    assert_ok(&run);
    assert!(
        stdout_of(&run).contains("No archives among the extracted files."),
        "{}",
        stdout_of(&run)
    );
    assert_parts(&out, &parts.iter().collect::<Vec<_>>());
}

/// With `--skip-existing`, a segment the person had stays in place of the
/// run's copy. It would be removed with the set, so the set is not offered.
#[test]
fn a_split_zip_with_a_skipped_segment_is_not_extracted() {
    let tmp = TempDir::new().unwrap();
    let parts = split_set(&split_files());
    let out = tmp.path().join("out");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join(&parts[1].0), &parts[1].1).unwrap();
    let archive = tmp.path().join("outer.zip");
    fs::write(&archive, outer_of(&parts.iter().collect::<Vec<_>>())).unwrap();

    let run = extract_nested(&archive, &out, &["--skip-existing"]);
    assert_ok(&run);
    assert!(
        stdout_of(&run).contains("No archives among the extracted files."),
        "{}",
        stdout_of(&run)
    );
    assert_parts(&out, &parts.iter().collect::<Vec<_>>());
    assert!(!out.join("set").exists(), "nothing was extracted");
}

/// `.z01`, `.z03`, ... and `.zip` without the `.z02` are a set with a hole,
/// so it is not offered.
#[test]
fn a_split_zip_missing_a_middle_segment_is_not_extracted() {
    let tmp = TempDir::new().unwrap();
    let parts = split_set(&split_files());
    let left: Vec<_> = parts
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 1)
        .map(|(_, p)| p)
        .collect();
    let archive = tmp.path().join("outer.zip");
    fs::write(&archive, outer_of(&left)).unwrap();
    let out = tmp.path().join("out");

    let run = extract_nested(&archive, &out, &[]);
    assert_ok(&run);
    assert!(
        stdout_of(&run).contains("No archives among the extracted files."),
        "{}",
        stdout_of(&run)
    );
    assert_parts(&out, &left);
    assert!(!out.join(&parts[1].0).exists());
    assert!(!out.join("set").exists(), "nothing was extracted");
}

/// A `.z01` and a `.zip` look like a whole set by name, but the index says
/// there are more segments. The nested run refuses it before it changes
/// anything.
#[test]
fn a_split_zip_missing_segments_its_index_names_is_refused_intact() {
    let tmp = TempDir::new().unwrap();
    let parts = split_set(&split_files());
    let ends = [&parts[0], &parts[parts.len() - 1]];
    let archive = tmp.path().join("outer.zip");
    fs::write(&archive, outer_of(&ends)).unwrap();
    let out = tmp.path().join("out");

    let run = extract_nested(&archive, &out, &[]);
    assert!(!run.status.success(), "{}", stdout_of(&run));
    let said = format!(
        "{}{}",
        stdout_of(&run),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(said.contains("is missing"), "{said}");
    assert_parts(&out, &ends);
}

/// A link inside the output can point anywhere; the scan must not follow it.
#[cfg(unix)]
#[test]
fn a_link_is_not_followed_or_extracted() {
    let tmp = TempDir::new().unwrap();
    let outside = tmp.path().join("outside.zip");
    fs::write(&outside, zip_bytes(&[("x.txt", b"x")])).unwrap();

    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    tar.append_link(&mut header, "link.zip", "../outside.zip")
        .unwrap();
    let archive = tmp.path().join("outer.tar");
    fs::write(&archive, tar.into_inner().unwrap()).unwrap();
    let out = tmp.path().join("out");

    assert_ok(&extract_nested(&archive, &out, &[]));
    let link = out.join("link.zip");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link survives"
    );
    assert!(outside.exists(), "what it points at is untouched");
    assert!(!out.join("link").exists(), "nothing was extracted");
}

/// The scan must not follow a Windows link either. An archive cannot carry
/// one, so the link is left in the output between two runs, the way a user
/// would. Windows makes links only with Developer Mode on or as admin; the
/// test steps aside where the machine cannot.
#[cfg(windows)]
#[test]
fn a_windows_link_is_not_followed_or_extracted() {
    let tmp = TempDir::new().unwrap();
    let outside = tmp.path().join("outside.zip");
    fs::write(&outside, zip_bytes(&[("x.txt", b"x")])).unwrap();

    let first = zip_bytes(&[("holder.zip", &zip_bytes(&[("y.txt", b"y")]))]);
    let archive = tmp.path().join("outer1.zip");
    fs::write(&archive, first).unwrap();
    let out = tmp.path().join("out");
    assert_ok(&extract_nested(&archive, &out, &[]));

    if std::os::windows::fs::symlink_file(&outside, out.join("link.zip")).is_err() {
        return;
    }

    let second = zip_bytes(&[("holder2.zip", &zip_bytes(&[("z.txt", b"z")]))]);
    let archive = tmp.path().join("outer2.zip");
    fs::write(&archive, second).unwrap();
    assert_ok(&extract_nested(&archive, &out, &[]));

    let link = out.join("link.zip");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link survives"
    );
    assert!(outside.exists(), "what it points at is untouched");
    assert!(!out.join("link").exists(), "nothing was extracted");
}
