//! 7z, tar and split archives, native split ZIP included, through the same
//! checks as ZIP: identical output, the archive gone, and survival of a
//! crash at every commit.

mod common;

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use common::*;
use sevenz_rust2::{ArchiveEntry, ArchiveWriter, SourceReader};
use tempfile::TempDir;

/// Solid archives pack many files into one block; this splits the sample
/// into three blocks so truncation has something to do between them.
fn build_7z(dir: &Path, files: &Files, solid: bool) -> PathBuf {
    let path = dir.join("game.7z");
    let mut w = ArchiveWriter::create(&path).unwrap();
    w.push_archive_entry::<&[u8]>(ArchiveEntry::new_directory(EMPTY_DIR), None)
        .unwrap();
    let all: Vec<_> = files.iter().collect();
    if solid {
        for chunk in all.chunks(all.len().div_ceil(3)) {
            let entries = chunk
                .iter()
                .map(|(n, _)| ArchiveEntry::new_file(n))
                .collect();
            let readers = chunk
                .iter()
                .map(|(_, d)| SourceReader::new(Cursor::new(d.to_vec())))
                .collect();
            w.push_archive_entries(entries, readers).unwrap();
        }
    } else {
        for (name, data) in all {
            w.push_archive_entry(ArchiveEntry::new_file(name), Some(data.as_slice()))
                .unwrap();
        }
    }
    w.finish().unwrap();
    path
}

fn build_tar(dir: &Path, files: &Files) -> PathBuf {
    let mut b = tar::Builder::new(Vec::new());
    let mut dir_header = tar::Header::new_gnu();
    dir_header.set_entry_type(tar::EntryType::Directory);
    dir_header.set_mode(0o755);
    dir_header.set_size(0);
    b.append_data(&mut dir_header, "./", std::io::empty())
        .unwrap();
    b.append_data(&mut dir_header.clone(), EMPTY_DIR, std::io::empty())
        .unwrap();
    for (name, data) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(1_700_000_000);
        b.append_data(&mut h, name, data.as_slice()).unwrap();
    }
    let path = dir.join("game.tar");
    fs::write(&path, b.into_inner().unwrap()).unwrap();
    path
}

/// A file name over 100 bytes, which tar can only store in an extra record.
fn long_name() -> String {
    format!("game/{}/deep.txt", "very-long-folder-name".repeat(8))
}

fn tar_files() -> Files {
    let mut files = sample_files();
    files.insert(
        long_name(),
        b"long names need a GNU or pax record\n".to_vec(),
    );
    files
}

#[test]
fn extracts_non_solid_7z() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_7z(tmp.path(), &files, false);
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

#[test]
fn extracts_solid_7z_block_by_block() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_7z(tmp.path(), &files, true);
    let out = tmp.path().join("out");

    let run = extract(&archive, &out, &[("HATCHZIP_BATCH_ENTRIES", "1")]);
    assert_ok(&run);
    assert!(String::from_utf8_lossy(&run.stdout).contains(" 7z,"));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

#[test]
fn resumes_7z_after_a_crash() {
    let files = sample_files();
    crash_and_resume(|d| build_7z(d, &files, false), &files, &[0, 3, 20]);
    crash_and_resume(|d| build_7z(d, &files, true), &files, &[0, 1, 2]);
}

#[test]
fn verify_first_catches_a_damaged_7z_block() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_7z(tmp.path(), &files, true);
    let mut bytes = fs::read(&archive).unwrap();
    let mid = bytes.len() / 3;
    bytes[mid] ^= 0xFF;
    fs::write(&archive, &bytes).unwrap();
    let (a, out) = (archive.to_str().unwrap(), tmp.path().join("out"));

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
    assert_eq!(fs::read(&archive).unwrap(), bytes);
}

#[test]
fn extracts_tar_with_long_names_and_a_dot_entry() {
    let tmp = TempDir::new().unwrap();
    let files = tar_files();
    let archive = build_tar(tmp.path(), &files);
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

#[test]
fn resumes_tar_after_a_crash() {
    let files = tar_files();
    crash_and_resume(|d| build_tar(d, &files), &files, &[0, 5, 30]);
}

#[cfg(unix)]
#[test]
fn tar_links_are_made_after_their_targets() {
    let tmp = TempDir::new().unwrap();
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(6);
    h.set_mode(0o644);
    b.append_data(&mut h, "bin/real", &b"hello\n"[..]).unwrap();
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Link);
    link.set_size(0);
    b.append_link(&mut link, "bin/hard", "bin/real").unwrap();
    let mut sym = tar::Header::new_gnu();
    sym.set_entry_type(tar::EntryType::Symlink);
    sym.set_size(0);
    b.append_link(&mut sym, "bin/soft", "real").unwrap();
    let archive = tmp.path().join("links.tar");
    fs::write(&archive, b.into_inner().unwrap()).unwrap();
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[("HATCHZIP_BATCH_ENTRIES", "1")]));
    assert_eq!(fs::read(out.join("bin/hard")).unwrap(), b"hello\n");
    assert_eq!(
        fs::read_link(out.join("bin/soft")).unwrap(),
        Path::new("real")
    );
    assert_eq!(fs::read(out.join("bin/soft")).unwrap(), b"hello\n");
}

/// GNU tar stores a time before 1970 as a negative base-256 number.
#[test]
fn tar_keeps_a_modified_time_before_1970() {
    let tmp = TempDir::new().unwrap();
    let mut h = tar::Header::new_gnu();
    h.set_size(4);
    h.set_mode(0o644);
    let mtime = &mut h.as_old_mut().mtime;
    mtime[..4].fill(0xff);
    mtime[4..].copy_from_slice(&(-152_391_233_i64).to_be_bytes());
    let mut b = tar::Builder::new(Vec::new());
    b.append_data(&mut h, "old.txt", &b"data"[..]).unwrap();
    let archive = tmp.path().join("old.tar");
    fs::write(&archive, b.into_inner().unwrap()).unwrap();
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[]));
    let modified = fs::metadata(out.join("old.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        modified,
        std::time::UNIX_EPOCH - std::time::Duration::from_secs(152_391_233)
    );
}

/// A pax archive keeps a time the header can't hold in an `mtime` record,
/// with 0 in the header.
#[test]
fn tar_reads_the_modified_time_from_a_pax_record() {
    let tmp = TempDir::new().unwrap();
    let mut b = tar::Builder::new(Vec::new());
    let record = b"20 mtime=-152391233\n";
    let mut pax = tar::Header::new_ustar();
    pax.set_entry_type(tar::EntryType::XHeader);
    pax.set_size(record.len() as u64);
    b.append_data(&mut pax, "PaxHeaders/old.txt", &record[..])
        .unwrap();
    let mut h = tar::Header::new_ustar();
    h.set_size(4);
    h.set_mode(0o644);
    h.set_mtime(0);
    b.append_data(&mut h, "old.txt", &b"data"[..]).unwrap();
    let archive = tmp.path().join("pax.tar");
    fs::write(&archive, b.into_inner().unwrap()).unwrap();
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[]));
    let modified = fs::metadata(out.join("old.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        modified,
        std::time::UNIX_EPOCH - std::time::Duration::from_secs(152_391_233)
    );
}

/// A hard link is copied where the filesystem can't link, so the space check
/// counts it at its file's size, even when the link comes first.
#[test]
fn tar_links_before_their_targets_count_at_the_target_size() {
    let tmp = TempDir::new().unwrap();
    let mut b = tar::Builder::new(Vec::new());
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Link);
    link.set_size(0);
    b.append_link(&mut link, "copy.bin", "big.bin").unwrap();
    let mut h = tar::Header::new_gnu();
    h.set_size(1_000_000);
    h.set_mode(0o644);
    b.append_data(&mut h, "big.bin", &noise(1_000_000, 1)[..])
        .unwrap();
    let bytes = b.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&bytes).unwrap();

    for (name, bytes) in [("links.tar", bytes), ("links.tar.gz", gz.finish().unwrap())] {
        let archive = tmp.path().join(name);
        fs::write(&archive, bytes).unwrap();
        let check = hatchzip(&["check", archive.to_str().unwrap()], &[]);
        assert_ok(&check);
        let stdout = String::from_utf8_lossy(&check.stdout);
        let needs = stdout
            .lines()
            .find(|l| l.contains("standard extraction needs"))
            .unwrap_or_else(|| panic!("{stdout}"));
        assert!(needs.ends_with(" 2.0 MB"), "{name}: {needs}");
    }
}

#[test]
fn refuses_tar_paths_that_escape() {
    let tmp = TempDir::new().unwrap();
    let mut raw = tar::Header::new_gnu();
    raw.set_size(4);
    raw.set_mode(0o644);
    // set_path refuses "..", so the name is written into the header by hand.
    raw.as_old_mut().name[..13].copy_from_slice(b"../escape.txt");
    raw.set_cksum();
    let mut bytes = raw.as_bytes().to_vec();
    bytes.extend(b"nope");
    bytes.resize(512 * 4, 0);
    let archive = tmp.path().join("evil.tar");
    fs::write(&archive, &bytes).unwrap();

    let run = extract(&archive, &tmp.path().join("out"), &[]);
    assert!(!run.status.success());
    assert!(!tmp.path().join("escape.txt").exists());
    assert_eq!(fs::read(&archive).unwrap(), bytes);
}

fn split_into(dir: &Path, archive: PathBuf, parts: usize) -> PathBuf {
    let bytes = fs::read(&archive).unwrap();
    fs::remove_file(&archive).unwrap();
    split(&bytes, &dir.join(archive.file_name().unwrap()), parts).remove(0)
}

#[test]
fn split_archives_lose_a_volume_at_a_time() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let built = build_7z(tmp.path(), &files, false);
    let first = split_into(tmp.path(), built, 4);
    let last = first.with_extension("004");
    assert!(last.exists());

    // Late enough that big.pak, which spans most of the set, is committed.
    let env = [
        ("HATCHZIP_BATCH_ENTRIES", "1"),
        ("HATCHZIP_CRASH_AT", "after-extract"),
        ("HATCHZIP_CRASH_AFTER", "45"),
    ];
    assert!(
        !extract(&first, &tmp.path().join("out"), &env)
            .status
            .success()
    );
    // Exactly the volumes wholly past the committed length are gone.
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(&side_files(&first)[0]).unwrap()).unwrap();
    let committed = journal["committed_len"].as_u64().unwrap();
    let mut start = 0;
    for (path, len) in journal["parts"].as_array().unwrap().iter().map(|p| {
        (
            PathBuf::from(p[0].as_str().unwrap()),
            p[1].as_u64().unwrap(),
        )
    }) {
        let expect = start < committed;
        assert_eq!(path.exists(), expect, "{} existence", path.display());
        start += len;
    }
    assert!(!last.exists(), "the last volume should be deleted by now");

    assert_ok(&extract(&first, &tmp.path().join("out"), &[]));
    assert_tree(&tmp.path().join("out"), &files);
    assert_consumed(&first);
    for n in 2..=4 {
        assert!(!first.with_extension(format!("{n:03}")).exists());
    }
}

#[test]
fn split_zip_resumes_across_volume_boundaries() {
    let files = sample_files();
    let build = |d: &Path| {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        zip.add_directory(format!("{EMPTY_DIR}/"), opts).unwrap();
        for (name, data) in &files {
            zip.start_file(name, opts).unwrap();
            zip.write_all(data).unwrap();
        }
        let path = d.join("game.zip");
        fs::write(&path, zip.finish().unwrap().into_inner()).unwrap();
        // Five volumes put boundaries inside the 3 MB entry and the index.
        split_into(d, path, 5)
    };
    crash_and_resume(build, &files, &[0, 2, 25]);
}

#[test]
fn any_volume_name_finds_the_whole_set() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let first = split_into(tmp.path(), build_7z(tmp.path(), &files, true), 3);
    let second = first.with_extension("002");
    let out = tmp.path().join("out");

    assert_ok(&extract(&second, &out, &[]));
    assert_tree(&out, &files);
    assert_consumed(&first);
}

/// Compressed tars have their own suites; damaged streams and unsupported
/// formats are refused without changing the archive.
#[test]
fn compressed_tar_streams_are_refused_without_touching_them() {
    let tmp = TempDir::new().unwrap();
    for (name, bytes, says) in [
        (
            "game.tar.zst",
            &b"\x28\xB5\x2F\xFDpretend zstd"[..],
            "zstd stream's structure",
        ),
        (
            "game.tar.xz",
            &b"\xFD7zXZ\x00pretend xz"[..],
            "xz stream's structure",
        ),
        ("game.tar.bz2", &b"BZh9pretend bzip2"[..], "bzip2"),
        ("zeros.tar.bz2", &[0; 32][..], "tar headers"),
        (
            "game.tar.gz",
            &b"\x1F\x8B\x08\x00pretend gzip"[..],
            "compressed",
        ),
    ] {
        let archive = tmp.path().join(name);
        fs::write(&archive, bytes).unwrap();
        let run = extract(&archive, &tmp.path().join("out"), &[]);
        assert!(!run.status.success(), "{name}");
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(stderr.contains(says), "{name}: {stderr}");
        if name == "game.tar.bz2" || name == "game.tar.zst" {
            assert_eq!(run.status.code(), Some(4), "{name}: reported as damaged");
        }
        assert_eq!(fs::read(&archive).unwrap(), bytes, "{name}");
        assert!(!side_files(&archive)[0].exists());
    }
}

/// Bzip2 is found by its signature, so a ZIP behind a bzip2 tar name still
/// extracts as a ZIP.
#[test]
fn a_zip_with_a_bzip2_tar_name_extracts_as_a_zip() {
    for name in ["game.tbz2", "game.tbz"] {
        let tmp = TempDir::new().unwrap();
        let files = sample_files();
        let archive = zip_of(tmp.path(), name, &files);
        let out = tmp.path().join("out");
        assert_ok(&extract(&archive, &out, &[]));
        assert_tree(&out, &files);
        assert_consumed(&archive);
    }
}

#[test]
fn split_archives_verified_by_sfv_extract_successfully() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let built = build_7z(tmp.path(), &files, false);
    let first = split_into(tmp.path(), built, 3);
    let v1 = &first;
    let v2 = first.with_extension("002");
    let v3 = first.with_extension("003");
    assert!(v1.exists() && v2.exists() && v3.exists());

    let crc1 = crc32fast::hash(&fs::read(v1).unwrap());
    let crc2 = crc32fast::hash(&fs::read(&v2).unwrap());
    let crc3 = crc32fast::hash(&fs::read(&v3).unwrap());

    let sfv_content = format!(
        "; Generated SFV\r\ngame.7z.001 {crc1:08X}\r\ngame.7z.002 {crc2:08X}\r\ngame.7z.003 {crc3:08X}\r\n"
    );
    let sfv_path = tmp.path().join("game.7z.sfv");
    fs::write(&sfv_path, sfv_content).unwrap();

    let out = tmp.path().join("out");
    let run = extract(&first, &out, &[]);
    assert_ok(&run);
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("volumes verified against game.7z.sfv"),
        "{stdout}"
    );
    assert_tree(&out, &files);
    assert_consumed(&first);
}

#[test]
fn damaged_volume_stopped_by_sfv_before_modifying_anything() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let built = build_7z(tmp.path(), &files, false);
    let first = split_into(tmp.path(), built, 3);
    let before = snapshot(&first);

    let v1 = &first;
    let v2 = first.with_extension("002");
    let v3 = first.with_extension("003");

    let crc1 = crc32fast::hash(&fs::read(v1).unwrap());
    let crc2 = crc32fast::hash(&fs::read(&v2).unwrap()) ^ 0xFFFF_FFFF;
    let crc3 = crc32fast::hash(&fs::read(&v3).unwrap());

    let sfv_content = format!(
        "; Generated SFV\r\ngame.7z.001 {crc1:08X}\r\ngame.7z.002 {crc2:08X}\r\ngame.7z.003 {crc3:08X}\r\n"
    );
    let sfv_path = tmp.path().join("game.7z.sfv");
    fs::write(&sfv_path, sfv_content).unwrap();

    let out = tmp.path().join("out");
    let run = extract(&first, &out, &[]);
    assert!(!run.status.success());
    assert_eq!(run.status.code(), Some(4));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("game.7z.002 does not match its CRC-32 in game.7z.sfv"),
        "{stderr}"
    );
    assert!(stderr.contains("Nothing was modified"), "{stderr}");

    assert_unchanged(&before);
    assert_no_side_files(&first);
}

#[test]
fn missing_volume_in_sfv_stopped_before_modifying_anything() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let built = build_7z(tmp.path(), &files, false);
    let first = split_into(tmp.path(), built, 2);
    let before = snapshot(&first);

    let v1 = &first;
    let v2 = first.with_extension("002");

    let crc1 = crc32fast::hash(&fs::read(v1).unwrap());
    let crc2 = crc32fast::hash(&fs::read(&v2).unwrap());

    let sfv_content = format!(
        "; Generated SFV\r\ngame.7z.001 {crc1:08X}\r\ngame.7z.002 {crc2:08X}\r\ngame.7z.003 12345678\r\n"
    );
    let sfv_path = tmp.path().join("game.7z.sfv");
    fs::write(&sfv_path, sfv_content).unwrap();

    let out = tmp.path().join("out");
    let run = extract(&first, &out, &[]);
    assert!(!run.status.success());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("game.7z.003 listed in game.7z.sfv is missing"),
        "{stderr}"
    );
    assert!(stderr.contains("Nothing was modified"), "{stderr}");

    assert_unchanged(&before);
    assert_no_side_files(&first);
}

/// `:` is a normal name character outside Windows, like in a file named
/// after a time. zip 9.5 reads a later `c:` component, as in Wine's
/// `dosdevices/c:`, as a drive on every system, so ZIP gets that name too.
#[test]
fn names_with_a_colon_extract_outside_windows() {
    let mut files = Files::new();
    files.insert("notes/meeting 10:30.txt".into(), b"agenda".to_vec());
    for (i, build) in [
        |d: &Path, f: &Files| sevenz_of(d, "notes.7z", f, false),
        |d: &Path, f: &Files| tar_of(d, "notes.tar", f),
    ]
    .iter()
    .enumerate()
    {
        let tmp = TempDir::new().unwrap();
        let archive = build(tmp.path(), &files);
        let out = tmp.path().join("out");
        let run = extract(&archive, &out, &[]);
        if cfg!(windows) {
            assert!(!run.status.success(), "{i}");
        } else {
            assert_ok(&run);
            assert_tree(&out, &files);
        }
    }

    files.insert("wine/dosdevices/c:/x.txt".into(), b"wine".to_vec());
    let tmp = TempDir::new().unwrap();
    let archive = zip_of(tmp.path(), "notes.zip", &files);
    let before = fs::read(&archive).unwrap();
    let out = tmp.path().join("out");
    let run = extract(&archive, &out, &[]);
    if cfg!(windows) {
        assert!(!run.status.success(), "zip");
        assert_eq!(fs::read(&archive).unwrap(), before, "zip");
    } else {
        assert_ok(&run);
        assert_tree(&out, &files);
    }
}

// Native split ZIP: `game.z01`, `game.z02`, ..., `game.zip`, as Info-ZIP
// `zip -s`, WinZip and WinRAR write it. Unlike a `.zip.001` byte split, its
// index is in the last segment and every offset in it is a segment number
// and an offset inside that segment. Sets come from `split_zip_segments`,
// which rewrites a ZIP from the zip crate the way `zip -s` lays one out,
// and from Info-ZIP itself where it is installed.

/// The stub journal next to the `.zip` that keeps older builds away.
fn stub_of(first: &Path) -> PathBuf {
    let name = first.file_name().unwrap().to_str().unwrap();
    let upper = name.ends_with(".Z01");
    let zip = split_zip_name(
        first.parent().unwrap(),
        &name[..name.len() - 4],
        1,
        2,
        upper,
    );
    PathBuf::from(format!("{}.hatchzip", zip.display()))
}

/// Every segment, the side files and the stub are gone.
fn assert_split_gone(first: &Path) {
    let left = split_zip_parts(first);
    assert!(left.is_empty(), "segments left: {left:?}");
    assert_consumed(first);
    assert!(
        !stub_of(first).exists(),
        "{} is left",
        stub_of(first).display()
    );
}

fn zip_bytes_of(files: &Files) -> Vec<u8> {
    let tmp = TempDir::new().unwrap();
    fs::read(zip_of(tmp.path(), "whole.zip", files)).unwrap()
}

/// The offset in a `split_zip_segments` set, marker included, where the
/// central directory of `zip` starts and ends, and where the local header
/// of `name` starts.
fn layout(bytes: &[u8], marker: usize, name: &str) -> (usize, usize, usize) {
    let eocd = bytes.len() - 22;
    let word = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) as usize;
    let (size, at) = (word(eocd + 12), word(eocd + 16));
    let mut z = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).unwrap();
    let header = z.by_name(name).unwrap().header_start() as usize;
    (marker + at, marker + at + size, marker + header)
}

#[test]
fn extracts_a_native_split_zip() {
    let files = sample_files();
    let bytes = zip_bytes_of(&files);
    let (cd_start, cd_end, big) = layout(&bytes, 4, "game/data/big.pak");
    let small = layout(&bytes, 4, "game/data/small.pak").2;
    let splits = [
        ("default", Split::default()),
        (
            "PK00",
            Split {
                marker: b"PK00",
                ..Split::default()
            },
        ),
        (
            "no marker",
            Split {
                marker: b"",
                ..Split::default()
            },
        ),
        (
            "ZIP64 fields",
            Split {
                zip64: true,
                ..Split::default()
            },
        ),
        (
            "upper case",
            Split {
                upper: true,
                ..Split::default()
            },
        ),
        (
            "one entry per segment",
            Split {
                at_headers: true,
                ..Split::default()
            },
        ),
        // A boundary inside the central directory, and one inside a local
        // header: the first segment ends ten bytes into big.pak's.
        (
            "index across segments",
            Split {
                size: (cd_start + cd_end) / 2 / 10,
                ..Split::default()
            },
        ),
        (
            "header across segments",
            Split {
                size: big + 10,
                ..Split::default()
            },
        ),
        // A segment that ends exactly where small.pak's header starts.
        (
            "header at a boundary",
            Split {
                size: small,
                ..Split::default()
            },
        ),
    ];
    for (what, split) in splits {
        let tmp = TempDir::new().unwrap();
        let first = write_split_zip(
            tmp.path(),
            "game",
            &split_zip_segments(&bytes, &split),
            split.upper,
        );
        assert!(split_zip_parts(&first).len() >= 2, "{what}");
        let out = tmp.path().join("out");
        let run = extract(&first, &out, &[("HATCHZIP_BATCH_ENTRIES", "3")]);
        assert_ok(&run);
        assert_tree(&out, &files);
        assert_split_gone(&first);
    }
}

#[test]
fn every_segment_name_finds_a_native_split_zip() {
    let files = sample_files();
    for pick in ["z01", "z03", "zip"] {
        let tmp = TempDir::new().unwrap();
        let first = split_zip_of(tmp.path(), &files, &Split::default());
        let given = first.with_extension(pick);
        assert!(given.exists(), "{pick}");
        let out = tmp.path().join("out");
        assert_ok(&extract(&given, &out, &[]));
        assert_tree(&out, &files);
        assert_split_gone(&first);
    }

    // Without -o, `game.z01` extracts into `game`, as `game.zip` would.
    let tmp = TempDir::new().unwrap();
    let first = split_zip_of(tmp.path(), &files, &Split::default());
    let a = first.to_str().unwrap();
    assert_ok(&hatchzip(&["extract", a, "--keep-free", "0", "--yes"], &[]));
    assert_tree(&tmp.path().join("game"), &files);
    assert_split_gone(&first);
}

/// What a shell's `game.z*` gives, half way through an extraction: every
/// segment and the progress files, which all count as the one archive.
#[test]
fn a_glob_over_a_half_extracted_split_zip_counts_it_once() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let first = split_zip_of(tmp.path(), &files, &Split::default());
    let out = tmp.path().join("out");
    let crashed = extract(
        &first,
        &out,
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "0"),
        ],
    );
    assert!(!crashed.status.success());
    let [journal, tail] = side_files(&first);
    let mut names: Vec<String> = split_zip_parts(&first)
        .iter()
        .chain([&journal, &tail])
        .map(|p| p.to_str().unwrap().to_string())
        .collect();
    names.sort();
    let mut args = vec!["list"];
    args.extend(names.iter().map(String::as_str));
    let run = hatchzip(&args, &[]);
    assert_ok(&run);
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert_eq!(stdout.matches("game/data/big.pak").count(), 1, "{stdout}");
}

/// Kills a run at both commit points after each of `afters` commits. After
/// each, what is left must be an untouched prefix of the set: every segment
/// that starts below the journal's `committed_len` there, cut at most to it,
/// and, once the cut is done, every segment past it gone. Then resumes.
fn split_zip_crash_and_resume(build: impl Fn(&Path) -> PathBuf, files: &Files, afters: &[u32]) {
    for point in ["after-extract", "after-journal"] {
        for after in afters {
            let tmp = TempDir::new().unwrap();
            let first = build(tmp.path());
            let before = split_zip_snapshot(&first);
            let out = tmp.path().join("out");
            let what = format!("{point}#{after}");
            let after_s = after.to_string();
            let crashed = extract(
                &first,
                &out,
                &[
                    ("HATCHZIP_BATCH_ENTRIES", "1"),
                    ("HATCHZIP_CRASH_AT", point),
                    ("HATCHZIP_CRASH_AFTER", &after_s),
                ],
            );
            assert!(!crashed.status.success(), "{what} should have crashed");
            let [journal, tail] = side_files(&first);
            assert!(journal.exists() && tail.exists(), "{what}");
            assert!(stub_of(&first).exists(), "{what}: no stub");
            let j: serde_json::Value =
                serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
            assert_eq!(j["format"], "split-zip", "{what}");
            let committed = j["committed_len"].as_u64().unwrap();
            let mut start = 0u64;
            for (path, bytes) in &before {
                let len = bytes.len() as u64;
                let now = fs::read(path).ok();
                if start < committed {
                    let now = now.unwrap_or_else(|| panic!("{what}: {} is gone", path.display()));
                    assert!(
                        bytes.starts_with(&now),
                        "{what}: {} changed",
                        path.display()
                    );
                    if point == "after-extract" {
                        assert_eq!(now.len() as u64, len.min(committed - start), "{what}");
                    }
                } else if point == "after-extract" {
                    assert!(now.is_none(), "{what}: {} should be gone", path.display());
                } else if let Some(now) = now {
                    assert!(
                        bytes.starts_with(&now),
                        "{what}: {} changed",
                        path.display()
                    );
                }
                start += len;
            }

            assert_ok(&extract(&first, &out, &[]));
            assert_tree(&out, files);
            assert_split_gone(&first);
        }
    }
}

#[test]
fn resumes_a_native_split_zip_after_a_crash() {
    let files = sample_files();
    let units = zip::ZipArchive::new(Cursor::new(zip_bytes_of(&files)))
        .unwrap()
        .len() as u32;
    let afters = [0, 3, 30, units - 2];
    for split in [
        Split::default(),
        Split {
            size: 65_536,
            marker: b"",
            ..Split::default()
        },
        Split {
            zip64: true,
            ..Split::default()
        },
        Split {
            at_headers: true,
            ..Split::default()
        },
    ] {
        split_zip_crash_and_resume(|d| split_zip_of(d, &files, &split), &files, &afters);
    }
}

/// A large entry is extracted while the bytes behind it are freed, across
/// segment boundaries; killed at each step of a resume point, it resumes
/// from that point.
#[test]
fn streams_an_entry_across_segments_and_resumes_at_every_step() {
    let files = sample_files();
    let step = ("HATCHZIP_STREAM_STEP", "262144");
    for point in [
        "stream-after-sync",
        "stream-after-journal",
        "stream-after-punch",
    ] {
        for after in ["0", "3"] {
            let tmp = TempDir::new().unwrap();
            let first = split_zip_of(
                tmp.path(),
                &files,
                &Split {
                    size: 300_000,
                    ..Split::default()
                },
            );
            let out = tmp.path().join("out");
            let crashed = extract(
                &first,
                &out,
                &[
                    step,
                    ("HATCHZIP_BATCH_ENTRIES", "1"),
                    ("HATCHZIP_CRASH_AT", point),
                    ("HATCHZIP_CRASH_AFTER", after),
                ],
            );
            assert!(
                !crashed.status.success(),
                "{point}#{after} should have crashed"
            );
            for p in side_files(&first) {
                assert!(p.exists(), "{point}#{after}: {} is missing", p.display());
            }
            assert_ok(&extract(&first, &out, &[step]));
            assert_tree(&out, &files);
            assert_split_gone(&first);
        }
    }
}

/// A `.z01` moved away mid-run is not an archive that is done: the run
/// stops with the journal and the saved index kept, and resumes once it is
/// back.
#[test]
fn a_first_segment_moved_mid_run_stops_the_run() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let first = split_zip_of(tmp.path(), &files, &Split::default());
    let out = tmp.path().join("out");
    let crashed = extract(
        &first,
        &out,
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "3"),
        ],
    );
    assert!(!crashed.status.success());
    let moved = tmp.path().join("moved.bin");
    fs::rename(&first, &moved).unwrap();

    let run = extract(&first, &out, &[]);
    assert!(!run.status.success());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("was moved or deleted"), "{stderr}");
    for p in side_files(&first) {
        assert!(p.exists(), "{} was removed", p.display());
    }

    fs::rename(&moved, &first).unwrap();
    assert_ok(&extract(&first, &out, &[]));
    assert_tree(&out, &files);
    assert_split_gone(&first);
}

/// Refused, with every segment byte for byte as it was and no side file
/// written.
fn assert_split_refused(given: &Path, first: &Path, says: &str) {
    let before: Vec<_> = split_zip_snapshot(first);
    let out = given.parent().unwrap().join("out");
    let run = extract(given, &out, &[]);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(!run.status.success(), "{says}: extracted");
    assert!(stderr.contains(says), "{says}: {stderr}");
    assert_unchanged(&before);
    assert_no_side_files(first);
    assert!(!stub_of(first).exists());
}

#[test]
fn a_native_split_zip_with_a_part_missing_or_extra_is_refused() {
    let files = sample_files();
    let fresh = |tmp: &TempDir| split_zip_of(tmp.path(), &files, &Split::default());

    let tmp = TempDir::new().unwrap();
    let first = fresh(&tmp);
    fs::remove_file(first.with_extension("z02")).unwrap();
    assert_split_refused(&first, &first, "game.z02 is missing");

    let tmp = TempDir::new().unwrap();
    let first = fresh(&tmp);
    fs::remove_file(first.with_extension("zip")).unwrap();
    assert_split_refused(&first, &first, "game.zip is missing");

    // A stray segment after the last.
    let tmp = TempDir::new().unwrap();
    let first = fresh(&tmp);
    let n = split_zip_parts(&first).len();
    let stray = first.with_extension(format!("z{n:02}"));
    fs::copy(&first, &stray).unwrap();
    assert_split_refused(&first, &first, "does not belong to this split ZIP");

    // A `.z01` next to a ZIP that is one file.
    let tmp = TempDir::new().unwrap();
    let whole = zip_of(tmp.path(), "game.zip", &files);
    let other = TempDir::new().unwrap();
    let segment = fs::read(fresh(&other)).unwrap();
    let first = tmp.path().join("game.z01");
    fs::write(&first, segment).unwrap();
    assert_split_refused(&whole, &first, "does not belong to this split ZIP");

    // The last segment on its own.
    let tmp = TempDir::new().unwrap();
    let first = fresh(&tmp);
    let last = first.with_extension("zip");
    let zip = fs::read(&last).unwrap();
    for p in split_zip_parts(&first) {
        fs::remove_file(p).unwrap();
    }
    fs::write(&last, &zip).unwrap();
    let run = extract(&last, &tmp.path().join("out"), &[]);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(!run.status.success());
    assert!(stderr.contains("the last part of a split ZIP"), "{stderr}");
    assert_eq!(fs::read(&last).unwrap(), zip);
    assert_no_side_files(&last);

    // An empty segment.
    let tmp = TempDir::new().unwrap();
    let first = fresh(&tmp);
    fs::write(first.with_extension("z02"), b"").unwrap();
    assert_split_refused(&first, &first, "game.z02 is empty");
}

/// APPNOTE lets a ZIP64 writer saturate the disk numbers to 0xFFFF and keep
/// the real ones in the ZIP64 end record and extra fields. A ZIP that is one
/// file written so extracts, as it did before split ZIP support, and the
/// last segment of a split ZIP written so is still refused on its own.
#[test]
fn a_one_file_zip64_with_saturated_disk_numbers_extracts() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let whole = zip_of(tmp.path(), "whole.zip", &files);
    let bytes = fs::read(&whole).unwrap();
    fs::remove_file(&whole).unwrap();
    let one = Split {
        size: 1 << 40,
        marker: b"",
        zip64: true,
        ..Split::default()
    };
    let segments = split_zip_segments(&bytes, &one);
    assert_eq!(segments.len(), 1);
    let archive = tmp.path().join("game.zip");
    fs::write(&archive, &segments[0]).unwrap();
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &files);
    assert_consumed(&archive);

    let tmp = TempDir::new().unwrap();
    let first = split_zip_of(
        tmp.path(),
        &files,
        &Split {
            zip64: true,
            ..Split::default()
        },
    );
    let last = first.with_extension("zip");
    let zip = fs::read(&last).unwrap();
    for p in split_zip_parts(&first) {
        fs::remove_file(p).unwrap();
    }
    fs::write(&last, &zip).unwrap();
    let run = extract(&last, &tmp.path().join("out"), &[]);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(!run.status.success());
    assert!(stderr.contains("the last part of a split ZIP"), "{stderr}");
    assert_eq!(fs::read(&last).unwrap(), zip);
    assert_no_side_files(&last);
}

#[test]
fn a_native_split_zip_with_a_wrong_index_or_a_foreign_part_is_refused() {
    let files = sample_files();

    // The end record counts one segment more than there are.
    let tmp = TempDir::new().unwrap();
    let first = split_zip_of(tmp.path(), &files, &Split::default());
    let last = first.with_extension("zip");
    let mut zip = fs::read(&last).unwrap();
    let at = zip.len() - 22 + 4;
    let disk = u16::from_le_bytes([zip[at], zip[at + 1]]) + 1;
    zip[at..at + 2].copy_from_slice(&disk.to_le_bytes());
    fs::write(&last, &zip).unwrap();
    let missing = format!("game.z{:02} is missing", disk);
    assert_split_refused(&first, &first, &missing);

    // A record whose offset lies past the end of its segment. With one
    // entry per segment, the `.zip` holds only the index, so its first
    // record is at its start.
    let tmp = TempDir::new().unwrap();
    let first = split_zip_of(
        tmp.path(),
        &files,
        &Split {
            at_headers: true,
            ..Split::default()
        },
    );
    let last = first.with_extension("zip");
    let mut zip = fs::read(&last).unwrap();
    assert_eq!(&zip[..4], b"PK\x01\x02");
    zip[42..46].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    fs::write(&last, &zip).unwrap();
    assert_split_refused(&first, &first, "past the end of its part");

    // A first segment from another copy, whose settings.ini is different
    // but the same length: everything lines up, and its local header
    // disagrees with the index.
    let tmp = TempDir::new().unwrap();
    let first = split_zip_of(tmp.path(), &files, &Split::default());
    let mut changed = files.clone();
    changed.insert(
        "game/config/settings.ini".into(),
        b"volume=8\n".repeat(20_000),
    );
    let other = TempDir::new().unwrap();
    let foreign = fs::read(split_zip_of(other.path(), &changed, &Split::default())).unwrap();
    assert_ne!(fs::read(&first).unwrap(), foreign);
    fs::write(&first, foreign).unwrap();
    assert_split_refused(&first, &first, "from another copy of the archive");
}

#[cfg(unix)]
#[test]
fn a_linked_segment_is_refused() {
    let files = sample_files();
    let tmp = TempDir::new().unwrap();
    let first = split_zip_of(tmp.path(), &files, &Split::default());
    let second = first.with_extension("z02");
    let elsewhere = tmp.path().join("elsewhere.bin");
    fs::rename(&second, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &second).unwrap();
    let kept = fs::read(&elsewhere).unwrap();
    let run = extract(&first, &tmp.path().join("out"), &[]);
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("is a link"));
    assert_eq!(fs::read(&elsewhere).unwrap(), kept);

    // A hard link would be emptied under its other name.
    fs::remove_file(&second).unwrap();
    fs::hard_link(&elsewhere, &second).unwrap();
    let before = split_zip_snapshot(&first);
    let run = extract(&first, &tmp.path().join("out"), &[]);
    assert!(!run.status.success());
    assert_unchanged(&before);
    assert_eq!(fs::read(&elsewhere).unwrap(), kept);
}

/// Info-ZIP, where it is installed (CI's Linux and macOS runners): `files`
/// under `<dir>/src`, packed with `zip -r -s 64k` and `extra` into
/// `<dir>/game.z01`, ..., `<dir>/game.zip`. `None` without `zip`.
fn info_zip_split(dir: &Path, files: &Files, extra: &[&str]) -> Option<PathBuf> {
    let src = dir.join("src");
    for (name, data) in files {
        let p = src.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }
    fs::create_dir_all(src.join(EMPTY_DIR)).unwrap();
    let run = std::process::Command::new("zip")
        .current_dir(&src)
        .args(["-q", "-r", "-s", "64k"])
        .args(extra)
        .arg(dir.join("game.zip"))
        .arg("game")
        .output()
        .ok()?;
    assert!(
        run.status.success(),
        "zip -s failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    fs::remove_dir_all(&src).unwrap();
    Some(dir.join("game.z01"))
}

/// The sample without its non-ASCII name, which Info-ZIP may store in the
/// system's own encoding instead of UTF-8.
fn ascii_files() -> Files {
    let mut files = sample_files();
    files.retain(|name, _| name.is_ascii());
    files
}

#[test]
fn extracts_split_zips_made_by_info_zip() {
    let mut files = ascii_files();
    // Over 100 segments of 64 KiB, so names go past `.z99`.
    files.insert("game/data/huge.pak".into(), noise(7_000_000, 3));
    for (what, extra, password) in [
        ("plain", &[][..], None),
        ("ZipCrypto", &["-P", "secret"][..], Some("secret")),
        ("ZIP64", &["-fz"][..], None),
    ] {
        let tmp = TempDir::new().unwrap();
        let Some(first) = info_zip_split(tmp.path(), &files, extra) else {
            eprintln!("zip not installed; skipping");
            return;
        };
        assert!(first.exists(), "{what}: no {}", first.display());
        if what == "plain" {
            assert!(first.with_extension("z100").exists(), "{what}: no .z100");
        }
        let out = tmp.path().join("out");
        let env: Vec<(&str, &str)> = password
            .map(|p| ("HATCHZIP_PASSWORD", p))
            .into_iter()
            .collect();
        assert_ok(&extract(&first, &out, &env));
        assert_tree(&out, &files);
        assert_split_gone(&first);
    }
}

#[test]
fn resumes_a_split_zip_made_by_info_zip_after_a_crash() {
    let files = ascii_files();
    let probe = TempDir::new().unwrap();
    if info_zip_split(probe.path(), &files, &[]).is_none() {
        eprintln!("zip not installed; skipping");
        return;
    }
    split_zip_crash_and_resume(
        |d| info_zip_split(d, &files, &[]).unwrap(),
        &files,
        &[0, 4, 20],
    );
}
