//! `extract --nested`: after the archives given, the archives found inside
//! the folders they left are extracted too, by signature and one level deep.

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

fn extract_nested(archive: &Path, out: &Path) -> std::process::Output {
    hatchzip(
        &[
            "extract",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--nested",
            "--keep-free",
            "0",
            "--yes",
        ],
        &[],
    )
}

#[test]
fn the_archives_inside_are_extracted_one_level() {
    let tmp = TempDir::new().unwrap();
    let deeper = zip_bytes(&[("deeper.txt", b"three")]);
    let first = zip_bytes(&[("first.txt", b"one"), ("deeper.zip", &deeper)]);
    let mut files = Files::new();
    files.insert("second.txt".into(), b"two".to_vec());
    let second = targz_of(tmp.path(), "second.tar.gz", &files);
    let second = fs::read(second).unwrap();
    // A `.tar.bz2` is another format the scan must recognise now that it
    // extracts in place; the fixture is a real bzip2 stream from the CLI.
    let third =
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nested/third.tar.bz2"))
            .unwrap();
    let outer = zip_bytes(&[
        ("first.zip", &first),
        ("second.tar.gz", &second),
        ("third.tar.bz2", &third),
        ("notes.zip", b"not an archive at all"),
    ]);
    let archive = tmp.path().join("outer.zip");
    fs::write(&archive, outer).unwrap();
    let out = tmp.path().join("out");

    assert_ok(&extract_nested(&archive, &out));
    assert!(!archive.exists(), "the outer archive is removed");
    assert_eq!(fs::read(out.join("first/first.txt")).unwrap(), b"one");
    assert_eq!(fs::read(out.join("second/second.txt")).unwrap(), b"two");
    assert_eq!(
        fs::read(out.join("third/third.txt")).unwrap(),
        b"three-bz\n"
    );
    assert!(
        !out.join("first.zip").exists()
            && !out.join("second.tar.gz").exists()
            && !out.join("third.tar.bz2").exists(),
        "the nested archives are removed in place"
    );
    assert!(
        out.join("notes.zip").exists(),
        "a name is not a signature, so this stays"
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

    assert_ok(&extract_nested(&archive, &out));
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
