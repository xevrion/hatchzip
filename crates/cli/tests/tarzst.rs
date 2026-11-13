//! `.tar.zst`: one zstd stream of independent frames, extracted front to
//! back while the frames behind the decoder are punched away. Killed at
//! every step of a resume point, resumed, and compared byte for byte;
//! damaged; refused in place; kept whole without hole punching.
//!
//! `HATCHZIP_STREAM_STEP` shrinks the step between resume points to 256 KiB,
//! so with 1 MiB frames nearly every frame boundary is one of them.

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use common::*;
use tempfile::TempDir;

const STEP: &str = "262144";
const CHUNK: usize = 1024 * 1024;

fn tar_bytes(files: &Files) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut dir = tar::Header::new_gnu();
    dir.set_entry_type(tar::EntryType::Directory);
    dir.set_mode(0o755);
    dir.set_size(0);
    b.append_data(&mut dir.clone(), "./", std::io::empty())
        .unwrap();
    b.append_data(&mut dir, EMPTY_DIR, std::io::empty())
        .unwrap();
    for (name, data) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(1_700_000_000);
        b.append_data(&mut h, name, data.as_slice()).unwrap();
    }
    b.into_inner().unwrap()
}

/// One independent zstd frame per chunk of the tar, each with its checksum,
/// what `pzstd` and concatenated files produce.
fn zstd(tar: &[u8], chunk: usize, check: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for part in tar.chunks(chunk) {
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        enc.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(check))
            .unwrap();
        enc.write_all(part).unwrap();
        out.extend_from_slice(&enc.finish().unwrap());
    }
    out
}

fn build(dir: &Path, files: &Files) -> PathBuf {
    let path = dir.join("game.tar.zst");
    fs::write(&path, zstd(&tar_bytes(files), CHUNK, true)).unwrap();
    path
}

fn single_frame(dir: &Path, files: &Files) -> PathBuf {
    let path = dir.join("game.tar.zst");
    fs::write(&path, zstd(&tar_bytes(files), usize::MAX, true)).unwrap();
    path
}

fn stream_env<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut env = vec![("HATCHZIP_STREAM_STEP", STEP)];
    env.extend_from_slice(extra);
    env
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../core/src/archive/fixtures/zst")
        .join(name)
}

fn copy_fixture(dir: &Path, name: &str) -> PathBuf {
    let archive = dir.join("game.tar.zst");
    fs::copy(fixture(name), &archive).unwrap();
    archive
}

#[test]
fn extracts_multi_frame_tar_byte_identical() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path(), &files);
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &stream_env(&[])));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

/// A frame boundary is a point even with a skippable frame in front of the
/// next one, as `pzstd` writes it.
#[test]
fn extracts_concatenated_and_skippable_frames() {
    let files = sample_files();
    let tar = tar_bytes(&files);
    let split = tar.len() / 2;
    let mut archive_bytes = zstd(&tar[..split], split, true);
    archive_bytes.extend_from_slice(&[0x50, 0x2A, 0x4D, 0x18, 4, 0, 0, 0, b'p', b'z', b's', b't']);
    archive_bytes.extend_from_slice(&zstd(&tar[split..], tar.len() - split, true));

    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("game.tar.zst");
    fs::write(&archive, archive_bytes).unwrap();
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &stream_env(&[])));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

#[test]
fn real_pzstd_frames_extract() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_fixture(tmp.path(), "zst-pzstd.tar.zst");
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &stream_env(&[])));
    assert_eq!(
        fs::metadata(out.join("game/random.bin")).unwrap().len(),
        4 * 1024 * 1024
    );
    assert_eq!(
        fs::metadata(out.join("game/text.txt")).unwrap().len(),
        4 * 1024 * 1024
    );
    assert!(out.join("game/README.txt").is_file());
    assert_consumed(&archive);
}

#[test]
fn a_single_frame_is_refused_in_place_and_extracts_when_kept() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let archive = single_frame(tmp.path(), &files);
    let original = fs::read(&archive).unwrap();
    let out = tmp.path().join("out");
    let refused = extract(&archive, &out, &[]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("one zstd frame"), "{stderr}");
    assert!(stderr.contains("--keep-archive"), "{stderr}");
    assert_eq!(fs::read(&archive).unwrap(), original);
    assert!(!out.exists());
    assert_no_side_files(&archive);

    assert_ok(&hatchzip(
        &[
            "extract",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--keep-archive",
            "--keep-free",
            "0",
            "--yes",
        ],
        &[],
    ));
    assert_tree(&out, &files);
    assert_eq!(fs::read(&archive).unwrap(), original);
    assert_no_side_files(&archive);
}

#[test]
fn frames_without_a_check_are_refused_in_place() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("game.tar.zst");
    fs::write(&archive, zstd(&tar_bytes(&files), CHUNK, false)).unwrap();
    let original = fs::read(&archive).unwrap();
    let out = tmp.path().join("out");
    let refused = extract(&archive, &out, &[]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("checksum"), "{stderr}");
    assert_eq!(fs::read(&archive).unwrap(), original);
    assert!(!out.exists());

    assert_ok(&hatchzip(
        &[
            "extract",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--keep-archive",
            "--keep-free",
            "0",
            "--yes",
        ],
        &[],
    ));
    assert_tree(&out, &files);
}

fn journal(archive: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(&side_files(archive)[0]).unwrap()).unwrap()
}

fn resume_byte(archive: &Path) -> Option<u64> {
    journal(archive)["resume"]["bit"].as_u64().map(|b| b / 8)
}

/// What a crashed run must leave: the archive at its full length, the frame
/// header at the journaled point whole, and every byte from there on
/// untouched.
fn check_crashed(archive: &Path, original: &[u8], what: &str) {
    let now = fs::read(archive).unwrap();
    assert_eq!(
        now.len(),
        original.len(),
        "{what}: the archive was truncated"
    );
    for p in side_files(archive) {
        assert!(p.exists(), "{what}: {} is missing", p.display());
    }
    let j = journal(archive);
    if let Some(stream) = j["resume"]["stream"].as_object() {
        assert_eq!(stream["kind"], "zstd", "{what}: a point of the wrong codec");
    }
    let Some(byte) = resume_byte(archive) else {
        assert!(now == original, "{what}: changed with no resume point");
        return;
    };
    let byte = byte as usize;
    assert!(
        now[byte..] == original[byte..],
        "{what}: changed past the resume point"
    );
}

fn crash_at_every_point(files: &Files, afters: &[u32]) {
    for point in [
        "stream-after-sync",
        "stream-after-journal",
        "stream-after-punch",
    ] {
        for after in afters {
            let tmp = TempDir::new().unwrap();
            let archive = build(tmp.path(), files);
            let original = fs::read(&archive).unwrap();
            let out = tmp.path().join("out");
            let after_s = after.to_string();
            let what = format!("{point}#{after}");

            let crashed = extract(
                &archive,
                &out,
                &stream_env(&[
                    ("HATCHZIP_CRASH_AT", point),
                    ("HATCHZIP_CRASH_AFTER", &after_s),
                ]),
            );
            assert!(!crashed.status.success(), "{what} should have crashed");
            check_crashed(&archive, &original, &what);

            assert_ok(&extract(&archive, &out, &stream_env(&[])));
            assert_tree(&out, files);
            assert_consumed(&archive);
        }
    }
}

#[test]
fn multi_frame_runs_survive_crashes() {
    crash_at_every_point(&sample_files(), &[0, 1, 2]);
}

/// Once a frame was punched, the stream can't be put back together. Before
/// the first point nothing was released, and restore leaves the archive.
#[test]
fn restore_refuses_a_partly_freed_tar_zst() {
    let files = sample_files();
    for (point, after, refused) in [
        ("stream-after-punch", "1", true),
        ("stream-after-journal", "0", true),
        ("stream-after-sync", "0", false),
    ] {
        let tmp = TempDir::new().unwrap();
        let archive = build(tmp.path(), &files);
        let original = fs::read(&archive).unwrap();
        let out = tmp.path().join("out");
        let crash = stream_env(&[
            ("HATCHZIP_CRASH_AT", point),
            ("HATCHZIP_CRASH_AFTER", after),
        ]);
        assert!(!extract(&archive, &out, &crash).status.success());

        let before = fs::read(&archive).unwrap();
        let journal = fs::read(&side_files(&archive)[0]).unwrap();
        let run = hatchzip(
            &[
                "restore",
                archive.to_str().unwrap(),
                "-o",
                out.to_str().unwrap(),
            ],
            &[],
        );
        if !refused {
            assert_ok(&run);
            assert_eq!(fs::read(&archive).unwrap(), original);
            for p in side_files(&archive) {
                assert!(!p.exists());
            }
            assert_ok(&extract(&archive, &out, &[]));
            assert_tree(&out, &files);
            continue;
        }
        assert!(!run.status.success(), "{point}: restore should refuse");
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(stderr.contains("cannot be restored"), "{point}: {stderr}");
        assert_eq!(fs::read(&archive).unwrap(), before, "{point}: changed");
        assert_eq!(fs::read(&side_files(&archive)[0]).unwrap(), journal);

        assert_ok(&extract(&archive, &out, &stream_env(&[])));
        assert_tree(&out, &files);
        assert_consumed(&archive);
    }
}

#[test]
fn damage_is_refused_before_anything_is_touched() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let good = zstd(&tar_bytes(&files), CHUNK, true);
    let mut bad_check = good.clone();
    let middle = bad_check.len() / 2;
    bad_check[middle] ^= 0xFF;
    let mut truncated = good.clone();
    truncated.truncate(truncated.len() - 8);
    // A damaged first magic is indistinguishable from a file that is not
    // zstd at all; the unit tests cover that refusal.
    for (name, bytes) in [("bad-check", bad_check), ("truncated", truncated)] {
        let archive = tmp.path().join(format!("{name}.tar.zst"));
        fs::write(&archive, &bytes).unwrap();
        let out = tmp.path().join(format!("{name}-out"));
        let run = extract(&archive, &out, &[]);
        assert!(!run.status.success(), "{name}");
        assert_eq!(run.status.code(), Some(4), "{name}: reported as damaged");
        assert_eq!(fs::read(&archive).unwrap(), bytes, "{name}: changed");
        assert!(!out.exists(), "{name}: output was created");
        assert_no_side_files(&archive);
    }
}

#[test]
fn a_plain_zstd_file_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("notes.zst");
    fs::write(&archive, zstd(b"not a tar archive\n", usize::MAX, true)).unwrap();
    let original = fs::read(&archive).unwrap();
    let out = tmp.path().join("out");
    let run = extract(&archive, &out, &[]);
    assert!(!run.status.success());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("does not contain a tar archive"),
        "{stderr}"
    );
    assert_eq!(fs::read(&archive).unwrap(), original);
    assert!(!out.exists());
}

#[test]
fn a_dictionary_frame_is_unsupported() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_fixture(tmp.path(), "zst-dict.tar.zst");
    let original = fs::read(&archive).unwrap();
    let out = tmp.path().join("out");
    let run = extract(&archive, &out, &[]);
    assert!(!run.status.success());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("dictionary"), "{stderr}");
    assert_eq!(fs::read(&archive).unwrap(), original);
    assert!(!out.exists());
    assert_no_side_files(&archive);
}
