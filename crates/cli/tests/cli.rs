//! The command line contract: exit codes scripts can rely on, quiet output,
//! listing, and the generated completions and man page.

mod common;

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use common::*;
use tempfile::TempDir;
use zip::write::{SimpleFileOptions, ZipWriter};

fn small_zip(dir: &Path) -> (PathBuf, Files) {
    let files = sample_files();
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.add_directory(format!("{EMPTY_DIR}/"), SimpleFileOptions::default())
        .unwrap();
    for (name, data) in &files {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(data).unwrap();
    }
    let path = dir.join("game.zip");
    fs::write(&path, zip.finish().unwrap().into_inner()).unwrap();
    (path, files)
}

fn orphaned_zip(dir: &Path) -> PathBuf {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));

    zip.start_file("only.txt", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"orphan fixture\n").unwrap();

    let archive = dir.join("orphan.zip");
    fs::write(&archive, zip.finish().unwrap().into_inner()).unwrap();

    let out = dir.join("out");

    let crashed = hatchzip(
        &[
            "extract",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--keep-free",
            "0",
            "--yes",
        ],
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "0"),
        ],
    );

    assert!(
        !crashed.status.success(),
        "expected a crash after the final journal commit"
    );

    let journal_path = side_files(&archive)[0].clone();
    assert!(journal_path.exists());

    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(journal_path).unwrap()).unwrap();

    assert_eq!(journal["done"], true);

    archive
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn not_enough_space_exits_with_3() {
    let tmp = TempDir::new().unwrap();
    let (archive, _) = small_zip(tmp.path());
    let a = archive.to_str().unwrap();

    let check = hatchzip(&["check", a, "--keep-free", "900TB"], &[]);
    assert_eq!(check.status.code(), Some(3));
    let run = hatchzip(&["extract", a, "--keep-free", "900TB", "-y"], &[]);
    assert_eq!(run.status.code(), Some(3));
    assert!(archive.exists());
}

#[test]
fn check_json_reports_space_plan() {
    let tmp = TempDir::new().unwrap();
    let (archive, _) = small_zip(tmp.path());

    let run = hatchzip(
        &[
            "check",
            archive.to_str().unwrap(),
            "--json",
            "--keep-free",
            "0",
        ],
        &[],
    );

    assert_ok(&run);

    let document: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();

    assert_eq!(document["version"], 1);

    let archives = document["archives"].as_array().unwrap();
    assert_eq!(archives.len(), 1);

    let result = &archives[0];
    let plan = &result["space_plan"];

    assert_eq!(result["state"], "fresh");
    assert!(result["error"].is_null());

    assert!(plan["needed_bytes"].as_u64().is_some());
    assert!(plan["available_bytes"].as_u64().is_some());
    assert_eq!(plan["reserved_bytes"], 0);

    assert!(plan["fits_in_place"].is_boolean());
    assert!(plan["fits_with_archive_kept"].is_boolean());
    assert!(plan["in_place_refusal_reason"].is_null());
}

#[test]
fn check_json_reports_insufficient_space_with_exit_code_3() {
    let tmp = TempDir::new().unwrap();
    let (archive, _) = small_zip(tmp.path());

    let run = hatchzip(
        &[
            "check",
            archive.to_str().unwrap(),
            "--json",
            "--keep-free",
            "900TB",
        ],
        &[],
    );

    assert_eq!(run.status.code(), Some(3));

    let document: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();

    assert_eq!(document["version"], 1);

    let archives = document["archives"].as_array().unwrap();
    assert_eq!(archives.len(), 1);

    let result = &archives[0];
    let plan = &result["space_plan"];

    assert!(result["error"].is_null());
    assert_eq!(plan["reserved_bytes"], 900_000_000_000_000_u64);
    assert_eq!(plan["fits_in_place"], false);
    assert_eq!(plan["fits_with_archive_kept"], false);
}

#[test]
fn damage_exits_with_4() {
    let tmp = TempDir::new().unwrap();
    let (archive, files) = small_zip(tmp.path());
    let mut bytes = fs::read(&archive).unwrap();
    let needle = &files["game/data/big.pak"][5000..5032];
    let at = bytes.windows(32).position(|w| w == needle).unwrap();
    bytes[at] ^= 0xFF;
    fs::write(&archive, &bytes).unwrap();
    let a = archive.to_str().unwrap();

    assert_eq!(hatchzip(&["test", a], &[]).status.code(), Some(4));
    let run = hatchzip(&["x", a, "--keep-free", "0", "--verify-first", "-y"], &[]);
    assert_eq!(run.status.code(), Some(4));
    assert_eq!(fs::read(&archive).unwrap(), bytes);
}

#[test]
fn quiet_prints_nothing_on_success() {
    let tmp = TempDir::new().unwrap();
    let (archive, files) = small_zip(tmp.path());
    let out = tmp.path().join("out");
    let run = hatchzip(
        &[
            "-q",
            "extract",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--keep-free",
            "0",
            "-y",
        ],
        &[],
    );
    assert_ok(&run);
    assert_eq!(stdout(&run), "");
    assert_tree(&out, &files);
}

#[test]
fn refusing_to_run_unattended_without_yes_changes_nothing() {
    let tmp = TempDir::new().unwrap();
    let (archive, _) = small_zip(tmp.path());
    let before = fs::read(&archive).unwrap();
    let run = hatchzip(
        &["extract", archive.to_str().unwrap(), "--keep-free", "0"],
        &[],
    );
    assert_eq!(run.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&run.stderr).contains("--yes"));
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert!(!side_files(&archive)[0].exists());
}

#[test]
fn list_shows_every_entry_without_touching_anything() {
    let tmp = TempDir::new().unwrap();
    let (archive, files) = small_zip(tmp.path());
    let before = fs::read(&archive).unwrap();
    let run = hatchzip(&["list", archive.to_str().unwrap()], &[]);
    assert_ok(&run);
    // Paths are listed with the platform's own separator.
    let text = stdout(&run).replace('\\', "/");
    for name in files.keys() {
        assert!(
            text.contains(name.as_str()),
            "{name} missing from the listing"
        );
    }
    assert!(text.contains(&format!("{EMPTY_DIR}/")));
    assert_eq!(fs::read(&archive).unwrap(), before);
}

#[test]
fn list_json_reports_archive_metadata_and_entries() {
    let tmp = TempDir::new().unwrap();
    let (archive, _) = small_zip(tmp.path());
    let before = fs::read(&archive).unwrap();

    let run = hatchzip(&["list", archive.to_str().unwrap(), "--json"], &[]);
    assert_ok(&run);

    let document: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON: {e}\nstdout:\n{}",
            String::from_utf8_lossy(&run.stdout)
        )
    });

    assert_eq!(document["version"], 1);

    let archives = document["archives"].as_array().unwrap();
    assert_eq!(archives.len(), 1);

    let result = &archives[0];

    assert_eq!(
        result["archive_path"],
        archive.to_string_lossy().to_string()
    );
    assert_eq!(result["state"], "fresh");
    assert!(result["format"].as_str().is_some());
    assert!(result["archive_size_bytes"].as_u64().is_some());
    assert!(
        result["remaining_uncompressed_size_bytes"]
            .as_u64()
            .is_some()
    );
    assert!(result["error"].is_null());
    assert!(result.get("archive_name").is_none());

    let entries = result["entries"].as_array().unwrap();

    assert_eq!(result["entry_count"].as_u64(), Some(entries.len() as u64));

    assert!(entries.iter().any(|entry| entry["kind"] == "directory"));
    assert!(entries.iter().any(|entry| entry["kind"] == "file"));

    for entry in entries {
        for field in [
            "path",
            "kind",
            "size_bytes",
            "mode",
            "modified_at",
            "encrypted",
            "hard_link_target",
        ] {
            assert!(entry.get(field).is_some(), "missing field: {field}");
        }

        assert!(entry["path"].as_str().is_some());
        assert!(entry["size_bytes"].as_u64().is_some());
        assert!(entry["encrypted"].is_boolean());
    }

    assert_eq!(fs::read(&archive).unwrap(), before);
}

#[test]
fn list_json_emits_one_document_for_multiple_archives() {
    let tmp = TempDir::new().unwrap();

    let first_dir = tmp.path().join("first");
    let second_dir = tmp.path().join("second");

    fs::create_dir_all(&first_dir).unwrap();
    fs::create_dir_all(&second_dir).unwrap();

    let (first, _) = small_zip(&first_dir);
    let (second, _) = small_zip(&second_dir);

    let run = hatchzip(
        &[
            "list",
            first.to_str().unwrap(),
            second.to_str().unwrap(),
            "--json",
        ],
        &[],
    );

    assert_ok(&run);

    let document: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();

    assert_eq!(document["version"], 1);

    let archives = document["archives"].as_array().unwrap();

    assert_eq!(archives.len(), 2);

    assert_eq!(
        archives[0]["archive_path"],
        first.to_string_lossy().to_string()
    );

    assert_eq!(
        archives[1]["archive_path"],
        second.to_string_lossy().to_string()
    );

    for archive in archives {
        assert!(archive["error"].is_null());
        assert!(archive["entries"].is_array());
    }
}

#[test]
fn list_json_keeps_results_when_one_archive_is_unreadable() {
    let tmp = TempDir::new().unwrap();

    let (valid, _) = small_zip(tmp.path());
    let missing = tmp.path().join("missing.zip");

    let run = hatchzip(
        &[
            "list",
            valid.to_str().unwrap(),
            missing.to_str().unwrap(),
            "--json",
        ],
        &[],
    );

    assert_eq!(run.status.code(), Some(1));

    assert!(!run.stderr.is_empty());

    let document: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();

    assert_eq!(document["version"], 1);

    let archives = document["archives"].as_array().unwrap();

    assert_eq!(archives.len(), 2);

    assert!(archives[0]["error"].is_null());
    assert!(archives[0]["entries"].is_array());

    assert!(archives[1]["error"].is_string());
    assert!(archives[1]["entries"].is_null());

    assert_eq!(
        archives[1]["archive_path"],
        missing.to_string_lossy().to_string()
    );
}

#[test]
fn check_json_reports_orphaned_archive() {
    let tmp = TempDir::new().unwrap();
    let archive = orphaned_zip(tmp.path());

    let run = hatchzip(
        &[
            "check",
            archive.to_str().unwrap(),
            "--json",
            "--keep-free",
            "0",
        ],
        &[],
    );

    assert_ok(&run);

    let document: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();

    assert_eq!(document["version"], 1);

    let result = &document["archives"][0];

    assert_eq!(result["state"], "resuming");
    assert!(result["format"].as_str().is_some());
    assert!(result["archive_size_bytes"].is_null());
    assert_eq!(result["entry_count"], 0);
    assert_eq!(result["remaining_uncompressed_size_bytes"], 0);
    assert!(result["space_plan"].is_null());
    assert!(result["error"].is_null());
}

#[test]
fn list_json_reports_orphaned_archive_error() {
    let tmp = TempDir::new().unwrap();
    let archive = orphaned_zip(tmp.path());

    let run = hatchzip(&["list", archive.to_str().unwrap(), "--json"], &[]);

    assert_eq!(run.status.code(), Some(1));
    assert!(!run.stderr.is_empty());

    let document: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();

    assert_eq!(document["version"], 1);

    let result = &document["archives"][0];

    assert_eq!(result["state"], "resuming");
    assert_eq!(result["remaining_uncompressed_size_bytes"], 0);

    let entries = result["entries"].as_array().unwrap();

    assert!(entries.is_empty());
    assert_eq!(
        entries.len() as u64,
        result["entry_count"].as_u64().unwrap()
    );

    let error = result["error"].as_str().unwrap();
    assert!(error.contains("no entries remain"), "{error}");
}

#[test]
fn list_json_reports_only_remaining_entries_when_resuming() {
    let tmp = TempDir::new().unwrap();
    let (archive, _) = small_zip(tmp.path());
    let out = tmp.path().join("out");

    let crashed = hatchzip(
        &[
            "extract",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--keep-free",
            "0",
            "--yes",
        ],
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "1"),
        ],
    );

    assert!(
        !crashed.status.success(),
        "expected the extraction to crash"
    );
    assert!(
        side_files(&archive)[0].exists(),
        "resume journal should exist"
    );

    let run = hatchzip(&["list", archive.to_str().unwrap(), "--json"], &[]);
    assert_ok(&run);

    let document: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();

    assert_eq!(document["version"], 1);

    let archives = document["archives"].as_array().unwrap();
    assert_eq!(archives.len(), 1);

    let result = &archives[0];
    let entries = result["entries"].as_array().unwrap();

    assert_eq!(result["state"], "resuming");
    assert!(result["archive_size_bytes"].is_null());
    assert!(
        result["remaining_uncompressed_size_bytes"]
            .as_u64()
            .is_some()
    );
    assert_eq!(result["entry_count"].as_u64(), Some(entries.len() as u64));
    assert!(result["error"].is_null());
}

#[test]
fn check_json_reports_resumed_in_place_plan() {
    let tmp = TempDir::new().unwrap();
    let (archive, _) = small_zip(tmp.path());
    let out = tmp.path().join("out");

    let crashed = hatchzip(
        &[
            "extract",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--keep-free",
            "0",
            "--yes",
        ],
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "1"),
        ],
    );

    assert!(!crashed.status.success());
    assert!(side_files(&archive)[0].exists());

    let run = hatchzip(
        &[
            "check",
            archive.to_str().unwrap(),
            "--json",
            "--keep-free",
            "0",
        ],
        &[],
    );

    assert!(
        matches!(run.status.code(), Some(0) | Some(3)),
        "unexpected exit code: {:?}\nstderr: {}",
        run.status.code(),
        String::from_utf8_lossy(&run.stderr)
    );

    let document: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();

    let result = &document["archives"][0];
    let plan = &result["space_plan"];

    assert_eq!(result["state"], "resuming");
    assert!(result["archive_size_bytes"].is_null());
    assert!(result["error"].is_null());

    assert!(plan["fits_in_place"].is_boolean());
    assert!(plan["fits_with_archive_kept"].is_null());
    assert!(plan["in_place_refusal_reason"].is_null());
}

#[test]
fn check_json_reports_resumed_keep_archive_plan() {
    let tmp = TempDir::new().unwrap();
    let (archive, _) = small_zip(tmp.path());
    let out = tmp.path().join("out");

    let crashed = hatchzip(
        &[
            "extract",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--keep-free",
            "0",
            "--keep-archive",
            "--yes",
        ],
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "1"),
        ],
    );

    assert!(!crashed.status.success());
    assert!(side_files(&archive)[0].exists());

    let run = hatchzip(
        &[
            "check",
            archive.to_str().unwrap(),
            "--json",
            "--keep-free",
            "0",
        ],
        &[],
    );

    assert!(
        matches!(run.status.code(), Some(0) | Some(3)),
        "unexpected exit code: {:?}\nstderr: {}",
        run.status.code(),
        String::from_utf8_lossy(&run.stderr)
    );

    let document: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();

    let result = &document["archives"][0];
    let plan = &result["space_plan"];

    assert_eq!(result["state"], "resuming");
    assert!(result["archive_size_bytes"].is_null());
    assert!(result["error"].is_null());

    assert!(plan["fits_in_place"].is_null());
    assert!(plan["fits_with_archive_kept"].is_boolean());
    assert!(plan["in_place_refusal_reason"].is_null());
}

#[test]
fn check_json_emits_one_document_for_multiple_archives() {
    let tmp = TempDir::new().unwrap();

    let first_dir = tmp.path().join("first");
    let second_dir = tmp.path().join("second");

    fs::create_dir_all(&first_dir).unwrap();
    fs::create_dir_all(&second_dir).unwrap();

    let (first, _) = small_zip(&first_dir);
    let (second, _) = small_zip(&second_dir);

    let run = hatchzip(
        &[
            "check",
            first.to_str().unwrap(),
            second.to_str().unwrap(),
            "--json",
            "--keep-free",
            "0",
        ],
        &[],
    );

    assert_ok(&run);

    let document: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();

    assert_eq!(document["version"], 1);

    let archives = document["archives"].as_array().unwrap();
    assert_eq!(archives.len(), 2);

    assert_eq!(
        archives[0]["archive_path"],
        first.to_string_lossy().to_string()
    );
    assert_eq!(
        archives[1]["archive_path"],
        second.to_string_lossy().to_string()
    );

    for archive in archives {
        assert!(archive["error"].is_null());
        assert!(archive["space_plan"].is_object());
    }
}

#[test]
fn check_json_keeps_results_when_one_archive_is_unreadable() {
    let tmp = TempDir::new().unwrap();

    let (valid, _) = small_zip(tmp.path());
    let missing = tmp.path().join("missing.zip");

    let run = hatchzip(
        &[
            "check",
            valid.to_str().unwrap(),
            missing.to_str().unwrap(),
            "--json",
            "--keep-free",
            "0",
        ],
        &[],
    );

    assert_eq!(run.status.code(), Some(1));
    assert!(!run.stderr.is_empty());

    let document: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();

    assert_eq!(document["version"], 1);

    let archives = document["archives"].as_array().unwrap();
    assert_eq!(archives.len(), 2);

    assert!(archives[0]["error"].is_null());
    assert!(archives[0]["space_plan"].is_object());

    assert!(archives[1]["error"].is_string());
    assert!(archives[1]["space_plan"].is_null());

    assert_eq!(
        archives[1]["archive_path"],
        missing.to_string_lossy().to_string()
    );
}

#[test]
fn completions_exist_for_every_shell() {
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let run = hatchzip(&["completions", shell], &[]);
        assert_ok(&run);
        let script = stdout(&run);
        assert!(
            script.contains("extract") && script.contains("keep-free"),
            "{shell} script looks wrong"
        );
    }
}

#[test]
fn man_page_renders() {
    let run = hatchzip(&["man"], &[]);
    assert_ok(&run);
    let page = stdout(&run);
    assert!(page.starts_with(".ie") || page.contains(".TH"), "not roff");
    assert!(page.contains("hatchzip") && page.contains("Exit status"));
}

#[test]
fn help_and_version_work() {
    assert_ok(&hatchzip(&["--version"], &[]));
    let help = hatchzip(&["--help"], &[]);
    assert_ok(&help);
    assert!(stdout(&help).contains("Examples:"));
    assert_eq!(hatchzip(&["extract"], &[]).status.code(), Some(2));
}
