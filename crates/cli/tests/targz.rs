//! `.tar.gz`: one compressed stream, extracted front to back while the
//! compressed bytes behind the decoder are freed. Killed at every step of a
//! resume point, resumed, and compared byte for byte; damaged, with and
//! without hole punching, in several gzip members, with long names and links.
//!
//! `HATCHZIP_STREAM_STEP` shrinks the step between resume points to 1 MiB,
//! so the 8 MB text file and the rest cross about ten of them.

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use common::*;
use flate2::Compression;
use flate2::write::GzEncoder;
use tempfile::TempDir;

const STEP: &str = "1048576";
const HUGE: &str = "game/data/huge.txt";

/// Text-like data with bursts of noise: deflate gives it Huffman blocks
/// whose matches reach back across block boundaries, and stored blocks in
/// between, so resuming needs the window and the exact bit position.
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

/// Gzip with a new member at each of `cuts`, offsets into `data`.
fn gzip(data: &[u8], cuts: &[usize], level: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut from = 0;
    for &to in cuts.iter().chain([data.len()].iter()) {
        let mut e = GzEncoder::new(Vec::new(), Compression::new(level));
        e.write_all(&data[from..to]).unwrap();
        out.extend(e.finish().unwrap());
        from = to;
    }
    out
}

fn build(dir: &Path, files: &Files) -> PathBuf {
    let path = dir.join("game.tar.gz");
    fs::write(&path, gzip(&tar_bytes(files), &[], 6)).unwrap();
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
fn extracts_tar_gz_and_tgz_byte_identical() {
    let files = files();
    for name in ["game.tar.gz", "game.tgz"] {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join(name);
        fs::write(&archive, gzip(&tar_bytes(&files), &[], 6)).unwrap();
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
        assert!(String::from_utf8_lossy(&run.stdout).contains("tar.gz"));
        assert_tree(&tmp.path().join("game"), &files);
        assert_consumed(&archive);
    }
}

#[test]
fn list_and_test_read_a_tar_gz_without_changing_it() {
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
fn a_tar_gz_survives_a_crash_at_every_step_of_a_resume_point() {
    let files = files();
    crash_at_every_point(|d| build(d, &files), &files, &[0, 4, 9]);
}

/// The commit at the end of the stream goes through the same two crash
/// points as every batch.
#[test]
fn crashes_around_the_final_commit_of_a_tar_gz() {
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
fn crashing_again_and_again_inside_one_tar_gz_entry_is_harmless() {
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
    // Every run got further, and the later ones stopped inside the big file.
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

/// A resume point inside a file dated before 1970 keeps that date.
#[test]
fn a_time_before_1970_survives_a_resume_inside_its_file() {
    let files = files();
    let tmp = TempDir::new().unwrap();
    let mut b = tar::Builder::new(Vec::new());
    let mut dir = tar::Header::new_gnu();
    dir.set_entry_type(tar::EntryType::Directory);
    dir.set_mode(0o755);
    dir.set_size(0);
    b.append_data(&mut dir, EMPTY_DIR, std::io::empty())
        .unwrap();
    for (name, data) in &files {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(1_700_000_000);
        if name == HUGE {
            // GNU tar's base-256 form of a negative time.
            let mtime = &mut h.as_old_mut().mtime;
            mtime[..4].fill(0xff);
            mtime[4..].copy_from_slice(&(-152_391_233_i64).to_be_bytes());
        }
        b.append_data(&mut h, name, data.as_slice()).unwrap();
    }
    let archive = tmp.path().join("old.tar.gz");
    fs::write(&archive, gzip(&b.into_inner().unwrap(), &[], 6)).unwrap();
    let out = tmp.path().join("out");

    let inside = |a: &Path| {
        let j = journal(a);
        j["resume"]["written"].as_u64().unwrap_or(0) > 0
            && j["resume"]["stream"]["at"]["file"]["path"]
                .as_str()
                .is_some_and(|s| s.replace('\\', "/") == HUGE)
    };
    let mut crashes = 0;
    while crashes == 0 || !inside(&archive) {
        crashes += 1;
        assert!(crashes <= 10, "no resume point inside {HUGE}");
        let crashed = extract(
            &archive,
            &out,
            &stream_env(&[
                ("HATCHZIP_CRASH_AT", "stream-after-journal"),
                ("HATCHZIP_CRASH_AFTER", "1"),
            ]),
        );
        assert!(!crashed.status.success());
    }
    assert_eq!(
        journal(&archive)["resume"]["stream"]["at"]["file"]["mtime"],
        -152_391_233
    );

    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &files);
    let modified = fs::metadata(out.join(HUGE)).unwrap().modified().unwrap();
    assert_eq!(
        modified,
        std::time::UNIX_EPOCH - std::time::Duration::from_secs(152_391_233)
    );
}

/// Several gzip members, as `bgzip`, `pigz --independent` and concatenated
/// files make: member boundaries inside a file's data and inside a header,
/// an empty member, mixed levels, and many small members, each a free
/// resume point.
#[test]
fn multi_member_gzip_extracts_and_resumes() {
    let files = files();
    let tar = tar_bytes(&files);
    let n = tar.len();
    let cuts = [700, 1_000_000, 1_000_000, n / 2, n / 2 + 300];
    let mixed = |d: &Path| {
        let path = d.join("game.tar.gz");
        let mut bytes = gzip(&tar[..cuts[3]], &cuts[..3], 9);
        bytes.extend(gzip(&tar[cuts[3]..], &[cuts[4] - cuts[3]], 1));
        fs::write(&path, bytes).unwrap();
        path
    };
    crash_at_every_point(mixed, &files, &[0, 3]);

    let small = |d: &Path| {
        let cuts: Vec<usize> = (1..n / 65_536).map(|i| i * 65_536).collect();
        let path = d.join("game.tar.gz");
        fs::write(&path, gzip(&tar, &cuts, 6)).unwrap();
        path
    };
    crash_at_every_point(small, &files, &[2]);
}

/// A pax record: its length, counting itself, then `key=value`.
fn pax(key: &str, value: &str) -> Vec<u8> {
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    while format!("{len}").len() + body.len() != len {
        len += 1;
    }
    format!("{len}{body}").into_bytes()
}

#[cfg(unix)]
#[test]
fn long_names_and_links_survive_crashes() {
    use std::os::unix::fs::MetadataExt;

    let big = text(4_000_000);
    let long = long_name();
    let pax_path = format!("game/{}/pax.txt", "pax-named-folder".repeat(9));
    let long_target = format!("../{}/deep.txt", "very-long-folder-name".repeat(8));
    let build = |d: &Path| {
        let mut b = tar::Builder::new(Vec::new());
        // git archive starts with a global header holding the commit id.
        let comment = pax("comment", "0123456789abcdef0123456789abcdef01234567");
        let mut g = tar::Header::new_ustar();
        g.set_entry_type(tar::EntryType::XGlobalHeader);
        g.set_path("pax_global_header").unwrap();
        g.set_size(comment.len() as u64);
        g.set_cksum();
        b.append(&g, comment.as_slice()).unwrap();

        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_size(big.len() as u64);
        b.append_data(&mut h, &long, big.as_slice()).unwrap();

        // A path only a pax record holds; the ustar name is a stand-in.
        let record = pax("path", &pax_path);
        let mut x = tar::Header::new_ustar();
        x.set_entry_type(tar::EntryType::XHeader);
        x.set_path("PaxHeaders/pax.txt").unwrap();
        x.set_size(record.len() as u64);
        x.set_cksum();
        b.append(&x, record.as_slice()).unwrap();
        let mut h = tar::Header::new_ustar();
        h.set_path("stand-in.txt").unwrap();
        h.set_mode(0o600);
        h.set_size(big.len() as u64);
        h.set_cksum();
        b.append(&h, big.as_slice()).unwrap();

        let mut sym = tar::Header::new_gnu();
        sym.set_entry_type(tar::EntryType::Symlink);
        sym.set_size(0);
        b.append_link(&mut sym, "game/links/far", &long_target)
            .unwrap();
        let mut hard = tar::Header::new_gnu();
        hard.set_entry_type(tar::EntryType::Link);
        hard.set_size(0);
        b.append_link(&mut hard, "game/links/hard.txt", &long)
            .unwrap();

        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_size(big.len() as u64);
        b.append_data(&mut h, "game/after.txt", big.as_slice())
            .unwrap();
        let path = d.join("links.tar.gz");
        fs::write(&path, gzip(&b.into_inner().unwrap(), &[], 6)).unwrap();
        path
    };

    for after in [0u32, 2, 5] {
        let tmp = TempDir::new().unwrap();
        let archive = build(tmp.path());
        let out = tmp.path().join("out");
        let after_s = after.to_string();
        let crashed = extract(
            &archive,
            &out,
            &stream_env(&[
                ("HATCHZIP_CRASH_AT", "stream-after-punch"),
                ("HATCHZIP_CRASH_AFTER", &after_s),
            ]),
        );
        assert!(!crashed.status.success(), "#{after} should have crashed");
        assert_ok(&extract(&archive, &out, &stream_env(&[])));
        assert_consumed(&archive);

        for p in [&long, &pax_path, &"game/after.txt".to_string()] {
            assert!(fs::read(out.join(p)).unwrap() == big, "{p} differs");
        }
        let pax_mode = fs::metadata(out.join(&pax_path)).unwrap().mode();
        assert_eq!(pax_mode & 0o777, 0o600);
        assert_eq!(
            fs::read_link(out.join("game/links/far")).unwrap(),
            Path::new(&long_target)
        );
        assert!(fs::read(out.join("game/links/far")).unwrap() == big);
        let hard = fs::metadata(out.join("game/links/hard.txt")).unwrap();
        assert_eq!(hard.nlink(), 2);
        assert!(!out.join("pax_global_header").exists());
        assert!(!out.join("stand-in.txt").exists());
    }
}

/// Damage anywhere in the stream is found by the walk through it that
/// opening the archive makes, before anything is touched.
#[test]
fn a_damaged_tar_gz_is_refused_before_anything_is_touched() {
    let files = files();
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path(), &files);
    let good = fs::read(&archive).unwrap();
    let n = good.len();
    let out = tmp.path().join("out");

    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    let mut late = good.clone();
    for b in &mut late[n * 9 / 10..n * 9 / 10 + 64] {
        *b ^= 0x5A;
    }
    cases.push(("", late));
    let mut crc = good.clone();
    crc[n - 8] ^= 1;
    cases.push(("CRC mismatch", crc));
    let mut size = good.clone();
    size[n - 2] ^= 1;
    cases.push(("length mismatch", size));
    cases.push(("ends early", good[..n - 100].to_vec()));
    let mut garbage = good.clone();
    garbage.extend(b"not gzip");
    cases.push(("unexpected data", garbage));

    for (expect, bytes) in cases {
        fs::write(&archive, &bytes).unwrap();
        let run = extract(&archive, &out, &stream_env(&[]));
        assert_eq!(
            run.status.code(),
            Some(4),
            "{expect}: should be refused as damaged"
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
fn the_space_check_for_a_tar_gz_counts_freeing_only_where_it_works() {
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
    // NTFS frees in units of up to 1 MiB, so the plan keeps a granule at each
    // end of the punched range there, which eats most of the saving on an
    // archive this small. It must still need less.
    let saving = if cfg!(windows) { 0.0 } else { 2e6 };
    assert!(
        streaming + saving < whole,
        "with punching it should need less than {whole}, not {streaming}"
    );
}

/// On a filesystem that can't punch holes, the archive stays whole until
/// the end, and a crash still resumes from the last point.
#[test]
fn without_hole_punching_a_tar_gz_is_kept_whole_and_still_resumes() {
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
fn restore_refuses_a_partly_freed_tar_gz() {
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

/// Punching reaches across volumes, and every volume is gone at the end.
#[test]
fn a_split_tar_gz_resumes_across_volumes() {
    let files = files();
    for point in ["stream-after-journal", "stream-after-punch"] {
        let tmp = TempDir::new().unwrap();
        let bytes = gzip(&tar_bytes(&files), &[], 6);
        let volumes = split(&bytes, &tmp.path().join("game.tar.gz"), 3);
        let out = tmp.path().join("out");
        let crash = stream_env(&[("HATCHZIP_CRASH_AT", point), ("HATCHZIP_CRASH_AFTER", "6")]);
        assert!(!extract(&volumes[0], &out, &crash).status.success());
        // Past the first volume by now; all of them are still there.
        assert!(resume_byte(&volumes[0]).unwrap() > fs::metadata(&volumes[0]).unwrap().len());
        assert!(volumes.iter().all(|v| v.exists()));

        assert_ok(&extract(&volumes[1], &out, &stream_env(&[])));
        assert_tree(&out, &files);
        assert_consumed(&volumes[0]);
        assert!(volumes.iter().all(|v| !v.exists()));
    }
}

/// Archives from the real tools: `tar -czf` (GNU tar with gzip on Linux,
/// bsdtar on macOS), `gzip -1`, and `pigz` with its default shared
/// dictionary and with independent blocks. Skipped for a tool that isn't
/// installed. Not on Windows, where how bsdtar stores the Unicode names of
/// the sample is not checked.
#[cfg(unix)]
#[test]
fn archives_from_tar_gzip_and_pigz_extract_and_resume() {
    use std::process::{Command, Stdio};

    let files = files();
    let src = TempDir::new().unwrap();
    for (name, data) in &files {
        let p = src.path().join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, data).unwrap();
    }
    fs::create_dir_all(src.path().join(EMPTY_DIR)).unwrap();
    let src_dir = src.path().to_str().unwrap().to_string();

    let pipe = |into: &Path, compressor: &[&str]| -> bool {
        let Ok(mut tar) = Command::new("tar")
            .env("COPYFILE_DISABLE", "1")
            .args(["-cf", "-", "-C", &src_dir, "game"])
            .stdout(Stdio::piped())
            .spawn()
        else {
            return false;
        };
        let Ok(made) = Command::new(compressor[0])
            .args(&compressor[1..])
            .stdin(tar.stdout.take().unwrap())
            .stdout(fs::File::create(into).unwrap())
            .status()
        else {
            let _ = tar.wait();
            return false;
        };
        tar.wait().unwrap().success() && made.success()
    };
    type Maker<'a> = Box<dyn Fn(&Path) -> bool + 'a>;
    let makers: Vec<(&str, Maker)> = vec![
        (
            "tar -czf",
            Box::new(|p: &Path| {
                // Keeps macOS bsdtar from adding `._` files for extended
                // attributes.
                Command::new("tar")
                    .env("COPYFILE_DISABLE", "1")
                    .args(["-czf", p.to_str().unwrap(), "-C", &src_dir, "game"])
                    .status()
                    .is_ok_and(|s| s.success())
            }),
        ),
        (
            "gzip -1",
            Box::new(|p: &Path| pipe(p, &["gzip", "-1", "-c"])),
        ),
        (
            "pigz",
            Box::new(|p: &Path| pipe(p, &["pigz", "-p", "4", "-c"])),
        ),
        (
            "pigz -i",
            Box::new(|p: &Path| pipe(p, &["pigz", "-i", "-p", "4", "-c"])),
        ),
    ];
    for (tool, make) in &makers {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("game.tar.gz");
        if !make(&archive) {
            eprintln!("{tool} not available; skipping");
            continue;
        }
        let original = fs::read(&archive).unwrap();
        let allocated_before = allocated(&archive);
        let out = tmp.path().join("out");
        let crash = stream_env(&[
            ("HATCHZIP_CRASH_AT", "stream-after-punch"),
            ("HATCHZIP_CRASH_AFTER", "2"),
        ]);
        let crashed = extract(&archive, &out, &crash);
        if crashed.status.success() {
            // `gzip -1` may write too few blocks to have three points.
            eprintln!("{tool}: finished before the third point");
        } else {
            check_crashed(
                &archive,
                &original,
                true,
                tool,
                &punch_probe_log(&crashed),
                allocated_before,
            );
            assert_ok(&extract(&archive, &out, &stream_env(&[])));
        }
        assert_tree(&out, &files);
        assert_consumed(&archive);
    }
}

#[test]
fn a_gzip_file_that_is_not_a_tar_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("notes.txt.gz");
    let bytes = gzip(&text(100_000), &[], 6);
    fs::write(&archive, &bytes).unwrap();
    let run = extract(&archive, &tmp.path().join("out"), &[]);
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("does not contain a tar archive"));
    assert_eq!(fs::read(&archive).unwrap(), bytes);
}
