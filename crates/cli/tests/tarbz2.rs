//! Real bzip2 streams, killed at every durable-point step, then resumed
//! from damaged prefixes and compared byte for byte.

mod common;

use common::*;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const STEP: &str = "131072";
const HUGE: &str = "game/data/noise.bin";

fn files() -> Files {
    let mut files = Files::new();
    files.insert(HUGE.into(), noise(4096, 34).repeat(400));
    files.insert(
        "game/data/text.txt".into(),
        b"every durable point keeps the archive safe\n".repeat(50000),
    );
    files.insert("game/empty.txt".into(), Vec::new());
    files.insert(
        format!("game/{}/deep.txt", "long-folder-".repeat(10)),
        b"long name\n".to_vec(),
    );
    files
}

fn fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tarbz2")
            .join(name),
    )
    .unwrap()
}

fn build(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join("game.tar.bz2");
    fs::write(&path, fixture(name)).unwrap();
    path
}

fn stream_env<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut env = vec![("HATCHZIP_STREAM_STEP", STEP)];
    env.extend_from_slice(extra);
    env
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
/// there is one.
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

#[test]
fn real_bzip2_levels_aliases_and_content_extract_byte_identical() {
    let files = files();
    for name in [
        "game.tar.bz2",
        "game.tbz2",
        "game.tbz",
        "game.TAR.BZ2",
        "game.TbZ2",
        "game.TBZ",
        "game.bz2",
        "unrecognized",
    ] {
        for source in ["level1.tar.bz2", "level9.tar.bz2", "multi.tar.bz2"] {
            let tmp = TempDir::new().unwrap();
            let archive = tmp.path().join(name);
            fs::write(&archive, fixture(source)).unwrap();
            let out = tmp.path().join("out");
            assert_ok(&extract(&archive, &out, &stream_env(&[])));
            assert_tree(&out, &files);
            assert_consumed(&archive);
        }
    }
}

#[test]
fn default_output_strips_every_bzip2_tar_suffix() {
    for name in ["game.tar.bz2", "game.tbz2", "game.tbz", "game.TAR.BZ2"] {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join(name);
        fs::write(&archive, fixture("level9.tar.bz2")).unwrap();
        assert_ok(&hatchzip(
            &[
                "extract",
                archive.to_str().unwrap(),
                "--keep-free",
                "0",
                "--yes",
            ],
            &[],
        ));
        assert_tree(&tmp.path().join("game"), &files());
    }
}

#[test]
fn list_and_test_leave_the_archive_unchanged() {
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path(), "multi.tar.bz2");
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
fn survives_every_step_at_every_resume_point() {
    // Discover the point count from real runs so changes to the fixture or
    // encoder cannot silently leave the final points untested.
    let files = files();
    for source in ["level1.tar.bz2", "level9.tar.bz2", "multi.tar.bz2"] {
        for point in [
            "stream-after-sync",
            "stream-after-journal",
            "stream-after-punch",
        ] {
            let mut after = 0;
            loop {
                let tmp = TempDir::new().unwrap();
                let archive = build(tmp.path(), source);
                let original = fs::read(&archive).unwrap();
                let out = tmp.path().join("out");
                let after_s = after.to_string();
                let run = extract(
                    &archive,
                    &out,
                    &stream_env(&[
                        ("HATCHZIP_CRASH_AT", point),
                        ("HATCHZIP_CRASH_AFTER", &after_s),
                    ]),
                );
                if run.status.success() {
                    assert!(after > 0, "{source}: no resume points");
                    assert_tree(&out, &files);
                    assert_consumed(&archive);
                    break;
                }
                let what = format!("{source}: {point}#{after}");
                check_crashed(
                    &archive,
                    &original,
                    point == "stream-after-punch",
                    &what,
                    &punch_probe_log(&run),
                    None,
                );
                if let Some(byte) = resume_byte(&archive) {
                    let j = journal(&archive);
                    assert_eq!(j["resume"]["stream"]["kind"], "bzip2");
                    assert_eq!(j["resume"]["stream"]["window"], "");
                    // An unusable prefix proves that resume never needs it,
                    // including runs killed before their pending punch.
                    let mut damaged = fs::read(&archive).unwrap();
                    damaged[..byte as usize].fill(0xa5);
                    fs::write(&archive, damaged).unwrap();
                }
                assert_ok(&extract(&archive, &out, &stream_env(&[])));
                assert_tree(&out, &files);
                assert_consumed(&archive);
                after += 1;
            }
        }
    }
}

#[test]
fn crashes_repeatedly_inside_one_stream() {
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path(), "level1.tar.bz2");
    let out = tmp.path().join("out");
    let mut last = 0;
    for point in [
        "stream-after-punch",
        "stream-after-sync",
        "stream-after-journal",
        "stream-after-punch",
    ] {
        let run = extract(
            &archive,
            &out,
            &stream_env(&[("HATCHZIP_CRASH_AT", point), ("HATCHZIP_CRASH_AFTER", "1")]),
        );
        assert!(!run.status.success());
        let next = resume_byte(&archive).unwrap();
        assert!(next > last);
        last = next;
    }
    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &files());
    assert_consumed(&archive);
}

#[test]
fn final_commit_uses_the_ordinary_crash_protocol() {
    crash_and_resume(|d| build(d, "level1.tar.bz2"), &files(), &[0]);
}

#[test]
fn damaged_blocks_stream_crc_truncation_and_trailing_data_are_refused() {
    let good = fixture("level1.tar.bz2");
    let mut garbage = good.clone();
    garbage.extend(b"not bzip2");
    let mut later = fixture("multi.tar.bz2");
    later.extend(fixture("bad-stream-crc.tar.bz2"));
    for bytes in [
        fixture("corrupt-block.tar.bz2"),
        fixture("bad-stream-crc.tar.bz2"),
        good[..good.len() - 10].to_vec(),
        garbage,
        later,
    ] {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("bad.tar.bz2");
        fs::write(&archive, &bytes).unwrap();
        let out = tmp.path().join("out");
        let run = extract(&archive, &out, &[]);
        assert_eq!(
            run.status.code(),
            Some(4),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(String::from_utf8_lossy(&run.stderr).contains("nothing was changed"));
        assert_eq!(fs::read(&archive).unwrap(), bytes);
        assert!(!out.exists());
        assert!(!side_files(&archive)[0].exists());
    }
}

#[test]
fn a_false_block_magic_inside_a_valid_block_is_ignored() {
    let bytes = fixture("false-magic.tar.bz2");
    let bits: String = bytes.iter().map(|b| format!("{b:08b}")).collect();
    let magic = format!("{:048b}", 0x3141_5926_5359u64);
    let offsets: Vec<usize> = (0..=bits.len() - 48)
        .filter(|i| bits[*i..].starts_with(&magic))
        .collect();
    assert_eq!(offsets.len(), 2, "{offsets:?}");
    assert_eq!(offsets, [32, 465]);
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("false.tar.bz2");
    fs::write(&archive, bytes).unwrap();
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &[]));
    let mut expected = Files::new();
    expected.insert("game/empty.txt".into(), Vec::new());
    assert_tree(&out, &expected);
    assert_consumed(&archive);
}

#[test]
fn split_sets_resume_from_any_volume() {
    for point in ["stream-after-journal", "stream-after-punch"] {
        let tmp = TempDir::new().unwrap();
        let bytes = fixture("level1.tar.bz2");
        let volumes = split(&bytes, &tmp.path().join("game.tar.bz2"), 3);
        let out = tmp.path().join("out");
        let run = extract(
            &volumes[0],
            &out,
            &stream_env(&[("HATCHZIP_CRASH_AT", point), ("HATCHZIP_CRASH_AFTER", "4")]),
        );
        assert!(!run.status.success());
        assert!(resume_byte(&volumes[0]).unwrap() > fs::metadata(&volumes[0]).unwrap().len());
        assert!(volumes.iter().all(|v| v.exists()));
        assert_ok(&extract(&volumes[1], &out, &stream_env(&[])));
        assert_tree(&out, &files());
        assert_consumed(&volumes[0]);
        assert!(volumes.iter().all(|v| !v.exists()));
    }
}

#[test]
fn restore_refuses_after_a_point_and_removes_uncommitted_output_before_it() {
    for (point, refused) in [
        ("stream-after-sync", false),
        ("stream-after-journal", true),
        ("stream-after-punch", true),
    ] {
        let tmp = TempDir::new().unwrap();
        let archive = build(tmp.path(), "level1.tar.bz2");
        let original = fs::read(&archive).unwrap();
        let out = tmp.path().join("out");
        assert!(
            !extract(&archive, &out, &stream_env(&[("HATCHZIP_CRASH_AT", point)]))
                .status
                .success()
        );
        let before = fs::read(&archive).unwrap();
        let saved = fs::read(&side_files(&archive)[0]).unwrap();
        let run = hatchzip(
            &[
                "restore",
                archive.to_str().unwrap(),
                "-o",
                out.to_str().unwrap(),
            ],
            &[],
        );
        if refused {
            assert!(!run.status.success());
            assert!(String::from_utf8_lossy(&run.stderr).contains("cannot be restored"));
            assert_eq!(fs::read(&archive).unwrap(), before);
            assert_eq!(fs::read(&side_files(&archive)[0]).unwrap(), saved);
        } else {
            assert_ok(&run);
            assert_eq!(fs::read(&archive).unwrap(), original);
            assert!(!side_files(&archive)[0].exists());
            assert!(!out.join(HUGE).exists());
        }
        assert_ok(&extract(&archive, &out, &[]));
        assert_tree(&out, &files());
        assert_consumed(&archive);
    }
}

#[test]
fn without_punching_the_space_check_asks_for_full_output_and_resume_still_works() {
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path(), "level1.tar.bz2");
    let before = fs::read(&archive).unwrap();
    let out = tmp.path().join("out");
    let check = hatchzip(
        &["check", archive.to_str().unwrap(), "--keep-free", "0"],
        &[("HATCHZIP_NO_PUNCH", "1")],
    );
    assert_ok(&check);
    assert!(String::from_utf8_lossy(&check.stdout).contains("remains whole"));
    assert!(
        !extract(
            &archive,
            &out,
            &stream_env(&[
                ("HATCHZIP_NO_PUNCH", "1"),
                ("HATCHZIP_CRASH_AT", "stream-after-punch")
            ])
        )
        .status
        .success()
    );
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert_ok(&extract(
        &archive,
        &out,
        &stream_env(&[("HATCHZIP_NO_PUNCH", "1")]),
    ));
    assert_tree(&out, &files());
    assert_consumed(&archive);
}

#[test]
fn a_false_magic_in_the_second_blocks_bitmap_is_ignored() {
    let bytes = fixture("early-false-magic.tar.bz2");
    let bits: String = bytes.iter().map(|b| format!("{b:08b}")).collect();
    let magic = format!("{:048b}", 0x3141_5926_5359u64);
    let offsets: Vec<_> = (0..=bits.len() - 48)
        .filter(|i| bits[*i..].starts_with(&magic))
        .collect();
    // The decoy is 106 bits into block two, inside its in-use bitmap.
    assert_eq!(offsets, [32, 764, 870, 1045]);
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("early.tar.bz2");
    fs::write(&archive, bytes).unwrap();
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &stream_env(&[])));
    let mut expected = Files::new();
    expected.insert("game/early.bin".into(), vec![0; 3]);
    assert_tree(&out, &expected);
    assert_consumed(&archive);
}

#[test]
fn kept_archives_resume_without_changing_any_volume() {
    for source in ["level1.tar.bz2", "multi.tar.bz2"] {
        for point in [
            "stream-after-sync",
            "stream-after-journal",
            "stream-after-punch",
        ] {
            let tmp = TempDir::new().unwrap();
            let original = fixture(source);
            let volumes = split(&original, &tmp.path().join("game.tar.bz2"), 3);
            let before: Vec<_> = volumes.iter().map(|p| fs::read(p).unwrap()).collect();
            let out = tmp.path().join("out");
            let run = hatchzip(
                &[
                    "extract",
                    volumes[0].to_str().unwrap(),
                    "-o",
                    out.to_str().unwrap(),
                    "--keep-free",
                    "0",
                    "--keep-archive",
                    "--yes",
                ],
                &stream_env(&[("HATCHZIP_CRASH_AT", point)]),
            );
            assert!(!run.status.success());
            for (p, bytes) in volumes.iter().zip(&before) {
                assert_eq!(fs::read(p).unwrap(), *bytes);
            }
            assert_ok(&extract(&volumes[1], &out, &stream_env(&[])));
            assert_tree(&out, &files());
            for (p, bytes) in volumes.iter().zip(&before) {
                assert_eq!(fs::read(p).unwrap(), *bytes);
            }
            assert!(!side_files(&volumes[0])[0].exists());
        }
    }
}

#[test]
fn a_bare_bzip2_file_is_refused_unchanged_with_a_next_step() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("plain.bz2");
    let bytes = fixture("not-tar.bz2");
    fs::write(&archive, &bytes).unwrap();
    let out = tmp.path().join("out");
    let run = extract(&archive, &out, &[]);
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("Decompress it with bzip2"));
    assert_eq!(fs::read(&archive).unwrap(), bytes);
    assert!(!out.exists());
}
