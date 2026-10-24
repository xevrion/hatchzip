//! RAR volume sets made by the real WinRAR 7.x (`tests/fixtures/rar/`), each
//! extracted in place and compared byte for byte with the content they were
//! made from. Every set holds `sample()`.
//!
//! The fixtures are rebuilt from a tree that the ignored test
//! `write_sample_tree` writes (`HATCHZIP_RAR_SAMPLE_DIR=<empty folder> cargo
//! test -p hatchzip --test rar -- --ignored`). From inside that folder:
//!
//!   rar a -r -ma5 -m3 -v40k -ep1 <fixtures>/rar5/set.rar game
//!   rar a -r -ma5 -m3 -v40k -ep1 -s <fixtures>/rar5-solid/set.rar game
//!   rar a -r -ma5 -m3 -v40k -ep1 -psecret <fixtures>/rar5-pw/set.rar game
//!
//! WinRAR 7 can't write RAR 4, so those volumes are written by the test.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;
use tempfile::TempDir;

fn sample() -> Files {
    let mut files = Files::new();
    // Big enough to span volumes, in an order that gives each volume files
    // of its own to free.
    for (i, (name, len)) in [
        ("a.pak", 90_000),
        ("b.pak", 70_000),
        ("c.pak", 110_000),
        ("d.pak", 30_000),
    ]
    .into_iter()
    .enumerate()
    {
        files.insert(format!("game/{name}"), noise(len, i as u64 + 1));
    }
    files.insert(
        "game/config/settings.ini".into(),
        b"volume=7\n".repeat(14_000),
    );
    files.insert("game/empty.txt".into(), Vec::new());
    files.insert(
        "game/naïve résumé/日本語.txt".into(),
        "unicode names survive\n".into(),
    );
    for i in 0..10 {
        files.insert(
            format!("game/many/file{i:02}.txt"),
            format!("file number {i}\n").into(),
        );
    }
    files
}

#[test]
#[ignore = "regenerates the fixtures' source tree"]
fn write_sample_tree() {
    let dir = PathBuf::from(std::env::var("HATCHZIP_RAR_SAMPLE_DIR").unwrap());
    for (name, data) in sample() {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }
    fs::create_dir_all(dir.join(EMPTY_DIR)).unwrap();
}

/// Copies a fixture set into `to` and returns the volume to extract.
fn copy_set(set: &str, first: &str, to: &Path) -> PathBuf {
    let from = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rar")
        .join(set);
    for e in fs::read_dir(&from).unwrap() {
        let e = e.unwrap();
        fs::copy(e.path(), to.join(e.file_name())).unwrap();
    }
    to.join(first)
}

fn volumes(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.contains("hatchzip") && n != "out")
        .collect();
    v.sort();
    v
}

fn extracts(set: &str, first: &str) {
    let tmp = TempDir::new().unwrap();
    let archive = copy_set(set, first, tmp.path());
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &sample());
    assert_consumed(&archive);
    assert!(volumes(tmp.path()).is_empty(), "no volume should be left");
}

#[test]
fn rar5_volumes() {
    extracts("rar5", "set.part01.rar");
}

#[test]
fn any_volume_name_finds_the_set() {
    let tmp = TempDir::new().unwrap();
    copy_set("rar5", "set.part01.rar", tmp.path());
    let out = hatchzip(
        &["list", tmp.path().join("set.part02.rar").to_str().unwrap()],
        &[],
    );
    assert_ok(&out);
    assert!(String::from_utf8_lossy(&out.stdout).contains("a.pak"));
}

#[test]
fn crash_and_resume_rar5() {
    let files = sample();
    crash_and_resume(
        |dir| copy_set("rar5", "set.part01.rar", dir),
        &files,
        &[0, 1, 2],
    );
}

/// The point of it all: volumes go while the extraction is still running,
/// and what is left is a prefix of the set.
#[test]
fn volumes_are_deleted_as_it_goes() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_set("rar5", "set.part01.rar", tmp.path());
    let before = volumes(tmp.path());
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
    let left: Vec<String> = volumes(tmp.path())
        .into_iter()
        .filter(|n| !n.ends_with(".hatchzip") && !n.ends_with(".hatchzip-tail"))
        .collect();
    assert!(
        left.len() < before.len(),
        "nothing was freed: {left:?} of {before:?}"
    );
    assert_eq!(left[..], before[..left.len()], "what is left is a prefix");
}

#[test]
fn solid_archives_are_refused_untouched() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_set("rar5-solid", "set.part01.rar", tmp.path());
    let before = volumes(tmp.path());
    let out = extract(&archive, &tmp.path().join("out"), &[]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("solid"), "{err}");
    assert_eq!(volumes(tmp.path()), before);
    assert!(!tmp.path().join("out").exists());
}

#[test]
fn a_missing_volume_is_refused_untouched() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_set("rar5", "set.part01.rar", tmp.path());
    fs::remove_file(tmp.path().join("set.part04.rar")).unwrap();
    let before = volumes(tmp.path());
    let out = extract(&archive, &tmp.path().join("out"), &[]);
    assert!(!out.status.success());
    assert_eq!(volumes(tmp.path()), before);
}

// -- RAR 4 ---------------------------------------------------------------
//
// WinRAR 7 can't write RAR 4 any more, so these volumes are written here:
// stored entries only, which is all the layout needs. UnRAR reads them like
// any other, and the checksums are the real ones.

#[derive(Clone, Copy)]
enum Naming {
    /// `set.rar`, `set.r00`, `set.r01`, ...
    Old,
    /// `set.part1.rar`, `set.part2.rar`, ...
    Part,
}

fn crc16(bytes: &[u8]) -> u16 {
    (crc32fast::hash(bytes) & 0xFFFF) as u16
}

fn block(kind: u8, flags: u16, body: &[u8]) -> Vec<u8> {
    let mut b = vec![0, 0, kind];
    b.extend(flags.to_le_bytes());
    b.extend(((body.len() + 7) as u16).to_le_bytes());
    b.extend(body);
    let crc = crc16(&b[2..]);
    b[..2].copy_from_slice(&crc.to_le_bytes());
    b
}

fn file_header(
    name: &str,
    dir: bool,
    chunk: &[u8],
    total: &[u8],
    before: bool,
    after: bool,
) -> Vec<u8> {
    let mut flags = 0x8000u16;
    flags |= u16::from(before) | u16::from(after) << 1;
    if dir {
        flags |= 0xE0;
    }
    let mut body = Vec::new();
    body.extend((chunk.len() as u32).to_le_bytes());
    body.extend((total.len() as u32).to_le_bytes());
    body.push(2); // Windows
    body.extend(crc32fast::hash(total).to_le_bytes());
    body.extend(0x5821_0000u32.to_le_bytes()); // 2024-01-01 00:00:00, DOS time
    body.push(29);
    body.push(0x30); // stored
    body.extend((name.len() as u16).to_le_bytes());
    body.extend(if dir { 0x10u32 } else { 0x20 }.to_le_bytes());
    body.extend(name.as_bytes());
    let mut out = block(0x74, flags, &body);
    out.extend(chunk);
    out
}

/// Writes `entries` (name, data, is a folder) as volumes of at most `cap`
/// bytes, a file running from one into the next as WinRAR would split it.
fn write_rar4(
    dir: &Path,
    entries: &[(String, Vec<u8>, bool)],
    cap: usize,
    naming: Naming,
) -> PathBuf {
    let mut volumes: Vec<Vec<u8>> = vec![Vec::new()];
    let room = |v: &Vec<u8>| cap - 7 - 13 - 7 - v.len();
    for (name, data, is_dir) in entries {
        let name = name.replace('/', "\\");
        let mut at = 0;
        let mut first = true;
        loop {
            let header = 32 + name.len();
            if room(volumes.last().unwrap()) < header + 1 {
                volumes.push(Vec::new());
            }
            let left = room(volumes.last().unwrap()) - header;
            let take = (data.len() - at).min(left);
            let last = at + take == data.len();
            let chunk = &data[at..at + take];
            volumes
                .last_mut()
                .unwrap()
                .extend(file_header(&name, *is_dir, chunk, data, !first, !last));
            at += take;
            first = false;
            if last {
                break;
            }
            volumes.push(Vec::new());
        }
    }

    let name_of = |i: usize| match naming {
        Naming::Old if i == 0 => "set.rar".to_string(),
        Naming::Old => format!("set.r{:02}", i - 1),
        Naming::Part => format!("set.part{}.rar", i + 1),
    };
    let count = volumes.len();
    for (i, body) in volumes.iter().enumerate() {
        let mut main = 0x0001u16;
        if i == 0 {
            main |= 0x0100;
        }
        if matches!(naming, Naming::Part) {
            main |= 0x0010;
        }
        let mut file = b"Rar!\x1A\x07\x00".to_vec();
        file.extend(block(0x73, main, &[0; 6]));
        file.extend(body);
        file.extend(block(0x7B, 0x4000 | u16::from(i + 1 < count), &[]));
        fs::write(dir.join(name_of(i)), file).unwrap();
    }
    dir.join(name_of(0))
}

fn rar4_entries() -> (Files, Vec<(String, Vec<u8>, bool)>) {
    let files: Files = sample().into_iter().filter(|(n, _)| n.is_ascii()).collect();
    let mut entries: Vec<(String, Vec<u8>, bool)> = files
        .iter()
        .map(|(n, d)| (n.clone(), d.clone(), false))
        .collect();
    entries.push((EMPTY_DIR.to_string(), Vec::new(), true));
    (files, entries)
}

fn rar4(naming: Naming) {
    let (files, entries) = rar4_entries();
    let tmp = TempDir::new().unwrap();
    let archive = write_rar4(tmp.path(), &entries, 40_000, naming);
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &files);
    assert_consumed(&archive);
    assert!(volumes(tmp.path()).is_empty(), "no volume should be left");
}

#[test]
fn rar4_old_style_volumes() {
    rar4(Naming::Old);
}

#[test]
fn rar4_numbered_volumes() {
    rar4(Naming::Part);
}

#[test]
fn crash_and_resume_rar4() {
    let (files, entries) = rar4_entries();
    crash_and_resume(
        |dir| write_rar4(dir, &entries, 40_000, Naming::Old),
        &files,
        &[0, 1, 2],
    );
}

// -- Passwords -----------------------------------------------------------

const PW: &str = "secret";

fn extract_with(archive: &Path, out: &Path, password: Option<&str>) -> std::process::Output {
    let (a, o) = (archive.to_str().unwrap(), out.to_str().unwrap());
    let mut args = vec!["extract", a, "-o", o, "--keep-free", "0", "--yes"];
    if let Some(p) = password {
        args.extend(["--password", p]);
    }
    hatchzip(&args, &[])
}

#[test]
fn password_protected_volumes_extract_with_the_password() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_set("rar5-pw", "set.part01.rar", tmp.path());
    let out = tmp.path().join("out");
    assert_ok(&extract_with(&archive, &out, Some(PW)));
    assert_tree(&out, &sample());
    assert_consumed(&archive);
}

#[test]
fn a_wrong_or_missing_password_changes_nothing() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_set("rar5-pw", "set.part01.rar", tmp.path());
    let before = volumes(tmp.path());
    for password in [None, Some("wrong")] {
        let out = extract_with(&archive, &tmp.path().join("out"), password);
        assert!(!out.status.success(), "{password:?} should be refused");
        assert_eq!(
            volumes(tmp.path()),
            before,
            "{password:?} touched the archive"
        );
        assert!(!tmp.path().join("out").exists());
    }
}
