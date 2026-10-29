//! 7z, tar and split archives, through the same checks as ZIP: identical
//! output, the archive gone, and survival of a crash at every commit.

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
            "compressed stream",
        ),
        (
            "game.tar.xz",
            &b"\xFD7zXZ\x00pretend xz"[..],
            "xz stream's structure",
        ),
        ("game.tar.bz2", &b"BZh9pretend bzip2"[..], "bzip2"),
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
        if name.ends_with(".bz2") {
            assert_eq!(run.status.code(), Some(4), "reported as damaged");
        }
        assert_eq!(fs::read(&archive).unwrap(), bytes, "{name}");
        assert!(!side_files(&archive)[0].exists());
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
