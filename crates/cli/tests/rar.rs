//! RAR archives, extracted in place file by file and compared byte for byte
//! with what they were made from.
//!
//! Two kinds of archives: sets made by the real RAR (`tests/fixtures/rar/`),
//! and archives of stored files written here, for layouts a fixture can't
//! cover cheaply (big files, hostile names, many volumes). The fixtures hold
//! `sample()` or `small()`; they are rebuilt from the trees the ignored test
//! `write_sample_trees` writes (`HATCHZIP_RAR_SAMPLE_DIR=<empty folder>
//! cargo test -p hatchzip --features rar --test rar -- --ignored`), with RAR
//! 6.24 for Linux, from inside `<folder>/sample` or `<folder>/small`:
//!
//!   sample: rar a -r -idq -ma5 -m3 -v16k     <fixtures>/rar5/set.rar game
//!   sample: rar a -r -idq -ma4 -m3 -v16k -vn <fixtures>/rar4/set.rar game
//!   small:  rar a -r -idq -ma5 -m3 -v4k -psecret <fixtures>/rar5-pw/set.rar game
//!   small:  rar a -r -idq -ma4 -m3 -psecret  <fixtures>/rar4-pw/set.rar game
//!   small:  rar a -r -idq -ma5 -m3 -v4k -s   <fixtures>/rar5-solid/set.rar game
//!   small:  rar a -r -idq -ma4 -m3 -s        <fixtures>/rar4-solid/set.rar game
//!   small:  rar a -r -idq -ma5 -m3 -qo+ -rr5% -zREADME <fixtures>/rar5-extras/set.rar game
//!   small:  rar a -r -idq -ma5 -m3 -hpsecret <fixtures>/rar5-hp/set.rar game
//!
//! `README` in the small tree is the archive comment, not part of it.

#![cfg(feature = "rar")]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::*;
use tempfile::TempDir;

fn sample() -> Files {
    let mut files = small();
    // Big enough to span volumes of 16 KB, in an order that gives most
    // volumes files of their own.
    for (i, (name, len)) in [
        ("a.pak", 30_000),
        ("b.pak", 20_000),
        ("c.pak", 36_000),
        ("e.pak", 6_000),
    ]
    .into_iter()
    .enumerate()
    {
        files.insert(format!("game/{name}"), noise(len, i as u64 + 1));
    }
    files
}

fn small() -> Files {
    let mut files = Files::new();
    files.insert("game/d.pak".into(), noise(6_000, 9));
    files.insert(
        "game/config/settings.ini".into(),
        b"volume=7\n".repeat(2_000),
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
#[ignore = "regenerates the fixtures' source trees"]
fn write_sample_trees() {
    let dir = PathBuf::from(std::env::var("HATCHZIP_RAR_SAMPLE_DIR").unwrap());
    for (tree, files) in [("sample", sample()), ("small", small())] {
        for (name, data) in files {
            let path = dir.join(tree).join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, data).unwrap();
        }
        fs::create_dir_all(dir.join(tree).join(EMPTY_DIR)).unwrap();
    }
    fs::write(dir.join("small/README"), "an archive comment\n").unwrap();
}

/// Copies a fixture set into `to` and returns its first volume.
fn copy_set(set: &str, to: &Path) -> PathBuf {
    let from = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rar")
        .join(set);
    let mut first = None;
    for e in fs::read_dir(&from).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().into_owned();
        fs::copy(e.path(), to.join(&name)).unwrap();
        if name == "set.rar" || name == "set.part1.rar" {
            first = Some(to.join(&name));
        }
    }
    first.unwrap_or_else(|| panic!("{set} has no first volume"))
}

/// The volumes in `dir`, sorted, without HatchZip's side files.
fn volumes(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("set.") && !n.contains("hatchzip"))
        .collect();
    v.sort();
    v
}

/// Every volume in `dir` with its bytes.
fn snapshot_dir(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    volumes(dir)
        .into_iter()
        .map(|n| {
            let p = dir.join(n);
            let bytes = fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect()
}

fn on_disk(dir: &Path) -> u64 {
    volumes(dir)
        .iter()
        .map(|n| fs::metadata(dir.join(n)).unwrap().len())
        .sum()
}

fn committed_len(archive: &Path) -> u64 {
    let [journal, _] = side_files(archive);
    let j: serde_json::Value = serde_json::from_slice(&fs::read(journal).unwrap()).unwrap();
    j["committed_len"].as_u64().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn extract_with(archive: &Path, out: &Path, password: Option<&str>) -> Output {
    let (a, o) = (archive.to_str().unwrap(), out.to_str().unwrap());
    let mut args = vec!["extract", a, "-o", o, "--keep-free", "0", "--yes"];
    if let Some(p) = password {
        args.extend(["--password", p]);
    }
    hatchzip(&args, &[])
}

fn extracts(build: impl Fn(&Path) -> PathBuf, files: &Files) {
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path());
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, files);
    assert_consumed(&archive);
    assert!(volumes(tmp.path()).is_empty(), "no volume should be left");
}

/// The run fails and nothing changed: every volume as it was, no side
/// files, no output.
fn refused_untouched(tmp: &Path, out: &Output) {
    let _ = out;
    assert!(!tmp.join("out").exists(), "the output folder was created");
    assert!(
        fs::read_dir(tmp).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("hatchzip")),
        "side files were left"
    );
}

// -- Archives written here --------------------------------------------------
//
// Stored files only, which is all a layout needs. UnRAR reads these like any
// other, and the checksums are the real ones.

#[derive(Clone, Copy, PartialEq)]
enum Naming {
    /// `set.rar`, `set.r00`, `set.r01`, ...
    Old,
    /// `set.part1.rar`, `set.part2.rar`, ...
    Part,
}

struct Entry {
    /// The name as stored in the header. In RAR 4, a UTF-8 name that is not
    /// ASCII gets its Unicode form as well, as RAR writes it; other bytes
    /// are a legacy name in the OEM code page, with no Unicode form.
    name: Vec<u8>,
    data: Vec<u8>,
    dir: bool,
    /// RAR 5 compression information: 0 is RAR 5's algorithm, stored.
    info: u64,
}

fn entry(name: &str, data: Vec<u8>) -> Entry {
    Entry {
        name: name.as_bytes().to_vec(),
        data,
        dir: false,
        info: 0,
    }
}

fn entries_of(files: &Files) -> Vec<Entry> {
    let mut entries: Vec<Entry> = files.iter().map(|(n, d)| entry(n, d.clone())).collect();
    entries.push(Entry {
        name: EMPTY_DIR.as_bytes().to_vec(),
        data: Vec::new(),
        dir: true,
        info: 0,
    });
    entries
}

fn crc16(bytes: &[u8]) -> u16 {
    (crc32fast::hash(bytes) & 0xFFFF) as u16
}

fn block4(kind: u8, flags: u16, body: &[u8]) -> Vec<u8> {
    let mut b = vec![0, 0, kind];
    b.extend(flags.to_le_bytes());
    b.extend(((body.len() + 7) as u16).to_le_bytes());
    b.extend(body);
    let crc = crc16(&b[2..]);
    b[..2].copy_from_slice(&crc.to_le_bytes());
    b
}

fn vint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A RAR 5 header: CRC-32, size, then `body` (type and flags first).
fn block5(body: &[u8]) -> Vec<u8> {
    let mut sized = Vec::new();
    vint(body.len() as u64, &mut sized);
    sized.extend(body);
    let mut b = crc32fast::hash(&sized).to_le_bytes().to_vec();
    b.extend(sized);
    b
}

/// The Unicode form of a RAR 4 name, which follows the plain form and a NUL,
/// encoded as RAR does: a high byte, then two-bit opcodes, four to a flags
/// byte from the top bits down, each followed by its operands. 0 is a
/// character below 256, 1 a character in the high byte's page, 2 any UTF-16
/// unit, 3 a run of `n + 2` characters copied from the plain form at the
/// same positions.
fn unicode_name4(plain: &[u8], name: &str) -> Vec<u8> {
    let wide: Vec<u16> = name.encode_utf16().collect();
    let high = wide.iter().map(|&c| c >> 8).find(|&h| h != 0).unwrap_or(0) as u8;
    let mut ops: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut i = 0;
    while i < wide.len() {
        let run = wide[i..]
            .iter()
            .zip(plain.get(i..).unwrap_or_default())
            .take_while(|&(&c, &b)| b.is_ascii() && c == u16::from(b))
            .count()
            .min(0x7F + 2);
        if run >= 3 {
            ops.push((3, vec![(run - 2) as u8]));
            i += run;
            continue;
        }
        let c = wide[i];
        ops.push(match c >> 8 {
            0 => (0, vec![c as u8]),
            h if h == u16::from(high) => (1, vec![c as u8]),
            _ => (2, c.to_le_bytes().to_vec()),
        });
        i += 1;
    }
    let mut out = vec![high];
    for group in ops.chunks(4) {
        let flags = group
            .iter()
            .enumerate()
            .fold(0u8, |f, (k, (op, _))| f | op << (6 - 2 * k));
        out.push(flags);
        for (_, operands) in group {
            out.extend(operands);
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Version {
    Four,
    Five,
}

/// The header of one piece of a file, `chunk` of `total`, followed by it.
fn file_piece(v: Version, e: &Entry, chunk: &[u8], before: bool, after: bool) -> Vec<u8> {
    match v {
        Version::Four => {
            let mut flags = 0x8000u16 | u16::from(before) | u16::from(after) << 1;
            if e.dir {
                flags |= 0xE0;
            }
            let mut name: Vec<u8> = e
                .name
                .iter()
                .map(|&b| if b == b'/' { b'\\' } else { b })
                .collect();
            // Without its Unicode form, UnRAR reads a name in the OEM code
            // page on Windows and as UTF-8 elsewhere.
            if let Ok(text) = std::str::from_utf8(&name).map(str::to_string)
                && !text.is_ascii()
            {
                flags |= 0x200;
                let unicode = unicode_name4(&name, &text);
                name.push(0);
                name.extend(unicode);
            }
            let mut body = Vec::new();
            body.extend((chunk.len() as u32).to_le_bytes());
            body.extend((e.data.len() as u32).to_le_bytes());
            body.push(2); // Windows
            body.extend(crc32fast::hash(&e.data).to_le_bytes());
            body.extend(0x5821_0000u32.to_le_bytes()); // 2024-01-01, DOS time
            body.push(29);
            body.push(0x30); // stored
            body.extend((name.len() as u16).to_le_bytes());
            body.extend(if e.dir { 0x10u32 } else { 0x20 }.to_le_bytes());
            body.extend(&name);
            let mut out = block4(0x74, flags, &body);
            out.extend(chunk);
            out
        }
        Version::Five => {
            let mut body = Vec::new();
            vint(2, &mut body); // file header
            vint(
                0x02 | u64::from(before) << 3 | u64::from(after) << 4,
                &mut body,
            );
            vint(chunk.len() as u64, &mut body);
            vint(u64::from(e.dir) | 0x04, &mut body); // CRC-32 present
            vint(e.data.len() as u64, &mut body);
            vint(if e.dir { 0x10 } else { 0x20 }, &mut body);
            body.extend(crc32fast::hash(&e.data).to_le_bytes());
            vint(e.info, &mut body);
            vint(0, &mut body); // Windows
            vint(e.name.len() as u64, &mut body);
            body.extend(&e.name);
            let mut out = block5(&body);
            out.extend(chunk);
            out
        }
    }
}

/// Writes `entries` as volumes of at most `cap` bytes (one volume if `cap`
/// is `None`), a file running from one into the next as RAR splits it.
fn write_rar(
    dir: &Path,
    v: Version,
    entries: &[Entry],
    cap: Option<usize>,
    naming: Naming,
) -> PathBuf {
    let cap = cap.unwrap_or(usize::MAX / 2);
    let overhead = match v {
        Version::Four => 7 + 13 + 7,
        Version::Five => 8 + 16 + 8,
    };
    let mut volumes: Vec<Vec<u8>> = vec![Vec::new()];
    let room = |vol: &Vec<u8>| cap - overhead - vol.len();
    for e in entries {
        let header = file_piece(v, e, &[], false, false).len() + 2;
        let mut at = 0;
        let mut first = true;
        loop {
            if room(volumes.last().unwrap()) < header + 1 {
                volumes.push(Vec::new());
            }
            let left = room(volumes.last().unwrap()) - header;
            let take = (e.data.len() - at).min(left);
            let last = at + take == e.data.len();
            let chunk = &e.data[at..at + take];
            volumes
                .last_mut()
                .unwrap()
                .extend(file_piece(v, e, chunk, !first, !last));
            at += take;
            first = false;
            if last {
                break;
            }
            volumes.push(Vec::new());
        }
    }

    let count = volumes.len();
    let multi = count > 1;
    let name_of = |i: usize| match naming {
        Naming::Old if i == 0 => "set.rar".to_string(),
        Naming::Old => format!("set.r{:02}", i - 1),
        Naming::Part => format!("set.part{}.rar", i + 1),
    };
    for (i, body) in volumes.iter().enumerate() {
        let more = i + 1 < count;
        let mut file = Vec::new();
        match v {
            Version::Four => {
                let mut main = u16::from(multi);
                if i == 0 {
                    main |= 0x0100;
                }
                if naming == Naming::Part {
                    main |= 0x0010;
                }
                file.extend(b"Rar!\x1A\x07\x00");
                file.extend(block4(0x73, main, &[0; 6]));
                file.extend(body);
                file.extend(block4(0x7B, 0x4000 | u16::from(more), &[]));
            }
            Version::Five => {
                file.extend(b"Rar!\x1A\x07\x01\x00");
                let mut main = Vec::new();
                vint(1, &mut main);
                vint(0, &mut main);
                if multi {
                    vint(0x01 | 0x02, &mut main);
                    vint(i as u64, &mut main);
                } else {
                    vint(0, &mut main);
                }
                file.extend(block5(&main));
                file.extend(body);
                let mut end = Vec::new();
                vint(5, &mut end);
                vint(0, &mut end);
                vint(u64::from(more), &mut end);
                file.extend(block5(&end));
            }
        }
        let name = if multi || naming == Naming::Part {
            name_of(i)
        } else {
            "set.rar".to_string()
        };
        fs::write(dir.join(name), file).unwrap();
    }
    dir.join(if multi || naming == Naming::Part {
        name_of(0)
    } else {
        "set.rar".to_string()
    })
}

// -- Extraction --------------------------------------------------------------

#[test]
fn rar5_volumes_made_by_rar() {
    extracts(|d| copy_set("rar5", d), &sample());
}

#[test]
fn rar4_volumes_made_by_rar() {
    extracts(|d| copy_set("rar4", d), &sample());
}

#[test]
fn quick_open_data_recovery_record_and_comment() {
    extracts(|d| copy_set("rar5-extras", d), &small());
}

#[test]
fn rar4_written_here_with_both_namings() {
    for naming in [Naming::Old, Naming::Part] {
        extracts(
            |d| {
                write_rar(
                    d,
                    Version::Four,
                    &entries_of(&sample()),
                    Some(20_000),
                    naming,
                )
            },
            &sample(),
        );
    }
}

#[test]
fn one_rar5_file_written_here() {
    extracts(
        |d| write_rar(d, Version::Five, &entries_of(&sample()), None, Naming::Old),
        &sample(),
    );
}

#[test]
fn any_volume_name_finds_the_set() {
    let tmp = TempDir::new().unwrap();
    copy_set("rar5", tmp.path());
    let out = hatchzip(
        &["list", tmp.path().join("set.part2.rar").to_str().unwrap()],
        &[],
    );
    assert_ok(&out);
    assert!(String::from_utf8_lossy(&out.stdout).contains("a.pak"));
}

/// UnRAR opens the volumes by name, which on Linux goes through its own
/// conversion of the path.
#[test]
fn a_set_in_a_folder_with_a_non_ascii_name() {
    extracts(
        |d| {
            let folder = d.join("Téléchargements 下载");
            fs::create_dir(&folder).unwrap();
            copy_set("rar5", &folder)
        },
        &sample(),
    );
}

#[test]
fn test_reads_every_file() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_set("rar4", tmp.path());
    let before = snapshot_dir(tmp.path());
    let out = hatchzip(&["test", archive.to_str().unwrap()], &[]);
    assert_ok(&out);
    assert_unchanged(&before);
}

// -- Space is freed file by file ----------------------------------------------

/// Stops a run after each number of commits in turn and checks that every
/// commit gave space back: what is on disk is exactly what the journal says
/// is left, and it shrinks with every commit, inside a volume as well as a
/// volume at a time.
fn frees_after_every_batch(build: impl Fn(&Path) -> PathBuf) {
    let mut last = u64::MAX;
    let mut commits = 0;
    for after in 1u32.. {
        let tmp = TempDir::new().unwrap();
        let archive = build(tmp.path());
        let original = on_disk(tmp.path());
        let n = after.to_string();
        let crashed = extract(
            &archive,
            &tmp.path().join("out"),
            &[
                ("HATCHZIP_BATCH_ENTRIES", "1"),
                ("HATCHZIP_CRASH_AT", "after-extract"),
                ("HATCHZIP_CRASH_AFTER", &n),
            ],
        );
        if crashed.status.success() {
            break;
        }
        // `after` batches are committed and truncated; the next is not.
        let left = on_disk(tmp.path());
        assert_eq!(left, committed_len(&archive), "after {after} commits");
        assert!(
            left < last,
            "commit {after} freed nothing: {left} of {original}"
        );
        last = left;
        commits = after;
    }
    assert!(commits > 3, "only {commits} commits");
}

#[test]
fn space_is_freed_after_every_file_in_a_set() {
    frees_after_every_batch(|d| copy_set("rar5", d));
    frees_after_every_batch(|d| copy_set("rar4", d));
}

#[test]
fn space_is_freed_after_every_file_in_a_single_volume() {
    frees_after_every_batch(|d| {
        write_rar(d, Version::Five, &entries_of(&sample()), None, Naming::Old)
    });
    frees_after_every_batch(|d| {
        write_rar(d, Version::Four, &entries_of(&sample()), None, Naming::Old)
    });
}

/// A file that runs over four volumes keeps them all until it is out, even
/// after the files behind it are, and they then go together.
#[test]
fn a_split_file_keeps_its_volumes_until_it_is_out() {
    let big = noise(70_000, 7);
    let entries = vec![
        entry("game/first.txt", b"first\n".to_vec()),
        entry("game/big.bin", big.clone()),
        entry("game/after.bin", noise(15_000, 8)),
        entry("game/last.txt", b"last\n".to_vec()),
    ];
    let build = |d: &Path| write_rar(d, Version::Five, &entries, Some(20_000), Naming::Part);
    let tmp = TempDir::new().unwrap();
    let archive = build(tmp.path());
    let all = volumes(tmp.path());
    assert!(all.len() >= 5, "{all:?}");
    // Two commits: last.txt, then after.bin. big.bin is next.
    let crashed = extract(
        &archive,
        &tmp.path().join("out"),
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-extract"),
            ("HATCHZIP_CRASH_AFTER", "2"),
        ],
    );
    assert!(!crashed.status.success());
    let left = volumes(tmp.path());
    // big.bin starts in volume 1 and ends in volume 4, where after.bin
    // begins; that volume is cut, not deleted. The run stopped after
    // writing big.bin, before committing it.
    let out = tmp.path().join("out");
    assert!(out.join("game/after.bin").exists());
    assert!(left.len() >= 4, "a volume big.bin needs is gone: {left:?}");
    let cut = on_disk(tmp.path());
    assert_eq!(cut, committed_len(&archive));
    assert_eq!(left[..], all[..left.len()], "what is left is a prefix");

    assert_ok(&extract(&archive, &out, &[]));
    assert_eq!(fs::read(out.join("game/big.bin")).unwrap(), big);
    assert_consumed(&archive);
}

// -- Crashes ---------------------------------------------------------------------

/// Kills the run at both crash points after every number of commits the run
/// makes, the last one included, and checks that a resume finishes it.
fn crash_everywhere(build: impl Fn(&Path) -> PathBuf, files: &Files) {
    for point in ["after-extract", "after-journal"] {
        for after in 0u32.. {
            let tmp = TempDir::new().unwrap();
            let archive = build(tmp.path());
            let out = tmp.path().join("out");
            let n = after.to_string();
            let crashed = extract(
                &archive,
                &out,
                &[
                    ("HATCHZIP_BATCH_ENTRIES", "1"),
                    ("HATCHZIP_CRASH_AT", point),
                    ("HATCHZIP_CRASH_AFTER", &n),
                ],
            );
            if crashed.status.success() {
                assert!(after > 0, "{point} never crashed");
                break;
            }
            let resumed = extract(&archive, &out, &[]);
            assert!(
                resumed.status.success(),
                "{point}#{after}: the resume failed\n{}",
                stderr(&resumed)
            );
            assert_tree(&out, files);
            assert_consumed(&archive);
        }
    }
}

#[test]
fn crash_at_every_commit_rar5_set() {
    crash_everywhere(|d| copy_set("rar5", d), &sample());
}

#[test]
fn crash_at_every_commit_rar4_set() {
    crash_everywhere(|d| copy_set("rar4", d), &sample());
}

#[test]
fn crash_at_every_commit_rar5_single_volume() {
    crash_everywhere(
        |d| write_rar(d, Version::Five, &entries_of(&sample()), None, Naming::Old),
        &sample(),
    );
}

#[test]
fn crash_at_every_commit_rar4_single_volume() {
    crash_everywhere(
        |d| write_rar(d, Version::Four, &entries_of(&sample()), None, Naming::Old),
        &sample(),
    );
}

#[test]
fn crash_at_every_commit_with_quick_open_data() {
    crash_everywhere(|d| copy_set("rar5-extras", d), &small());
}

/// Every run commits one file and dies right after its journal write, until
/// one finishes.
#[test]
fn crash_after_every_journal_write_in_a_row() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_set("rar5", tmp.path());
    let out = tmp.path().join("out");
    let crash = [
        ("HATCHZIP_BATCH_ENTRIES", "1"),
        ("HATCHZIP_CRASH_AT", "after-journal"),
        ("HATCHZIP_CRASH_AFTER", "0"),
    ];
    let mut last = String::new();
    for _ in 0..40 {
        let r = extract(&archive, &out, &crash);
        if r.status.success() {
            assert_tree(&out, &sample());
            assert_consumed(&archive);
            return;
        }
        last = stderr(&r);
    }
    panic!("never finished; the last run said:\n{last}");
}

// -- One big stored file -------------------------------------------------------

/// A stored file in one volume is plain bytes, so it is streamed like a tar
/// entry: its space is freed while it is written, at resume points that
/// survive a crash at each of their steps.
#[test]
fn a_big_stored_file_frees_space_as_it_goes() {
    const STEP: &str = "1048576";
    let mut files = small();
    files.insert("game/huge.bin".into(), noise(8_000_000, 11));
    let build = |d: &Path| write_rar(d, Version::Five, &entries_of(&files), None, Naming::Old);
    for point in [
        "stream-after-sync",
        "stream-after-journal",
        "stream-after-punch",
    ] {
        for after in [0u32, 3] {
            let tmp = TempDir::new().unwrap();
            let archive = build(tmp.path());
            let out = tmp.path().join("out");
            let n = after.to_string();
            let env = [
                ("HATCHZIP_STREAM_STEP", STEP),
                ("HATCHZIP_BATCH_ENTRIES", "1"),
            ];
            let mut crash = env.to_vec();
            crash.extend([("HATCHZIP_CRASH_AT", point), ("HATCHZIP_CRASH_AFTER", &n)]);
            let crashed = extract(&archive, &out, &crash);
            assert!(
                !crashed.status.success(),
                "{point}#{after} should have crashed"
            );
            let journal: serde_json::Value =
                serde_json::from_slice(&fs::read(&side_files(&archive)[0]).unwrap()).unwrap();
            if point != "stream-after-sync" || after > 0 {
                assert!(
                    !journal["resume"].is_null(),
                    "{point}#{after}: no resume point was saved"
                );
            }
            #[cfg(unix)]
            if point == "stream-after-punch" {
                use std::os::unix::fs::MetadataExt;
                let m = fs::metadata(&archive).unwrap();
                let freed = (after as u64 + 1) * 1_048_576 - 2 * 65_536;
                assert!(
                    m.blocks() * 512 + freed <= m.len().next_multiple_of(4096),
                    "{point}#{after}: {} of {} bytes still allocated",
                    m.blocks() * 512,
                    m.len()
                );
            }
            assert_ok(&extract(&archive, &out, &env));
            assert_tree(&out, &files);
            assert_consumed(&archive);
        }
    }
}

// -- Passwords -------------------------------------------------------------------

const PW: &str = "secret";

#[test]
fn password_protected_sets_extract_with_the_password() {
    for set in ["rar5-pw", "rar4-pw"] {
        let tmp = TempDir::new().unwrap();
        let archive = copy_set(set, tmp.path());
        let out = tmp.path().join("out");
        assert_ok(&extract_with(&archive, &out, Some(PW)));
        assert_tree(&out, &small());
        assert_consumed(&archive);
    }
}

/// A wrong password is "incorrect password", exit 5, for RAR 5 (by its check
/// value) and RAR 4 (by decoding a file), and nothing changes.
#[test]
fn a_wrong_or_missing_password_changes_nothing() {
    for set in ["rar5-pw", "rar4-pw"] {
        let tmp = TempDir::new().unwrap();
        let archive = copy_set(set, tmp.path());
        let before = snapshot_dir(tmp.path());
        for password in [None, Some("wrong"), Some("Secret")] {
            let out = extract_with(&archive, &tmp.path().join("out"), password);
            assert_eq!(
                out.status.code(),
                Some(5),
                "{set} {password:?}: {}",
                stderr(&out)
            );
            if password.is_some() {
                assert!(
                    stderr(&out).contains("incorrect password"),
                    "{set}: {}",
                    stderr(&out)
                );
            }
            assert_unchanged(&before);
            refused_untouched(tmp.path(), &out);
        }
    }
}

// -- Refused before anything is touched -------------------------------------------

#[test]
fn solid_archives_are_refused_in_place_and_extract_when_kept() {
    for set in ["rar5-solid", "rar4-solid"] {
        let tmp = TempDir::new().unwrap();
        let archive = copy_set(set, tmp.path());
        let before = snapshot_dir(tmp.path());
        let out = extract(&archive, &tmp.path().join("out"), &[]);
        assert!(!out.status.success());
        let err = stderr(&out);
        assert!(
            err.contains("solid") && err.contains("--keep-archive"),
            "{err}"
        );
        assert_unchanged(&before);
        refused_untouched(tmp.path(), &out);

        let kept = hatchzip(
            &[
                "extract",
                archive.to_str().unwrap(),
                "-o",
                tmp.path().join("out").to_str().unwrap(),
                "--keep-free",
                "0",
                "--keep-archive",
            ],
            &[],
        );
        assert_ok(&kept);
        assert_tree(&tmp.path().join("out"), &small());
        assert_unchanged(&before);
        assert_no_side_files(&archive);
    }
}

/// Kept, a solid set is one unit; a crash in it resumes from the start of
/// the set and the archive stays as it was.
#[test]
fn a_kept_solid_archive_resumes_after_a_crash() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_set("rar5-solid", tmp.path());
    let before = snapshot_dir(tmp.path());
    let out = tmp.path().join("out");
    let args = |a: &Path, o: &Path| {
        vec![
            "extract".to_string(),
            a.to_string_lossy().into_owned(),
            "-o".into(),
            o.to_string_lossy().into_owned(),
            "--keep-free".into(),
            "0".into(),
            "--keep-archive".into(),
        ]
    };
    let a = args(&archive, &out);
    let a: Vec<&str> = a.iter().map(String::as_str).collect();
    let crashed = hatchzip(&a, &[("HATCHZIP_CRASH_AT", "after-extract")]);
    assert!(!crashed.status.success());
    assert_unchanged(&before);
    assert_ok(&hatchzip(&a, &[]));
    assert_tree(&out, &small());
    assert_unchanged(&before);
}

#[test]
fn a_missing_volume_is_refused_untouched() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_set("rar5", tmp.path());
    fs::remove_file(tmp.path().join("set.part4.rar")).unwrap();
    let before = snapshot_dir(tmp.path());
    let out = extract(&archive, &tmp.path().join("out"), &[]);
    assert!(!out.status.success());
    assert_unchanged(&before);
    refused_untouched(tmp.path(), &out);
}

#[test]
fn a_damaged_header_is_refused_untouched_as_damage() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_set("rar5", tmp.path());
    let third = tmp.path().join("set.part3.rar");
    let mut bytes = fs::read(&third).unwrap();
    // Inside the first file header after the signature and main header.
    bytes[40] ^= 0xFF;
    fs::write(&third, bytes).unwrap();
    let before = snapshot_dir(tmp.path());
    let out = extract(&archive, &tmp.path().join("out"), &[]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_unchanged(&before);
    refused_untouched(tmp.path(), &out);
}

/// Names that would land outside the output folder are refused before
/// anything is touched, in RAR 4 (`\` separators) and RAR 5 (`/`).
#[test]
fn hostile_names_are_refused_untouched() {
    for (v, name) in [
        (Version::Four, "..\\evil.txt"),
        (Version::Four, "game\\..\\..\\evil.txt"),
        (Version::Four, "C:\\evil.txt"),
        (Version::Five, "../evil.txt"),
        (Version::Five, "game/../../evil.txt"),
        (Version::Five, "game/a\0b.txt"),
    ] {
        let tmp = TempDir::new().unwrap();
        let mut entries = entries_of(&small());
        entries.insert(0, entry(name, b"evil\n".to_vec()));
        let archive = write_rar(tmp.path(), v, &entries, None, Naming::Old);
        let before = snapshot_dir(tmp.path());
        let out = extract(&archive, &tmp.path().join("out"), &[]);
        assert!(!out.status.success(), "{name:?} was extracted");
        assert_unchanged(&before);
        refused_untouched(tmp.path(), &out);
        assert!(!tmp.path().join("evil.txt").exists());
    }
}

/// A leading slash is dropped, as tar does, and a `\` inside a RAR 5 name
/// is part of the name, which WinRAR writes as `_` on Windows; so does
/// HatchZip, everywhere.
#[test]
fn absolute_names_and_backslashes_land_inside() {
    let tmp = TempDir::new().unwrap();
    let mut entries = entries_of(&small());
    entries.push(entry("/game/rooted.txt", b"rooted\n".to_vec()));
    entries.push(entry("game/back\\slash.txt", b"slash\n".to_vec()));
    let archive = write_rar(tmp.path(), Version::Five, &entries, None, Naming::Old);
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &[]));
    let mut files = small();
    files.insert("game/rooted.txt".into(), b"rooted\n".to_vec());
    files.insert("game/back_slash.txt".into(), b"slash\n".to_vec());
    assert_tree(&out, &files);
}

/// Two names that differ only in case would race on a case-insensitive
/// disk, and are refused as for every format.
#[test]
fn names_differing_only_in_case_are_refused_untouched() {
    let tmp = TempDir::new().unwrap();
    let mut entries = entries_of(&small());
    entries.push(entry("game/D.PAK", b"other\n".to_vec()));
    let archive = write_rar(tmp.path(), Version::Five, &entries, None, Naming::Old);
    let before = snapshot_dir(tmp.path());
    let out = extract(&archive, &tmp.path().join("out"), &[]);
    assert!(!out.status.success());
    assert_unchanged(&before);
    refused_untouched(tmp.path(), &out);
}

/// Extracts a set whose volume 1 holds `first`, followed by two files big
/// enough to make later units that commit, and delete volumes, first. The
/// run must either finish or fail with every volume still there.
fn finishes_or_changes_nothing(first: Entry) -> bool {
    let tmp = TempDir::new().unwrap();
    let entries = vec![
        first,
        entry("game/a.pak", noise(30_000, 1)),
        entry("game/b.pak", noise(30_000, 2)),
    ];
    let archive = write_rar(
        tmp.path(),
        Version::Four,
        &entries,
        Some(20_000),
        Naming::Part,
    );
    let before = snapshot_dir(tmp.path());
    let out = extract(
        &archive,
        &tmp.path().join("out"),
        &[("HATCHZIP_BATCH_ENTRIES", "1")],
    );
    if out.status.success() {
        return true;
    }
    assert_unchanged(&before);
    false
}

/// RAR 2 and 3 stored a name that fits the OEM code page without its
/// Unicode form. Here, Latin-1 "café.txt".
#[test]
fn a_name_without_its_unicode_form() {
    finishes_or_changes_nothing(Entry {
        name: b"game\\caf\xe9.txt".to_vec(),
        data: b"oem\n".to_vec(),
        dir: false,
        info: 0,
    });
}

/// Refused as not supported (exit 1), not as damage, and untouched.
fn refused_as_unsupported(archive: &Path, dir: &Path, says: &str) {
    let before = snapshot_dir(dir);
    let out = extract(archive, &dir.join("out"), &[]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains(says), "{}", stderr(&out));
    assert_unchanged(&before);
    refused_untouched(dir, &out);
}

/// RAR 7 can compress with dictionaries up to 64 GB, which UnRAR refuses
/// past 4 GB unless asked, and which need that much memory. Refused when
/// the archive is opened, not when the file comes up.
#[test]
fn a_dictionary_over_4_gb_is_refused_untouched() {
    let tmp = TempDir::new().unwrap();
    let mut entries = entries_of(&small());
    // Algorithm version 1 (RAR 7), dictionary 128 KB << 16: 8 GB.
    entries.push(Entry {
        info: 1 | 16 << 10,
        ..entry("game/huge-dictionary.bin", b"data\n".to_vec())
    });
    let archive = write_rar(tmp.path(), Version::Five, &entries, None, Naming::Old);
    refused_as_unsupported(&archive, tmp.path(), "8 GB dictionary");
}

/// Archives with encrypted file names are not supported yet; that is not
/// damage.
#[test]
fn encrypted_file_names_are_refused_as_unsupported() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_set("rar5-hp", tmp.path());
    refused_as_unsupported(&archive, tmp.path(), "encrypted file names");
}

/// Longer than the 1023 characters of UnRAR's `FileNameW`; read in full
/// through `FileNameEx`.
#[test]
fn a_name_longer_than_1024_characters_extracts() {
    let dir = "d".repeat(240);
    let name = format!("game/{dir}/{dir}/{dir}/{dir}/{dir}/deep.txt");
    assert!(name.len() > 1024);
    let finished = finishes_or_changes_nothing(entry(&name, b"deep\n".to_vec()));
    // macOS can't create a path this long (1024 bytes at most), so there it
    // must be refused with the archive untouched instead.
    assert!(finished || cfg!(target_os = "macos"));
}

/// Volumes of an older, longer set with the same name sit after the last
/// volume of this one. The last volume says no more follow, so they are not
/// part of it and must be left alone.
#[test]
fn volumes_after_the_last_one_are_not_part_of_the_set() {
    let tmp = TempDir::new().unwrap();
    let set = |dir: &str, n: u64| -> Vec<Entry> {
        (1..=n)
            .map(|i| entry(&format!("{dir}/f{i}.bin"), noise(15_000, i)))
            .collect()
    };
    write_rar(
        tmp.path(),
        Version::Four,
        &set("old", 5),
        Some(20_000),
        Naming::Part,
    );
    let old = volumes(tmp.path()).len();
    let archive = write_rar(
        tmp.path(),
        Version::Four,
        &set("new", 2),
        Some(20_000),
        Naming::Part,
    );
    let new = (1..)
        .take_while(|i| {
            fs::read(tmp.path().join(format!("set.part{i}.rar")))
                .is_ok_and(|b| b.windows(3).any(|w| w == b"new"))
        })
        .count();
    assert!(old > new, "the old set must be longer");
    let stray = tmp.path().join(format!("set.part{}.rar", new + 1));
    let before = fs::read(&stray).unwrap();

    let r = extract(&archive, &tmp.path().join("out"), &[]);
    assert!(
        !r.status.success(),
        "a set with a stray volume was extracted"
    );
    assert!(
        stderr(&r).contains("belongs to another one"),
        "{}",
        stderr(&r)
    );
    assert_eq!(fs::read(&stray).unwrap(), before);
}

/// `.z01` is a split ZIP, not a RAR volume, unless a `.rar` of the same
/// name is next to it.
#[test]
fn a_z01_file_is_not_taken_for_a_rar_volume() {
    let tmp = TempDir::new().unwrap();
    let z01 = tmp.path().join("game.z01");
    fs::write(&z01, b"PK\x07\x08 not a rar").unwrap();
    let out = hatchzip(&["list", z01.to_str().unwrap()], &[]);
    assert!(!out.status.success());
    assert!(!stderr(&out).contains("game.rar"), "{}", stderr(&out));
}

// -- Restore -----------------------------------------------------------------------

/// A half-extracted set is turned back into one any tool opens, holding the
/// files not yet extracted; extracting it again gives the rest.
#[test]
fn restore_gives_back_a_set_holding_the_rest() {
    for (set, files) in [
        ("rar5", sample()),
        ("rar4", sample()),
        ("rar5-extras", small()),
    ] {
        let tmp = TempDir::new().unwrap();
        let archive = copy_set(set, tmp.path());
        let out = tmp.path().join("out");
        let crashed = extract(
            &archive,
            &out,
            &[
                ("HATCHZIP_BATCH_ENTRIES", "1"),
                ("HATCHZIP_CRASH_AT", "after-extract"),
                ("HATCHZIP_CRASH_AFTER", "3"),
            ],
        );
        assert!(!crashed.status.success());
        assert_ok(&hatchzip(&["restore", archive.to_str().unwrap()], &[]));
        assert_no_side_files(&archive);
        assert_ok(&hatchzip(&["test", archive.to_str().unwrap()], &[]));

        let more = tmp.path().join("more");
        assert_ok(&extract(&archive, &more, &[]));
        // The two folders together hold every file, once.
        for (name, data) in &files {
            let a = fs::read(out.join(name)).ok();
            let b = fs::read(more.join(name)).ok();
            assert!(
                a.is_some() != b.is_some(),
                "{set}: {name} in both or neither"
            );
            assert_eq!(a.or(b).as_ref(), Some(data), "{set}: {name}");
        }
    }
}

/// Killed after it appends the end header, restore is undone by the next
/// run, which either restores again or continues the extraction.
#[test]
fn an_interrupted_restore_can_be_run_again() {
    let tmp = TempDir::new().unwrap();
    let archive = copy_set("rar5", tmp.path());
    let out = tmp.path().join("out");
    let crashed = extract(
        &archive,
        &out,
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-extract"),
            ("HATCHZIP_CRASH_AFTER", "4"),
        ],
    );
    assert!(!crashed.status.success());
    let a = archive.to_str().unwrap();
    let r = hatchzip(
        &["restore", a],
        &[("HATCHZIP_CRASH_AT", "restore-appended")],
    );
    assert!(!r.status.success());
    // Continuing the extraction instead.
    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &sample());
    assert_consumed(&archive);
}
