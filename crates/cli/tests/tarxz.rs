//! `.tar.xz`: one xz stream of independent blocks, extracted front to back
//! while the blocks behind the decoder are punched away. Killed at every
//! step of a resume point, resumed, and compared byte for byte; damaged;
//! refusals; kept whole without hole punching; single block.
//!
//! `HATCHZIP_STREAM_STEP` shrinks the step between resume points to 1 MiB,
//! so with 1 MiB blocks nearly every block end is one of them.

mod common;

use std::fs;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use common::*;
use lzma_rust2::{CheckType, XzOptions, XzWriter};
use tempfile::TempDir;

const STEP: &str = "1048576";
const BLOCK: u64 = 1024 * 1024;
const HUGE: &str = "game/data/huge.txt";

/// Text-like data with bursts of noise: LZMA2 gives it both literals and
/// matches reaching back across the whole dictionary, so every block decodes
/// through real back references to what came before it in that block.
fn text(len: usize) -> Vec<u8> {
    let words: Vec<&str> = "hatch every byte of the archive back into files while the disk \
        stays nearly full and a crash at any moment loses nothing 42 1337 zran"
        .split(' ')
        .collect();
    let bursts = noise(len / 50 + 1, 77);
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut out = Vec::with_capacity(len + 64);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(words[(x % words.len() as u64) as usize].as_bytes());
        out.push(if x & 0x700 == 0 { b'\n' } else { b' ' });
        if x & 0x3FFF == 0 {
            let at = (x as usize >> 20) % (bursts.len() - 600);
            out.extend_from_slice(&bursts[at..at + 600]);
        }
    }
    out.truncate(len);
    out
}

/// A file name over 100 bytes, which tar can only store in an extra record.
fn long_name() -> String {
    format!("game/{}/deep.txt", "very-long-folder-name".repeat(8))
}

fn files() -> Files {
    let mut files = sample_files();
    files.insert(HUGE.into(), text(8_000_000));
    files.insert(long_name(), text(300_000));
    files
}

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

/// xz with a block boundary every `blocks` bytes of output; `None` puts the
/// whole stream in one block, what `xz -T1` does. CRC64 checks, xz's default.
fn xz(tar: Vec<u8>, blocks: Option<u64>) -> Vec<u8> {
    let mut opts = XzOptions::with_preset(0);
    opts.set_block_size(blocks.and_then(NonZeroU64::new));
    let mut w = XzWriter::new(Vec::new(), opts).unwrap();
    w.write_all(&tar).unwrap();
    w.finish().unwrap()
}

fn build(dir: &Path, files: &Files) -> PathBuf {
    let path = dir.join("game.tar.xz");
    fs::write(&path, xz(tar_bytes(files), Some(BLOCK))).unwrap();
    path
}

fn single_block(dir: &Path, files: &Files) -> PathBuf {
    let path = dir.join("game.tar.xz");
    fs::write(&path, xz(tar_bytes(files), None)).unwrap();
    path
}

fn stream_env<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut env = vec![("HATCHZIP_STREAM_STEP", STEP)];
    env.extend_from_slice(extra);
    env
}

fn fixture_files_with_len(len: usize) -> Files {
    let mut state = 12345u64;
    let mut pattern: Vec<u8> = (0..262_144)
        .map(|_| {
            state = (state * 1_103_515_245 + 12_345) & 0x7fff_ffff;
            (state >> 16) as u8
        })
        .collect();
    for i in (0..pattern.len() - 5).step_by(64) {
        pattern[i..i + 5].copy_from_slice(&[0xe8, 1, 0, 0, 0]);
    }
    let mut tool = pattern.repeat(len.div_ceil(pattern.len()));
    tool.truncate(len);
    let mut files = Files::new();
    files.insert("game/tool.exe".into(), tool);
    files.insert("game/readme.txt".into(), b"tar.xz fixture\n".to_vec());
    files
}

fn fixture_files() -> Files {
    fixture_files_with_len(4 * 1024 * 1024 + 4096)
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tarxz")
        .join(name)
}

fn copy_fixture(dir: &Path, name: &str) -> PathBuf {
    let archive = dir.join("game.tar.xz");
    fs::copy(fixture(name), &archive).unwrap();
    archive
}

fn copy_split_fixture(dir: &Path) -> Vec<PathBuf> {
    (1..=3)
        .map(|i| {
            let path = dir.join(format!("game.tar.xz.{i:03}"));
            fs::copy(fixture(&format!("split.tar.xz.{i:03}")), &path).unwrap();
            path
        })
        .collect()
}

#[test]
fn real_xz_multiblock_filters_extract_byte_identical() {
    for name in ["multi.tar.xz", "bcj.tar.xz", "delta.tar.xz"] {
        let tmp = TempDir::new().unwrap();
        let archive = copy_fixture(tmp.path(), name);
        let out = tmp.path().join("out");
        assert_ok(&extract(&archive, &out, &stream_env(&[])));
        assert_tree(&out, &fixture_files());
        assert_consumed(&archive);
    }
}

#[test]
fn real_xz_default_multithreaded_blocks_extract() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_fixture(tmp.path(), "default.tar.xz");
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &stream_env(&[])));
    assert_tree(&out, &fixture_files_with_len(25 * 1024 * 1024));
    assert_consumed(&archive);
}

#[test]
fn real_xz_single_block_kept_and_multistream_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_fixture(tmp.path(), "single.tar.xz");
    let original = fs::read(&archive).unwrap();
    let out = tmp.path().join("out");
    let refused = extract(&archive, &out, &[]);
    assert!(!refused.status.success());
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
    assert_tree(&out, &fixture_files());
    assert_eq!(fs::read(&archive).unwrap(), original);
    assert_no_side_files(&archive);

    let tmp = TempDir::new().unwrap();
    let archive = copy_fixture(tmp.path(), "multistream.tar.xz");
    let original = fs::read(&archive).unwrap();
    let out = tmp.path().join("out");
    let refused = extract(&archive, &out, &[]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("one after another"));
    assert_eq!(fs::read(&archive).unwrap(), original);
    assert!(!out.exists());
    assert_no_side_files(&archive);
}

#[test]
fn real_xz_filter_blocks_survive_crashes() {
    let files = fixture_files();
    for name in ["multi.tar.xz", "bcj.tar.xz", "delta.tar.xz"] {
        crash_at_every_point(|dir| copy_fixture(dir, name), &files, &[0, 2]);
    }
}

#[test]
fn real_xz_default_blocks_survive_crashes() {
    let files = fixture_files_with_len(25 * 1024 * 1024);
    crash_at_every_point(|dir| copy_fixture(dir, "default.tar.xz"), &files, &[0]);
}

#[test]
fn real_xz_split_volumes_extract_and_resume() {
    {
        let tmp = TempDir::new().unwrap();
        let volumes = copy_split_fixture(tmp.path());
        let out = tmp.path().join("out");
        assert_ok(&extract(&volumes[2], &out, &stream_env(&[])));
        assert_tree(&out, &fixture_files());
        assert_consumed(&volumes[0]);
        for volume in &volumes {
            assert!(!volume.exists(), "{} remains", volume.display());
        }
    }
    for point in [
        "stream-after-sync",
        "stream-after-journal",
        "stream-after-punch",
    ] {
        let tmp = TempDir::new().unwrap();
        let volumes = copy_split_fixture(tmp.path());
        let archive = &volumes[0];
        let original: Vec<u8> = volumes.iter().flat_map(|p| fs::read(p).unwrap()).collect();
        let out = tmp.path().join("out");
        let crashed = extract(
            archive,
            &out,
            &stream_env(&[("HATCHZIP_CRASH_AT", point), ("HATCHZIP_CRASH_AFTER", "2")]),
        );
        assert!(!crashed.status.success(), "{point} should have crashed");
        let current: Vec<u8> = volumes.iter().flat_map(|p| fs::read(p).unwrap()).collect();
        assert_eq!(
            current.len(),
            original.len(),
            "{point}: volume lengths changed"
        );
        let offset = resume_byte(archive).expect("a durable resume point") as usize;
        assert_eq!(
            &current[offset..],
            &original[offset..],
            "{point}: changed past the resume point"
        );
        assert_ok(&extract(archive, &out, &stream_env(&[])));
        assert_tree(&out, &fixture_files());
        assert_consumed(archive);
        for volume in &volumes {
            assert!(!volume.exists(), "{} remains", volume.display());
        }
    }
}

/// Bytes the archive occupies on disk, where the system can say.
#[cfg(unix)]
fn allocated(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(fs::metadata(path).ok()?.blocks() * 512)
}

#[cfg(not(unix))]
fn allocated(_path: &Path) -> Option<u64> {
    None
}

fn journal(archive: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(&side_files(archive)[0]).unwrap()).unwrap()
}

/// The byte of the archive the journaled resume point restarts in, if
/// there is one. For xz that is a block header.
fn resume_byte(archive: &Path) -> Option<u64> {
    journal(archive)["resume"]["bit"].as_u64().map(|b| b / 8)
}

/// What a crashed run must leave: the archive at its full length, every
/// byte from the journaled resume point on untouched, and, where holes can
/// be punched, the space before it given back. `probe_log` is the crashed
/// run's punch-probe line, so a failure can tell a refusal from late space.
/// `allocated_before` is what the archive occupied before the run, when the
/// tool that wrote it may have left blocks allocated past its end (macOS
/// `tar` on APFS does, now and then).
fn check_crashed(
    archive: &Path,
    original: &[u8],
    punched: bool,
    what: &str,
    probe_log: &str,
    allocated_before: Option<u64>,
) {
    let now = fs::read(archive).unwrap();
    assert_eq!(
        now.len(),
        original.len(),
        "{what}: the archive was truncated"
    );
    for p in side_files(archive) {
        assert!(p.exists(), "{what}: {} is missing", p.display());
    }
    // An xz resume point is a block header, and the run journals the point
    // of the block it is about to decode. Whatever the journal says belongs
    // to this stream: the punch rounds its range down from the block's own
    // start of data, so the header at `byte` stays whole. Before the run
    // reached its first point there is no resume yet.
    let j = journal(archive);
    if let Some(stream) = j["resume"]["stream"].as_object() {
        assert_eq!(
            stream["kind"], "xz",
            "{what}: a tar.xz point of the wrong codec"
        );
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
    if punched
        && std::env::var_os("HATCHZIP_EXPECT_PUNCH").is_some()
        && let Some(used) = allocated(archive)
    {
        let len = original.len() as u64;
        let before = allocated_before
            .unwrap_or(0)
            .max(len.next_multiple_of(4096));
        assert!(
            used + byte as u64 <= before + 2 * 65_536,
            "{what}: {used} of {len} bytes ({before} before the run) still allocated \
             past a punch to {byte}; probe said: {probe_log}"
        );
    }
}

/// The line the debug engine logs about its scratch-file punch probe. The
/// failing assert alone cannot tell "the filesystem refused to punch" from
/// "APFS gave the space back late", and this line is what tells them apart.
fn punch_probe_log(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .find(|line| line.contains("punch probe"))
        .unwrap_or("no probe line in the crashed run's stderr")
        .to_owned()
}

/// Kills a run at each step of a resume point, after `after` earlier
/// points, checks what it left, then resumes it and checks the result.
fn crash_at_every_point(build: impl Fn(&Path) -> PathBuf, files: &Files, afters: &[u32]) {
    for point in [
        "stream-after-sync",
        "stream-after-journal",
        "stream-after-punch",
    ] {
        for after in afters {
            let tmp = TempDir::new().unwrap();
            let archive = build(tmp.path());
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
            check_crashed(
                &archive,
                &original,
                point == "stream-after-punch",
                &what,
                &punch_probe_log(&crashed),
                None,
            );

            assert_ok(&extract(&archive, &out, &stream_env(&[])));
            assert_tree(&out, files);
            assert_consumed(&archive);
        }
    }
}

#[test]
fn extracts_tar_xz_byte_identical() {
    let files = files();
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path(), &files);
    // No output folder given: it goes next to the archive, named after it.
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
    assert_ok(&run);
    assert!(String::from_utf8_lossy(&run.stdout).contains("tar.xz"));
    assert_tree(&tmp.path().join("game"), &files);
    assert_consumed(&archive);
}

#[test]
fn list_and_test_read_a_tar_xz_without_changing_it() {
    let files = files();
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path(), &files);
    let before = fs::read(&archive).unwrap();
    let a = archive.to_str().unwrap();
    let list = hatchzip(&["list", a], &[]);
    assert_ok(&list);
    assert!(String::from_utf8_lossy(&list.stdout).contains("deep.txt"));
    assert_ok(&hatchzip(&["test", a], &[]));
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert!(!side_files(&archive)[0].exists());
}

#[test]
fn a_tar_xz_survives_a_crash_at_every_step_of_a_resume_point() {
    let files = files();
    crash_at_every_point(|d| build(d, &files), &files, &[0, 4, 9]);
}

/// The commit at the end of the stream goes through the same two crash
/// points as every batch.
#[test]
fn crashes_around_the_final_commit_of_a_tar_xz() {
    let files = files();
    for point in ["after-extract", "after-journal"] {
        let tmp = TempDir::new().unwrap();
        let archive = build(tmp.path(), &files);
        let out = tmp.path().join("out");
        let crashed = extract(&archive, &out, &stream_env(&[("HATCHZIP_CRASH_AT", point)]));
        assert!(!crashed.status.success(), "{point}");
        assert_ok(&extract(&archive, &out, &stream_env(&[])));
        assert_tree(&out, &files);
        assert_consumed(&archive);
    }
}

#[test]
fn crashing_again_and_again_inside_one_tar_xz_entry_is_harmless() {
    let files = files();
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path(), &files);
    let original = fs::read(&archive).unwrap();
    let out = tmp.path().join("out");
    let mut points = Vec::new();
    for point in [
        "stream-after-punch",
        "stream-after-sync",
        "stream-after-journal",
        "stream-after-punch",
        "stream-after-punch",
    ] {
        let crashed = extract(
            &archive,
            &out,
            &stream_env(&[("HATCHZIP_CRASH_AT", point), ("HATCHZIP_CRASH_AFTER", "1")]),
        );
        assert!(!crashed.status.success(), "{point} should have crashed");
        check_crashed(
            &archive,
            &original,
            point == "stream-after-punch",
            point,
            &punch_probe_log(&crashed),
            None,
        );
        points.push(journal(&archive)["resume"].clone());
    }
    // Every run got further, and the later ones stopped inside the big file:
    // a block boundary of its data, past what an earlier run already wrote,
    // which a resume continues from.
    let bits: Vec<u64> = points.iter().map(|p| p["bit"].as_u64().unwrap()).collect();
    assert!(bits.windows(2).all(|w| w[0] < w[1]), "{bits:?}");
    let inside = |p: &serde_json::Value| {
        p["stream"]["at"]["file"]["path"]
            .as_str()
            .is_some_and(|s| s.replace('\\', "/") == HUGE)
    };
    assert!(
        points
            .iter()
            .any(|p| p["written"].as_u64().unwrap() > 0 && inside(p)),
        "no point inside {HUGE}: {points:?}"
    );
    // The last run goes on without the knobs: one step of 256 MiB.
    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

/// Damage anywhere is found by the walk through the whole stream that
/// opening the archive makes, before anything is touched.
#[test]
fn a_damaged_tar_xz_is_refused_before_anything_is_touched() {
    let files = files();
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path(), &files);
    let good = fs::read(&archive).unwrap();
    let n = good.len();
    let out = tmp.path().join("out");

    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    // A block's data: its CRC64 check fails in the pre-touch scan.
    let mut block = good.clone();
    let at = n / 8 + 10_000;
    for b in &mut block[at..at + 64] {
        *b ^= 0x5A;
    }
    cases.push(("CRC-64", block));
    // The index's or the footer's own bytes: the structure cannot be read.
    let mut index = good.clone();
    index[n - 8] ^= 1;
    cases.push(("", index));
    cases.push(("", good[..n - 100].to_vec()));
    let mut garbage = good.clone();
    garbage.extend(b"not xz");
    cases.push(("", garbage));
    let mut one_block_short = good.clone();
    one_block_short[n / 3] ^= 0xFF;
    cases.push(("", one_block_short));

    for (expect, bytes) in cases {
        fs::write(&archive, &bytes).unwrap();
        let run = extract(&archive, &out, &stream_env(&[]));
        assert_eq!(
            run.status.code(),
            Some(4),
            "{expect}: should be refused as damaged: \
             {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(stderr.contains(expect), "{expect}: {stderr}");
        assert!(stderr.contains("nothing was changed"), "{stderr}");
        assert_eq!(fs::read(&archive).unwrap(), bytes, "{expect}: changed");
        assert!(!side_files(&archive)[0].exists());
        assert!(!out.exists());
        let test = hatchzip(&["test", archive.to_str().unwrap()], &[]);
        assert_eq!(
            test.status.code(),
            Some(4),
            "{expect}: test should report damage"
        );
    }
}

/// Damage past the last resume point, found when a crashed run resumes:
/// the resume stops before changing anything, keeps the journal and the
/// half-written file, and once the archive is whole again it finishes.
#[test]
fn damage_found_on_resume_loses_no_progress() {
    let files = files();
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path(), &files);
    let out = tmp.path().join("out");
    let crash = stream_env(&[
        ("HATCHZIP_CRASH_AT", "stream-after-punch"),
        ("HATCHZIP_CRASH_AFTER", "4"),
    ]);
    assert!(!extract(&archive, &out, &crash).status.success());
    let journal_before = fs::read(&side_files(&archive)[0]).unwrap();
    let byte = resume_byte(&archive).unwrap() as usize;

    let mut bytes = fs::read(&archive).unwrap();
    let at = bytes.len() - 20_000;
    assert!(at > byte);
    let saved = bytes[at..at + 64].to_vec();
    for b in &mut bytes[at..at + 64] {
        *b ^= 0x5A;
    }
    fs::write(&archive, &bytes).unwrap();

    let run = extract(&archive, &out, &stream_env(&[]));
    assert!(!run.status.success());
    assert_eq!(fs::read(&archive).unwrap(), bytes, "the archive changed");
    assert_eq!(fs::read(&side_files(&archive)[0]).unwrap(), journal_before);
    let part = journal(&archive)["resume"]["stream"]["at"]["file"]["path"]
        .as_str()
        .map(|p| out.join(format!("{p}.hatchzip-part")));
    if let Some(part) = part {
        assert!(part.exists(), "{} is gone", part.display());
    }

    bytes[at..at + 64].copy_from_slice(&saved);
    fs::write(&archive, &bytes).unwrap();
    assert_ok(&extract(&archive, &out, &stream_env(&[])));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

/// One block holds the whole tar: nothing can be freed while it decodes, so
/// an in-place run is refused, while `--keep-archive` extracts it fine.
#[test]
fn a_single_block_tar_xz_refuses_in_place_and_extracts_when_kept() {
    let files = files();
    let tmp = TempDir::new().unwrap();
    let archive = single_block(tmp.path(), &files);

    let run = extract(&archive, &tmp.path().join("out"), &[]);
    assert!(!run.status.success());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("one xz block"), "{stderr}");
    assert!(
        stderr.contains("nothing of the archive can be freed"),
        "{stderr}"
    );
    assert!(!side_files(&archive)[0].exists());

    let a = archive.to_str().unwrap();
    let out = tmp.path().join("out");
    assert_ok(&hatchzip(
        &[
            "extract",
            a,
            "-o",
            out.to_str().unwrap(),
            "--keep-archive",
            "--yes",
        ],
        &[],
    ));
    assert_tree(&out, &files);
    assert!(
        fs::read(&archive)
            .unwrap()
            .starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0])
    );
    assert_no_side_files(&archive);
}

/// What `check` says HatchZip needs, in bytes, give or take its rounding.
fn needed(archive: &Path, env: &[(&str, &str)]) -> (f64, String) {
    let a = archive.to_str().unwrap();
    let run = hatchzip(&["check", a, "--keep-free", "0"], env);
    let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("required"))
        .unwrap_or_else(|| panic!("no plan in:\n{stdout}"));
    let words: Vec<&str> = line.split_whitespace().collect();
    let at = words.iter().position(|w| *w == "required").unwrap();
    let value: f64 = words[at + 1]
        .trim_matches(|c: char| !c.is_ascii_digit() && c != '.')
        .parse()
        .unwrap();
    let scale = match words[at + 2].trim_matches(|c: char| !c.is_ascii_alphabetic()) {
        "B" => 1.0,
        "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        other => panic!("unit {other} in {line}"),
    };
    (value * scale, stdout)
}

#[test]
fn the_space_check_for_a_tar_xz_counts_freeing_only_where_it_works() {
    let files = files();
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path(), &files);
    let total: u64 = files.values().map(|d| d.len() as u64).sum();

    let (streaming, said) = needed(&archive, &[("HATCHZIP_STREAM_STEP", STEP)]);
    assert!(!said.contains("remains whole"), "{said}");
    let (whole, said) = needed(
        &archive,
        &[("HATCHZIP_STREAM_STEP", STEP), ("HATCHZIP_NO_PUNCH", "1")],
    );
    assert!(whole >= total as f64, "without punching it needs {whole}");
    assert!(said.contains("remains whole"), "{said}");
    // NTFS frees in units of up to 1 MiB, and the plan keeps a granule at each
    // end of the punched range: with 1 MiB blocks that eats the whole saving,
    // so there it may need as much as keeping, never more. Elsewhere it must
    // need less.
    if cfg!(windows) {
        assert!(
            streaming <= whole,
            "with punching it should never need more than {whole}, not {streaming}"
        );
    } else {
        assert!(
            streaming < whole,
            "with punching it should need less than {whole}, not {streaming}"
        );
    }
}

/// On a filesystem that can't punch holes, the archive stays whole until
/// the end, and a crash still resumes from the last point.
#[test]
fn without_hole_punching_a_tar_xz_is_kept_whole_and_still_resumes() {
    let files = files();
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path(), &files);
    let original = fs::read(&archive).unwrap();
    let out = tmp.path().join("out");
    let crash = stream_env(&[
        ("HATCHZIP_NO_PUNCH", "1"),
        ("HATCHZIP_CRASH_AT", "stream-after-punch"),
        ("HATCHZIP_CRASH_AFTER", "3"),
    ]);
    assert!(!extract(&archive, &out, &crash).status.success());
    assert!(resume_byte(&archive).is_some());
    assert_eq!(fs::read(&archive).unwrap(), original, "punched anyway");
    if let Some(used) = allocated(&archive) {
        assert!(used >= original.len() as u64 / 4096 * 4096);
    }
    assert_ok(&extract(
        &archive,
        &out,
        &stream_env(&[("HATCHZIP_NO_PUNCH", "1")]),
    ));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

/// Once holes are punched, the stream can't be put back together. Before
/// the first point is journaled nothing was released, and restore leaves
/// the original archive.
#[test]
fn restore_refuses_a_partly_freed_tar_xz() {
    let files = files();
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

/// SHA-256 checks are refused on opening, keeping the archive unmodified
/// whether it would be extracted in place or kept.
#[test]
fn a_sha256_xz_is_refused_before_anything_is_touched() {
    let files = files();
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("game.tar.xz");
    let mut opts = XzOptions::with_preset(0);
    opts.set_block_size(Some(NonZeroU64::new(BLOCK).unwrap()));
    opts.set_check_sum_type(CheckType::Sha256);
    let mut w = XzWriter::new(Vec::new(), opts).unwrap();
    w.write_all(&tar_bytes(&files)).unwrap();
    fs::write(&archive, w.finish().unwrap()).unwrap();
    let good = fs::read(&archive).unwrap();
    let out = tmp.path().join("out");

    let run = extract(&archive, &out, &[]);
    // Valid archive, unsupported check: not damage, and the CLI says so by
    // its generic failure status rather than the damaged one.
    assert_eq!(run.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("SHA-256"), "{stderr}");
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(fs::read(&archive).unwrap(), good);

    let kept = hatchzip(
        &[
            "extract",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--keep-archive",
            "--yes",
        ],
        &[],
    );
    assert!(!kept.status.success(), "kept or not, sha256 is refused");
    assert_eq!(fs::read(&archive).unwrap(), good);
    assert!(!side_files(&archive)[0].exists());
}

/// One stream only: xz archives concatenated one after the other are
/// refused on opening, with the archive unmodified.
#[test]
fn concatenated_xz_streams_are_refused() {
    let files = files();
    let tmp = TempDir::new().unwrap();
    let tar = tar_bytes(&files);
    let archive = tmp.path().join("game.tar.xz");
    let mut bytes = xz(tar.clone(), None);
    bytes.extend(xz(tar, None));
    fs::write(&archive, &bytes).unwrap();
    let out = tmp.path().join("out");

    let run = extract(&archive, &out, &[]);
    assert_eq!(run.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("one after another"), "{stderr}");
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(fs::read(&archive).unwrap(), bytes);
}

/// An xz stream with no block checks decodes fine, but damage inside it
/// could not be told from the original bytes, so an in-place run is refused
/// and `--keep-archive` extracts it.
#[test]
fn an_unchecked_multi_block_tar_xz_refuses_in_place_and_extracts_when_kept() {
    let files = files();
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("game.tar.xz");
    let mut opts = XzOptions::with_preset(0);
    opts.set_block_size(Some(NonZeroU64::new(BLOCK).unwrap()));
    opts.set_check_sum_type(CheckType::None);
    let mut w = XzWriter::new(Vec::new(), opts).unwrap();
    w.write_all(&tar_bytes(&files)).unwrap();
    fs::write(&archive, w.finish().unwrap()).unwrap();
    let good = fs::read(&archive).unwrap();

    let run = extract(&archive, &tmp.path().join("out"), &[]);
    assert!(!run.status.success());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("carry no check"), "{stderr}");
    assert!(stderr.contains("--keep-archive"), "{stderr}");
    assert_eq!(fs::read(&archive).unwrap(), good);

    let out = tmp.path().join("kept");
    assert_ok(&hatchzip(
        &[
            "extract",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--keep-archive",
            "--yes",
        ],
        &[],
    ));
    assert_tree(&out, &files);
    assert_eq!(fs::read(&archive).unwrap(), good);
}

/// Real-world detail: a `.tar.xz` whose stream is fine but whose content
/// was never a tar. Nothing is changed, the archive is intact.
#[test]
fn an_xz_file_that_is_not_a_tar_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("notes.bin.xz");
    let bytes = xz(text(100_000), None);
    fs::write(&archive, &bytes).unwrap();
    let run = extract(&archive, &tmp.path().join("out"), &[]);
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("nothing was changed"));
    assert_eq!(fs::read(&archive).unwrap(), bytes);
}

/// Once the final commit is journaled, every entry is out and the archive
/// only waits to be removed. Restore must say so instead of cutting the
/// archive back into something that is no longer the original.
#[test]
fn restore_after_the_final_commit_says_everything_is_extracted() {
    let files = files();
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path(), &files);
    let out = tmp.path().join("out");
    let crashed = extract(
        &archive,
        &out,
        &stream_env(&[("HATCHZIP_CRASH_AT", "after-journal")]),
    );
    assert!(!crashed.status.success(), "the run should have crashed");
    let before = fs::read(&archive).unwrap();

    let run = hatchzip(
        &[
            "restore",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ],
        &[],
    );
    assert!(!run.status.success(), "restore should refuse");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("already extracted"), "{stderr}");
    assert_eq!(
        fs::read(&archive).unwrap(),
        before,
        "restore changed the archive"
    );

    assert_ok(&extract(&archive, &out, &stream_env(&[])));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}
