//! Password-protected archives: the right password extracts, a wrong or
//! missing one is refused before a single byte of the archive changes.

mod common;

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use common::*;
use sevenz_rust2::encoder_options::AesEncoderOptions;
use sevenz_rust2::{ArchiveEntry, ArchiveWriter, EncoderMethod, Password};
use tempfile::TempDir;
use zip::unstable::write::FileOptionsExt;
use zip::write::{SimpleFileOptions, ZipWriter};
use zip::{AesMode, CompressionMethod};

const PW: &str = "correct horse";

enum Crypto {
    Aes,
    /// The original ZIP encryption, still what most tools write by default.
    Legacy,
    /// Only every other file is encrypted, as some tools produce.
    Mixed,
}

fn build_zip(dir: &Path, files: &Files, crypto: Crypto) -> PathBuf {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.add_directory(format!("{EMPTY_DIR}/"), SimpleFileOptions::default())
        .unwrap();
    for (i, (name, data)) in files.iter().enumerate() {
        let base = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let opts = match crypto {
            Crypto::Aes => base.with_aes_encryption(AesMode::Aes256, PW),
            Crypto::Legacy => base.with_deprecated_encryption(PW.as_bytes()).unwrap(),
            Crypto::Mixed if i % 2 == 0 => base.with_aes_encryption(AesMode::Aes256, PW),
            Crypto::Mixed => base,
        };
        zip.start_file(name, opts).unwrap();
        zip.write_all(data).unwrap();
    }
    let path = dir.join("locked.zip");
    fs::write(&path, zip.finish().unwrap().into_inner()).unwrap();
    path
}

/// AES on the data and on the header, so even the file list is secret.
fn build_7z(dir: &Path, files: &Files) -> PathBuf {
    let path = dir.join("locked.7z");
    let mut w = ArchiveWriter::create(&path).unwrap();
    w.set_content_methods(vec![
        AesEncoderOptions::new(Password::new(PW)).into(),
        EncoderMethod::LZMA2.into(),
    ]);
    w.set_encrypt_header(true);
    w.push_archive_entry::<&[u8]>(ArchiveEntry::new_directory(EMPTY_DIR), None)
        .unwrap();
    for (name, data) in files {
        w.push_archive_entry(ArchiveEntry::new_file(name), Some(data.as_slice()))
            .unwrap();
    }
    w.finish().unwrap();
    path
}

fn extract_with(
    archive: &Path,
    out: &Path,
    password: Option<&str>,
    env: &[(&str, &str)],
) -> std::process::Output {
    let (a, o) = (archive.to_str().unwrap(), out.to_str().unwrap());
    let mut args = vec!["extract", a, "-o", o, "--keep-free", "0", "--yes"];
    if let Some(p) = password {
        args.extend(["--password", p]);
    }
    hatchzip(&args, env)
}

fn assert_untouched(archive: &Path, before: &[u8]) {
    assert_eq!(fs::read(archive).unwrap(), before, "the archive changed");
    for p in side_files(archive) {
        assert!(!p.exists(), "{} should not exist", p.display());
    }
}

#[test]
fn aes_zip_extracts_with_the_right_password() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_zip(tmp.path(), &files, Crypto::Aes);
    let out = tmp.path().join("out");

    assert_ok(&extract_with(&archive, &out, Some(PW), &[]));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

/// A native split ZIP of AES entries: their 0x9901 fields and AE-2 CRCs of
/// 0 pass the checks against the local headers, and a run killed after
/// some commits resumes with the password given again.
#[test]
fn aes_native_split_zip_extracts_and_resumes() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let whole = build_zip(tmp.path(), &files, Crypto::Aes);
    let bytes = fs::read(&whole).unwrap();
    fs::remove_file(&whole).unwrap();
    let segments = split_zip_segments(&bytes, &Split::default());
    let first = write_split_zip(tmp.path(), "locked", &segments, false);
    let out = tmp.path().join("out");

    let crashed = extract_with(
        &first,
        &out,
        Some(PW),
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "8"),
        ],
    );
    assert!(!crashed.status.success(), "should have crashed");
    assert_ok(&extract_with(&first, &out, Some(PW), &[]));
    assert_tree(&out, &files);
    assert!(split_zip_parts(&first).is_empty(), "segments left");
}

#[test]
fn zipcrypto_zip_extracts_with_the_password_from_the_environment() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_zip(tmp.path(), &files, Crypto::Legacy);
    let out = tmp.path().join("out");

    assert_ok(&extract_with(
        &archive,
        &out,
        None,
        &[("HATCHZIP_PASSWORD", PW)],
    ));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

#[test]
fn partly_encrypted_zip_extracts() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_zip(tmp.path(), &files, Crypto::Mixed);
    let out = tmp.path().join("out");

    assert_ok(&extract_with(&archive, &out, Some(PW), &[]));
    assert_tree(&out, &files);
}

#[test]
fn wrong_password_changes_nothing() {
    for crypto in [Crypto::Aes, Crypto::Legacy] {
        let tmp = TempDir::new().unwrap();
        let archive = build_zip(tmp.path(), &sample_files(), crypto);
        let before = fs::read(&archive).unwrap();
        let out = tmp.path().join("out");

        let run = extract_with(&archive, &out, Some("not it"), &[]);
        assert_eq!(run.status.code(), Some(5));
        assert!(String::from_utf8_lossy(&run.stderr).contains("password"));
        assert_untouched(&archive, &before);
        assert!(!out.exists());
    }
}

#[test]
fn missing_password_without_a_terminal_says_how_to_give_one() {
    let tmp = TempDir::new().unwrap();
    let archive = build_zip(tmp.path(), &sample_files(), Crypto::Aes);
    let before = fs::read(&archive).unwrap();

    let run = extract_with(&archive, &tmp.path().join("out"), None, &[]);
    assert_eq!(run.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&run.stderr).contains("--password"));
    assert_untouched(&archive, &before);
}

#[test]
fn list_json_reports_encrypted_archive_without_a_terminal() {
    let tmp = TempDir::new().unwrap();
    let archive = build_zip(tmp.path(), &sample_files(), Crypto::Aes);
    let before = fs::read(&archive).unwrap();

    let run = hatchzip(&["list", archive.to_str().unwrap(), "--json"], &[]);

    assert_eq!(run.status.code(), Some(5));
    assert!(!run.stderr.is_empty());

    let document: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();

    assert_eq!(document["version"], 1);

    let archives = document["archives"].as_array().unwrap();
    assert_eq!(archives.len(), 1);

    let result = &archives[0];

    assert_eq!(
        result["archive_path"],
        archive.to_string_lossy().to_string()
    );
    assert!(result["entries"].is_null());

    let error = result["error"].as_str().unwrap();
    assert!(error.contains("password"), "{error}");

    assert_eq!(fs::read(&archive).unwrap(), before);
}

#[test]
fn encrypted_7z_needs_the_password_even_to_list() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_7z(tmp.path(), &files);
    let a = archive.to_str().unwrap();

    assert_eq!(hatchzip(&["list", a], &[]).status.code(), Some(5));
    assert_eq!(
        hatchzip(&["list", a, "--password", "nope"], &[])
            .status
            .code(),
        Some(5)
    );
    let listed = hatchzip(&["list", a, "--password", PW], &[]);
    assert_ok(&listed);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("big.pak"));

    let out = tmp.path().join("out");
    assert_ok(&extract_with(&archive, &out, Some(PW), &[]));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

#[test]
fn a_resumed_encrypted_run_needs_the_password_again() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_zip(tmp.path(), &files, Crypto::Aes);
    let out = tmp.path().join("out");
    let crash = [
        ("HATCHZIP_BATCH_ENTRIES", "1"),
        ("HATCHZIP_CRASH_AT", "after-journal"),
        ("HATCHZIP_CRASH_AFTER", "7"),
    ];
    assert!(
        !extract_with(&archive, &out, Some(PW), &crash)
            .status
            .success()
    );

    // The password was never written down.
    let journal = fs::read_to_string(&side_files(&archive)[0]).unwrap();
    assert!(!journal.contains(PW));
    assert_eq!(
        extract_with(&archive, &out, None, &[]).status.code(),
        Some(5)
    );

    assert_ok(&extract_with(&archive, &out, Some(PW), &[]));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

#[test]
fn test_command_checks_encrypted_data() {
    let tmp = TempDir::new().unwrap();
    let archive = build_zip(tmp.path(), &sample_files(), Crypto::Aes);
    let a = archive.to_str().unwrap();
    assert_ok(&hatchzip(&["test", a, "--password", PW], &[]));
    assert_eq!(
        hatchzip(&["test", a, "--password", "nope"], &[])
            .status
            .code(),
        Some(5)
    );
}
