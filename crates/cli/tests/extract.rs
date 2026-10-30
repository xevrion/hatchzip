//! End to end tests for ZIP, the reference format: build real archives, run
//! the real binary, compare what lands on disk with what went in.

mod common;

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use common::*;
use tempfile::TempDir;
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

fn build_zip(dir: &Path, files: &Files) -> PathBuf {
    build_zip_with(dir, files, false)
}

/// Each file gets a compression method in turn, so every decoder is covered.
/// `zip64` forces ZIP64 records on every entry, as archives over 4 GB have.
fn build_zip_with(dir: &Path, files: &Files, zip64: bool) -> PathBuf {
    let methods = [
        CompressionMethod::Stored,
        CompressionMethod::Deflated,
        CompressionMethod::Zstd,
        CompressionMethod::Bzip2,
    ];
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.add_directory("game/saves/", SimpleFileOptions::default())
        .unwrap();
    for (i, (name, data)) in files.iter().enumerate() {
        let opts = SimpleFileOptions::default()
            .compression_method(methods[i % methods.len()])
            .unix_permissions(0o640)
            .large_file(zip64);
        zip.start_file(name, opts).unwrap();
        zip.write_all(data).unwrap();
    }
    let path = dir.join("game.zip");
    fs::write(&path, zip.finish().unwrap().into_inner()).unwrap();
    path
}

#[test]
fn extracts_every_method_and_deletes_the_archive() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_zip(tmp.path(), &files);
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

#[test]
fn extracts_zip64_archives_and_resumes_them() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_zip_with(tmp.path(), &files, true);
    let out = tmp.path().join("out");
    let crash = [
        ("HATCHZIP_BATCH_ENTRIES", "1"),
        ("HATCHZIP_CRASH_AT", "after-journal"),
        ("HATCHZIP_CRASH_AFTER", "5"),
    ];

    assert!(!extract(&archive, &out, &crash).status.success());
    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

#[test]
fn many_small_batches_give_the_same_result() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_zip(tmp.path(), &files);
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[("HATCHZIP_BATCH_ENTRIES", "1")]));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

#[test]
fn resumes_after_a_crash_at_any_point() {
    for point in ["after-extract", "after-journal"] {
        for after in [0, 1, 7, 30] {
            let tmp = TempDir::new().unwrap();
            let files = sample_files();
            let archive = build_zip(tmp.path(), &files);
            let original = fs::metadata(&archive).unwrap().len();
            let out = tmp.path().join("out");
            let after = after.to_string();

            let crashed = extract(
                &archive,
                &out,
                &[
                    ("HATCHZIP_BATCH_ENTRIES", "1"),
                    ("HATCHZIP_CRASH_AT", point),
                    ("HATCHZIP_CRASH_AFTER", &after),
                ],
            );
            assert!(
                !crashed.status.success(),
                "{point}#{after} should have crashed"
            );
            let left = fs::metadata(&archive).unwrap().len();
            assert!(left <= original, "{point}#{after}: archive grew");
            for p in side_files(&archive) {
                assert!(p.exists(), "{point}#{after}: {} is missing", p.display());
            }

            assert_ok(&extract(&archive, &out, &[]));
            assert_tree(&out, &files);
            assert_consumed(&archive);
        }
    }
}

#[test]
fn an_output_file_that_disappears_mid_run_is_extracted_again() {
    // The second name also covers spaces, non-ASCII and a folder, in the
    // debug-only knob's value.
    for removed in ["game/data/big.pak", "game/naïve résumé/日本語.txt"] {
        let tmp = TempDir::new().unwrap();
        let files = sample_files();
        let archive = build_zip(tmp.path(), &files);
        let out = tmp.path().join("out");
        // The debug-only knob removes the file after its batch is on disk
        // and before the run checks it, as antivirus software might.
        let env = [
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_REMOVE_AFTER_EXTRACT", removed),
        ];

        let run = extract(&archive, &out, &env);
        let stderr = String::from_utf8_lossy(&run.stderr).replace('\\', "/");
        assert!(
            !run.status.success(),
            "{removed}: the run should have stopped"
        );
        assert!(stderr.contains(removed), "{removed}: {stderr}");
        assert!(stderr.contains("disappeared"), "{removed}: {stderr}");
        assert!(
            archive.exists(),
            "{removed}: the archive must not be consumed"
        );
        for p in side_files(&archive) {
            assert!(p.exists(), "{removed}: {} is missing", p.display());
        }

        assert_ok(&extract(&archive, &out, &[]));
        assert_tree(&out, &files);
        assert_consumed(&archive);
    }
}

#[test]
fn resuming_twice_in_a_row_is_harmless() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_zip(tmp.path(), &files);
    let out = tmp.path().join("out");
    let env = [
        ("HATCHZIP_BATCH_ENTRIES", "1"),
        ("HATCHZIP_CRASH_AT", "after-journal"),
    ];

    assert!(!extract(&archive, &out, &env).status.success());
    assert!(!extract(&archive, &out, &env).status.success());
    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &files);
}

#[test]
fn check_and_test_change_nothing() {
    let tmp = TempDir::new().unwrap();
    let archive = build_zip(tmp.path(), &sample_files());
    let before = fs::read(&archive).unwrap();
    let a = archive.to_str().unwrap();

    assert_ok(&hatchzip(&["check", a, "--keep-free", "0"], &[]));
    assert_ok(&hatchzip(&["test", a], &[]));
    assert_eq!(fs::read(&archive).unwrap(), before);
    for p in side_files(&archive) {
        assert!(!p.exists());
    }
}

#[test]
fn refuses_to_start_without_enough_space() {
    let tmp = TempDir::new().unwrap();
    let archive = build_zip(tmp.path(), &sample_files());
    let before = fs::read(&archive).unwrap();
    let (a, o) = (archive.to_str().unwrap(), tmp.path().join("out"));

    let run = hatchzip(
        &[
            "extract",
            a,
            "-o",
            o.to_str().unwrap(),
            "--keep-free",
            "900TB",
            "-y",
        ],
        &[],
    );
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stdout).contains("Insufficient disk space"));
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert!(!o.exists());
}

#[test]
fn extracts_next_to_unrelated_files_and_leaves_them_alone() {
    let tmp = TempDir::new().unwrap();
    let mut files = sample_files();
    let archive = build_zip(tmp.path(), &files);
    let out = tmp.path().join("out");
    fs::create_dir_all(out.join("game/data")).unwrap();
    fs::write(out.join("mine.txt"), "keep me").unwrap();
    fs::write(out.join("game/data/notes.txt"), "mine too").unwrap();

    // A crashed run deletes uncommitted output on resume; that must never
    // reach files that were there before.
    let env = [
        ("HATCHZIP_BATCH_ENTRIES", "1"),
        ("HATCHZIP_CRASH_AT", "after-extract"),
        ("HATCHZIP_CRASH_AFTER", "10"),
    ];
    assert!(!extract(&archive, &out, &env).status.success());
    assert_ok(&extract(&archive, &out, &[]));

    files.insert("mine.txt".into(), b"keep me".to_vec());
    files.insert("game/data/notes.txt".into(), b"mine too".to_vec());
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

#[test]
fn refuses_to_overwrite_an_existing_file() {
    let tmp = TempDir::new().unwrap();
    let archive = build_zip(tmp.path(), &sample_files());
    let before = fs::read(&archive).unwrap();
    let out = tmp.path().join("out");
    fs::create_dir_all(out.join("game/data")).unwrap();
    fs::write(out.join("game/data/small.pak"), "already here").unwrap();

    let run = extract(&archive, &out, &[]);
    assert!(!run.status.success());
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("Use --replace to replace existing files")
    );
    assert_eq!(
        fs::read_to_string(out.join("game/data/small.pak")).unwrap(),
        "already here"
    );
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert!(!side_files(&archive)[0].exists());
}

#[test]
fn refuses_when_a_file_sits_where_a_folder_must_go() {
    let tmp = TempDir::new().unwrap();
    let archive = build_zip(tmp.path(), &sample_files());
    let out = tmp.path().join("out");
    fs::create_dir_all(out.join("game")).unwrap();
    fs::write(out.join("game/many"), "a file, not a folder").unwrap();

    let run = extract(&archive, &out, &[]);
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("requires a folder"));
    assert!(archive.exists());
}

#[test]
fn refuses_paths_that_escape_the_output_folder() {
    let tmp = TempDir::new().unwrap();
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("../escaped.txt", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"nope").unwrap();
    let archive = tmp.path().join("evil.zip");
    fs::write(&archive, zip.finish().unwrap().into_inner()).unwrap();
    let before = fs::read(&archive).unwrap();

    let run = extract(&archive, &tmp.path().join("out"), &[]);
    assert!(!run.status.success());
    assert!(!tmp.path().join("escaped.txt").exists());
    assert_eq!(fs::read(&archive).unwrap(), before);
}

/// `dir/..` names the output folder itself. Extracted last, it failed only
/// after the rest of the archive had been consumed.
#[test]
fn refuses_a_name_that_resolves_to_the_output_folder() {
    let tmp = TempDir::new().unwrap();
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("dir/..", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"nope").unwrap();
    zip.start_file("keep.txt", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"keep").unwrap();
    let archive = tmp.path().join("evil.zip");
    fs::write(&archive, zip.finish().unwrap().into_inner()).unwrap();
    let before = fs::read(&archive).unwrap();

    let out = tmp.path().join("out");
    let run = extract(&archive, &out, &[("HATCHZIP_BATCH_ENTRIES", "1")]);
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("'dir/..'"));
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert!(!side_files(&archive)[0].exists());
}

#[test]
fn refuses_encrypted_archives_before_touching_them() {
    let tmp = TempDir::new().unwrap();
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().with_aes_encryption(zip::AesMode::Aes256, "pw");
    zip.start_file("secret.txt", opts).unwrap();
    zip.write_all(b"hidden").unwrap();
    let archive = tmp.path().join("locked.zip");
    fs::write(&archive, zip.finish().unwrap().into_inner()).unwrap();
    let before = fs::read(&archive).unwrap();

    let run = extract(&archive, &tmp.path().join("out"), &[]);
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("password"));
    assert_eq!(fs::read(&archive).unwrap(), before);
}

/// Flips a byte inside the stored data of `name`.
fn corrupt(archive: &Path, files: &Files, name: &str) {
    let mut bytes = fs::read(archive).unwrap();
    let needle = &files[name][1000..1032];
    let at = bytes
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("stored data")
        + 5;
    bytes[at] ^= 0xFF;
    fs::write(archive, bytes).unwrap();
}

#[test]
fn verify_first_catches_damage_before_anything_is_deleted() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_zip(tmp.path(), &files);
    // Sorted first in the map, so it's Stored and its bytes are findable.
    corrupt(&archive, &files, "game/config/settings.ini");
    let before = fs::read(&archive).unwrap();
    let (a, out) = (archive.to_str().unwrap(), tmp.path().join("out"));

    assert!(!hatchzip(&["test", a], &[]).status.success());
    let run = hatchzip(
        &[
            "extract",
            a,
            "-o",
            out.to_str().unwrap(),
            "--keep-free",
            "0",
            "--verify-first",
            "-y",
        ],
        &[],
    );
    assert!(!run.status.success());
    assert_eq!(fs::read(&archive).unwrap(), before);
}

#[test]
fn damage_found_midway_stops_without_losing_the_damaged_entry() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_zip(tmp.path(), &files);
    corrupt(&archive, &files, "game/config/settings.ini");
    let out = tmp.path().join("out");

    let run = extract(&archive, &out, &[("HATCHZIP_BATCH_ENTRIES", "1")]);
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("settings.ini"));
    // Everything after the damaged entry was extracted and freed; the
    // damaged one and everything before it is still in the archive.
    assert!(archive.exists());
    assert!(
        fs::read(&archive)
            .unwrap()
            .windows(9)
            .any(|w| w == b"volume=7\n")
    );
    assert!(!out.join("game/config/settings.ini").exists());
    assert!(!out.join("game/config/settings.ini.hatchzip-part").exists());
}

#[test]
fn handles_a_self_extractor_stub_in_front() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let plain = build_zip(tmp.path(), &files);
    let mut bytes = b"MZ pretend this is a self-extracting exe stub".repeat(30);
    bytes.extend(fs::read(&plain).unwrap());
    let archive = tmp.path().join("setup.exe");
    fs::write(&archive, bytes).unwrap();
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[("HATCHZIP_BATCH_ENTRIES", "3")]));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

#[test]
fn a_resumed_run_stays_in_its_output_folder() {
    let tmp = TempDir::new().unwrap();
    let archive = build_zip(tmp.path(), &sample_files());
    let out = tmp.path().join("out");
    let env = [
        ("HATCHZIP_BATCH_ENTRIES", "1"),
        ("HATCHZIP_CRASH_AT", "after-journal"),
    ];
    assert!(!extract(&archive, &out, &env).status.success());

    let elsewhere = extract(&archive, &tmp.path().join("other"), &[]);
    assert!(!elsewhere.status.success());
    assert!(String::from_utf8_lossy(&elsewhere.stderr).contains("already partially extracted"));
}

#[cfg(unix)]
#[test]
fn restores_symlinks_and_permissions_but_not_setuid() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file(
        "bin/tool",
        SimpleFileOptions::default().unix_permissions(0o4755),
    )
    .unwrap();
    zip.write_all(b"#!/bin/sh\necho hi\n").unwrap();
    zip.add_symlink("bin/alias", "tool", SimpleFileOptions::default())
        .unwrap();
    let archive = tmp.path().join("tools.zip");
    fs::write(&archive, zip.finish().unwrap().into_inner()).unwrap();
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[]));
    let mode = fs::metadata(out.join("bin/tool"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o755);
    assert_eq!(
        fs::read_link(out.join("bin/alias")).unwrap(),
        Path::new("tool")
    );
}

/// Names that are fine on Linux and macOS but that Windows can't create.
#[test]
fn windows_only_names_are_refused_up_front_on_windows() {
    let tmp = TempDir::new().unwrap();
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for name in ["game/CON.txt", "game/notes."] {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(b"made on linux").unwrap();
    }
    let archive = tmp.path().join("odd.zip");
    fs::write(&archive, zip.finish().unwrap().into_inner()).unwrap();
    let before = fs::read(&archive).unwrap();
    let out = tmp.path().join("out");

    let run = extract(&archive, &out, &[]);
    if cfg!(windows) {
        assert!(!run.status.success());
        assert!(
            String::from_utf8_lossy(&run.stderr).contains("cannot be created at this location")
        );
        assert_eq!(fs::read(&archive).unwrap(), before);
        assert!(!side_files(&archive)[0].exists());
    } else {
        assert_ok(&run);
        assert_eq!(
            fs::read(out.join("game/CON.txt")).unwrap(),
            b"made on linux"
        );
    }
}

/// Game folders nest deep. Windows' old 260 character limit must not stop an
/// extraction partway, including the part files and the crash cleanup.
#[test]
fn paths_longer_than_260_characters_extract_and_resume() {
    let tmp = TempDir::new().unwrap();
    let deep = (0..12)
        .map(|i| format!("folder-number-{i:02}-with-a-long-name"))
        .collect::<Vec<_>>()
        .join("/");
    let mut files = Files::new();
    for i in 0..6 {
        files.insert(format!("{deep}/asset-{i}.bin"), noise(50_000, i));
    }
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.add_directory(format!("{EMPTY_DIR}/"), SimpleFileOptions::default())
        .unwrap();
    for (name, data) in &files {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(data).unwrap();
    }
    let archive = tmp.path().join("deep.zip");
    fs::write(&archive, zip.finish().unwrap().into_inner()).unwrap();
    let out = tmp.path().join("out");
    let longest = out.join(files.keys().next().unwrap());
    assert!(
        longest.as_os_str().len() > 300,
        "the test path must be long"
    );

    let crash = [
        ("HATCHZIP_BATCH_ENTRIES", "1"),
        ("HATCHZIP_CRASH_AT", "after-extract"),
        ("HATCHZIP_CRASH_AFTER", "2"),
    ];
    assert!(!extract(&archive, &out, &crash).status.success());
    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

/// Info-ZIP zip stores the UTC modification time in an extended timestamp
/// field next to the zone-less DOS time; that one is restored.
#[test]
fn restores_the_extended_timestamp_mtime() {
    let tmp = TempDir::new().unwrap();
    let utc: u32 = 981_173_106; // 2001-02-03 04:05:06 UTC
    let mut opts = zip::write::FullFileOptions::default().last_modified_time(
        zip::DateTime::from_date_and_time(2001, 2, 3, 9, 35, 6).unwrap(), // local time at UTC+05:30
    );
    let mut ut = vec![1u8];
    ut.extend_from_slice(&utc.to_le_bytes());
    opts.add_extra_field(0x5455, ut, false).unwrap();
    let mut w = ZipWriter::new(Cursor::new(Vec::new()));
    w.start_file("a.txt", opts).unwrap();
    w.write_all(b"a").unwrap();
    let archive = tmp.path().join("t.zip");
    fs::write(&archive, w.finish().unwrap().into_inner()).unwrap();
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[]));
    let mtime = fs::metadata(out.join("a.txt")).unwrap().modified().unwrap();
    assert_eq!(
        mtime,
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(utc.into())
    );
}

/// 7-Zip and Windows tools store the UTC modification time in an NTFS field
/// next to the zone-less DOS time; that one is restored.
#[test]
fn restores_the_ntfs_mtime() {
    let tmp = TempDir::new().unwrap();
    let utc: u64 = 981_173_106; // 2001-02-03 04:05:06 UTC
    let ticks = (utc + 11_644_473_600) * 10_000_000 + 2_500_000;
    let mut opts = zip::write::FullFileOptions::default().last_modified_time(
        zip::DateTime::from_date_and_time(2001, 2, 3, 9, 35, 6).unwrap(), // local time at UTC+05:30
    );
    let mut ntfs = vec![0u8; 4]; // reserved
    ntfs.extend_from_slice(&1u16.to_le_bytes()); // tag: file times
    ntfs.extend_from_slice(&24u16.to_le_bytes());
    for time in [ticks, 0, 0] {
        ntfs.extend_from_slice(&time.to_le_bytes());
    }
    opts.add_extra_field(0x000a, ntfs, false).unwrap();
    let mut w = ZipWriter::new(Cursor::new(Vec::new()));
    w.start_file("a.txt", opts).unwrap();
    w.write_all(b"a").unwrap();
    let archive = tmp.path().join("t.zip");
    fs::write(&archive, w.finish().unwrap().into_inner()).unwrap();
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[]));
    let mtime = fs::metadata(out.join("a.txt")).unwrap().modified().unwrap();
    assert_eq!(
        mtime,
        std::time::UNIX_EPOCH
            + std::time::Duration::from_secs(utc)
            + std::time::Duration::from_millis(250)
    );
}
