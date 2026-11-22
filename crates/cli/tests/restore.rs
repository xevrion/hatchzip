//! `restore`: a half-extracted archive becomes a normal one again. Every
//! restored archive is read back by an independent reader (the zip, tar and
//! sevenz-rust2 crates) and, when installed, by the real `unzip`, `7z` and
//! `tar`, and must hold exactly the entries the output folder doesn't. A
//! restored native split ZIP is extracted again, and joined by Info-ZIP's
//! `zip -s 0` for `unzip -t` where installed.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::*;
use sevenz_rust2::{ArchiveEntry, ArchiveReader, ArchiveWriter, Password, SourceReader};
use tempfile::TempDir;
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

/// Stored and Deflated only, the methods every `unzip` build has, so its
/// verdict is about the index and not about a missing codec.
fn zip_bytes(files: &Files, zip64: bool) -> Vec<u8> {
    let methods = [CompressionMethod::Stored, CompressionMethod::Deflated];
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.add_directory(format!("{EMPTY_DIR}/"), SimpleFileOptions::default())
        .unwrap();
    for (i, (name, data)) in files.iter().enumerate() {
        let opts = SimpleFileOptions::default()
            .compression_method(methods[i % methods.len()])
            .large_file(zip64);
        zip.start_file(name, opts).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn build_zip(dir: &Path, files: &Files, zip64: bool) -> PathBuf {
    let path = dir.join("game.zip");
    fs::write(&path, zip_bytes(files, zip64)).unwrap();
    path
}

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

const HARD_LINK: &str = "game/hard.pak";
const HARD_TARGET: &str = "game/data/big.pak";
const SYMLINK: &str = "game/soft.ini";

/// The sample files, then a hard link to one of the first of them and a
/// symlink, so a crash near the end leaves the link's target unextracted.
fn build_tar(dir: &Path, files: &Files) -> PathBuf {
    let mut b = tar::Builder::new(Vec::new());
    let mut dir_header = tar::Header::new_gnu();
    dir_header.set_entry_type(tar::EntryType::Directory);
    dir_header.set_mode(0o755);
    dir_header.set_size(0);
    b.append_data(&mut dir_header, EMPTY_DIR, std::io::empty())
        .unwrap();
    for (name, data) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(1_700_000_000);
        b.append_data(&mut h, name, data.as_slice()).unwrap();
    }
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Link);
    link.set_size(0);
    b.append_link(&mut link, HARD_LINK, HARD_TARGET).unwrap();
    let mut sym = tar::Header::new_gnu();
    sym.set_entry_type(tar::EntryType::Symlink);
    sym.set_size(0);
    b.append_link(&mut sym, SYMLINK, "config/settings.ini")
        .unwrap();
    let path = dir.join("game.tar");
    fs::write(&path, b.into_inner().unwrap()).unwrap();
    path
}

fn split_into(archive: &Path, parts: usize) -> PathBuf {
    let bytes = fs::read(archive).unwrap();
    fs::remove_file(archive).unwrap();
    split(&bytes, archive, parts).remove(0)
}

/// Kills an extraction at `point` once `after` one-unit batches got past it.
fn crash_extract(archive: &Path, out: &Path, point: &str, after: u32) {
    let after = after.to_string();
    let run = extract(
        archive,
        out,
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", point),
            ("HATCHZIP_CRASH_AFTER", &after),
        ],
    );
    assert!(!run.status.success(), "{point}#{after} should have crashed");
}

/// Every test here extracts into `out` next to the archive, a folder of its
/// own choosing, so restore names it again, as a person who chose one must.
fn restore(archive: &Path, env: &[(&str, &str)]) -> Output {
    let out = archive.parent().unwrap().join("out");
    hatchzip(
        &[
            "restore",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ],
        env,
    )
}

/// The archive's bytes, joining the volumes of a split set.
fn joined(first: &Path) -> Vec<u8> {
    let Some(base) = first.to_str().unwrap().strip_suffix(".001") else {
        return fs::read(first).unwrap();
    };
    let mut bytes = Vec::new();
    for n in 1.. {
        let Ok(part) = fs::read(format!("{base}.{n:03}")) else {
            break;
        };
        bytes.extend(part);
    }
    bytes
}

/// What an archive holds: the contents of its files, and the names of all
/// its entries, folders and links included.
#[derive(Default, Debug)]
struct Listing {
    files: Files,
    names: BTreeSet<String>,
    hard_links: BTreeMap<String, String>,
}

fn read_zip(bytes: Vec<u8>) -> Listing {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).expect("the zip crate opens it");
    let mut l = Listing::default();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let name = f.name().unwrap().trim_end_matches('/').to_string();
        if !f.is_dir() {
            let mut data = Vec::new();
            f.read_to_end(&mut data).expect("CRC matches");
            l.files.insert(name.clone(), data);
        }
        l.names.insert(name);
    }
    l
}

fn read_tar(bytes: Vec<u8>) -> Listing {
    let mut l = Listing::default();
    let mut tar = tar::Archive::new(Cursor::new(bytes));
    for e in tar.entries().unwrap() {
        let mut e = e.unwrap();
        let name = e.path().unwrap().to_string_lossy().into_owned();
        match e.header().entry_type() {
            tar::EntryType::Regular => {
                let mut data = Vec::new();
                e.read_to_end(&mut data).unwrap();
                l.files.insert(name.clone(), data);
            }
            tar::EntryType::Link => {
                let target = e.link_name().unwrap().unwrap();
                l.hard_links
                    .insert(name.clone(), target.to_string_lossy().into_owned());
            }
            _ => {}
        }
        l.names.insert(name);
    }
    l
}

fn read_7z(bytes: Vec<u8>) -> Listing {
    let mut l = Listing::default();
    let mut r =
        ArchiveReader::new(Cursor::new(bytes), Password::empty()).expect("sevenz-rust2 opens it");
    r.for_each_entries(|entry, data| {
        let mut buf = Vec::new();
        data.read_to_end(&mut buf)?;
        if !entry.is_directory() {
            l.files.insert(entry.name().to_string(), buf);
        }
        l.names.insert(entry.name().to_string());
        Ok(true)
    })
    .expect("every CRC matches");
    l
}

/// Every file of the original is in exactly one place, the output folder or
/// the restored archive, with its original contents, and both got some.
fn assert_split_between(out: &Path, archive: &Listing, files: &Files) {
    let mut extracted = files_in(out);
    // A symlink is a text file holding its target on Windows.
    extracted.remove(SYMLINK);
    for (name, data) in &extracted {
        assert!(
            files.get(name) == Some(data),
            "extracted {name} is not the original"
        );
        assert!(
            !archive.files.contains_key(name),
            "{name} is both extracted and in the archive"
        );
    }
    for (name, data) in &archive.files {
        assert!(
            files.get(name) == Some(data),
            "{name} in the restored archive is not the original"
        );
    }
    let all: BTreeSet<_> = extracted.keys().chain(archive.files.keys()).collect();
    let want: BTreeSet<_> = files.keys().collect();
    assert_eq!(all, want, "files missing from both");
    assert!(
        !extracted.is_empty() && !archive.files.is_empty(),
        "the crash should leave work on both sides"
    );
    assert!(
        out.join(EMPTY_DIR).is_dir() || archive.names.contains(EMPTY_DIR),
        "{EMPTY_DIR} is lost"
    );
}

/// Regular files under `dir` by relative name; symlinks and part files are
/// left out, part files must not exist at all.
fn files_in(dir: &Path) -> Files {
    let mut found = Files::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries {
            let p = e.unwrap().path();
            let meta = fs::symlink_metadata(&p).unwrap();
            let rel = p
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            assert!(!rel.ends_with(".hatchzip-part"), "{rel} left behind");
            if meta.is_dir() {
                stack.push(p);
            } else if meta.is_file() {
                found.insert(rel, fs::read(&p).unwrap());
            }
        }
    }
    found
}

fn assert_side_files_gone(archive: &Path) {
    for p in side_files(archive) {
        assert!(!p.exists(), "{} should be removed", p.display());
    }
}

/// Runs a real archiver if it is installed. `None` when it isn't.
fn real(tool: &str, args: &[&str]) -> Option<Output> {
    Command::new(tool).args(args).output().ok()
}

fn names_from_lines(text: &str, prefix: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|l| l.strip_prefix(prefix))
        .map(|n| n.trim_end_matches('/').to_string())
        .filter(|n| !n.is_empty())
        .collect()
}

/// `unzip -t` gives the restored archive the same verdict as the original
/// (a self-extractor stub glued on without adjusting offsets earns a
/// warning both times), and `unzip -Z1` lists exactly its entries.
fn check_with_unzip(restored: &Path, original: &Path, listing: &Listing) {
    let Some(before) = real("unzip", &["-t", original.to_str().unwrap()]) else {
        eprintln!("unzip not installed; skipping");
        return;
    };
    let after = real("unzip", &["-t", restored.to_str().unwrap()]).unwrap();
    assert_eq!(
        after.status.code(),
        before.status.code(),
        "unzip -t:\n{}",
        String::from_utf8_lossy(&after.stdout)
    );
    assert!(
        matches!(before.status.code(), Some(0 | 1)),
        "unzip -t rejects the original:\n{}",
        String::from_utf8_lossy(&before.stdout)
    );
    let list = real("unzip", &["-Z1", restored.to_str().unwrap()]).unwrap();
    let names = names_from_lines(&String::from_utf8_lossy(&list.stdout), "");
    assert_eq!(names, listing.names, "unzip -Z1");
}

/// `7z t` passes, and `7z l -slt` lists exactly the archive's entries.
fn check_with_7z(restored: &Path, listing: &Listing) {
    let a = restored.to_str().unwrap();
    let Some(test) = real("7z", &["t", a]) else {
        eprintln!("7z not installed; skipping");
        return;
    };
    let text = String::from_utf8_lossy(&test.stdout);
    assert!(test.status.success(), "7z t failed:\n{text}");
    assert!(text.contains("Everything is Ok"), "7z t:\n{text}");
    let list = real("7z", &["l", "-slt", "-ba", a]).unwrap();
    assert!(list.status.success());
    // 7-Zip on Windows lists paths with backslashes.
    let stdout = String::from_utf8_lossy(&list.stdout).replace('\\', "/");
    let names = names_from_lines(&stdout, "Path = ");
    assert_eq!(names, listing.names, "7z l");
}

fn check_with_tar(restored: &Path, listing: &Listing) {
    let Some(list) = real("tar", &["-tf", restored.to_str().unwrap()]) else {
        eprintln!("tar not installed; skipping");
        return;
    };
    assert!(
        list.status.success(),
        "tar -tf:\n{}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(
        list.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let names = names_from_lines(&String::from_utf8_lossy(&list.stdout), "");
    assert_eq!(names, listing.names, "tar -tf");
}

fn keep_copy(archive: &Path) -> PathBuf {
    let copy = archive.with_file_name(format!(
        "original-{}",
        archive.file_name().unwrap().to_string_lossy()
    ));
    fs::copy(archive, &copy).unwrap();
    copy
}

#[test]
fn restores_a_zip() {
    for zip64 in [false, true] {
        let tmp = TempDir::new().unwrap();
        let files = sample_files();
        let archive = build_zip(tmp.path(), &files, zip64);
        let original = keep_copy(&archive);
        let out = tmp.path().join("out");

        crash_extract(&archive, &out, "after-journal", 12);
        let run = restore(&archive, &[]);
        assert_ok(&run);
        assert!(String::from_utf8_lossy(&run.stdout).contains("Archive restored"));

        let listing = read_zip(fs::read(&archive).unwrap());
        assert_split_between(&out, &listing, &files);
        assert_side_files_gone(&archive);
        check_with_unzip(&archive, &original, &listing);
        check_with_7z(&archive, &listing);

        // And HatchZip itself takes it from there like any other archive.
        assert_ok(&extract(&archive, &out, &[]));
        assert_tree(&out, &files);
        assert_consumed(&archive);
    }
}

fn has_zip64_end(archive: &Path) -> bool {
    fs::read(archive)
        .unwrap()
        .windows(4)
        .any(|w| w == b"PK\x06\x07")
}

/// The zip crate's `large_file` writes ZIP64 extra fields but no ZIP64 end
/// records, so for those this takes Info-ZIP's `zip -fz`.
#[test]
fn restores_a_zip_with_zip64_end_records() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let tree = tmp.path().join("tree");
    for (name, data) in &files {
        let p = tree.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }
    fs::create_dir_all(tree.join(EMPTY_DIR)).unwrap();
    let archive = tmp.path().join("game.zip");
    let Ok(made) = Command::new("zip")
        .args(["-q", "-r", "-fz", archive.to_str().unwrap(), "game"])
        .current_dir(&tree)
        .output()
    else {
        eprintln!("zip not installed; skipping");
        return;
    };
    assert!(made.status.success());
    assert!(
        has_zip64_end(&archive),
        "zip -fz should write ZIP64 end records"
    );
    let original = keep_copy(&archive);
    let out = tmp.path().join("out");

    crash_extract(&archive, &out, "after-journal", 10);
    assert_ok(&restore(&archive, &[]));
    assert!(
        has_zip64_end(&archive),
        "the original used ZIP64, so the new index does"
    );
    let listing = read_zip(fs::read(&archive).unwrap());
    assert_split_between(&out, &listing, &files);
    check_with_unzip(&archive, &original, &listing);
    check_with_7z(&archive, &listing);
}

#[test]
fn restores_a_self_extracting_zip() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let stub = b"MZ pretend this is a self-extracting exe stub".repeat(30);
    let mut bytes = stub.clone();
    bytes.extend(zip_bytes(&files, false));
    let archive = tmp.path().join("setup.exe");
    fs::write(&archive, bytes).unwrap();
    let original = keep_copy(&archive);
    let out = tmp.path().join("out");

    crash_extract(&archive, &out, "after-extract", 20);
    assert_ok(&restore(&archive, &[]));

    let restored = fs::read(&archive).unwrap();
    assert!(restored.starts_with(&stub), "the stub must stay in front");
    let listing = read_zip(restored);
    assert_split_between(&out, &listing, &files);
    check_with_unzip(&archive, &original, &listing);
}

#[test]
fn restores_a_split_zip_from_any_volume_name() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let first = split_into(&build_zip(tmp.path(), &files, false), 5);
    let out = tmp.path().join("out");

    crash_extract(&first, &out, "after-journal", 30);
    assert_ok(&restore(&first.with_extension("002"), &[]));
    assert_side_files_gone(&first);

    let bytes = joined(&first);
    let whole = tmp.path().join("joined.zip");
    fs::write(&whole, &bytes).unwrap();
    let listing = read_zip(bytes);
    assert_split_between(&out, &listing, &files);
    check_with_7z(&first, &listing);
    check_with_7z(&whole, &listing);
    if let Some(t) = real("unzip", &["-t", whole.to_str().unwrap()]) {
        assert!(t.status.success(), "{}", String::from_utf8_lossy(&t.stdout));
    }
}

/// When the cut falls exactly on a volume boundary, the new index makes
/// that volume longer than it was. An interrupted restore must still be
/// undone from there, by `restore` and by `extract`.
#[test]
fn an_interrupted_restore_grown_past_a_volume_is_undone() {
    for then in ["restore", "extract"] {
        let tmp = TempDir::new().unwrap();
        let files = sample_files();
        let bytes = zip_bytes(&files, false);
        let mut starts: Vec<u64> = {
            let mut z = zip::ZipArchive::new(Cursor::new(bytes.clone())).unwrap();
            (0..z.len())
                .map(|i| z.by_index_raw(i).unwrap().header_start())
                .collect()
        };
        starts.sort_unstable_by(|a, b| b.cmp(a));
        // One-unit batches commit units from the highest offset down, so
        // the third commit leaves the archive ending at `starts[2]`.
        let boundary = starts[2] as usize;
        let first = tmp.path().join("game.zip.001");
        fs::write(&first, &bytes[..boundary]).unwrap();
        fs::write(first.with_extension("002"), &bytes[boundary..]).unwrap();
        let out = tmp.path().join("out");

        crash_extract(&first, &out, "after-journal", 2);
        let crashed = restore(&first, &[("HATCHZIP_CRASH_AT", "restore-written")]);
        assert!(!crashed.status.success());
        assert!(fs::metadata(&first).unwrap().len() > boundary as u64);
        assert!(!first.with_extension("002").exists());

        let check = hatchzip(&["check", first.to_str().unwrap()], &[]);
        assert!(String::from_utf8_lossy(&check.stderr).contains("restore"));

        if then == "restore" {
            assert_ok(&restore(&first, &[]));
            assert_side_files_gone(&first);
            let listing = read_zip(joined(&first));
            assert_split_between(&out, &listing, &files);
            check_with_7z(&first, &listing);
        } else {
            assert_ok(&extract(&first, &out, &[]));
            assert_tree(&out, &files);
            assert_consumed(&first);
        }
    }
}

#[test]
fn restores_a_tar_and_puts_back_hard_links_to_what_is_left() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_tar(tmp.path(), &files);
    let out = tmp.path().join("out");

    crash_extract(&archive, &out, "after-journal", 6);
    assert_ok(&restore(&archive, &[]));
    assert_side_files_gone(&archive);

    let bytes = fs::read(&archive).unwrap();
    assert!(bytes.ends_with(&[0; 1024]));
    let listing = read_tar(bytes);
    assert_split_between(&out, &listing, &files);
    assert!(listing.files.contains_key(HARD_TARGET));
    assert_eq!(listing.hard_links[HARD_LINK], HARD_TARGET);
    assert!(!out.join(HARD_LINK).exists());
    #[cfg(unix)]
    assert_eq!(
        fs::read_link(out.join(SYMLINK)).unwrap(),
        Path::new("config/settings.ini")
    );
    check_with_tar(&archive, &listing);

    // GNU tar makes the hard link from it.
    let unpacked = tmp.path().join("unpacked");
    fs::create_dir(&unpacked).unwrap();
    if let Some(x) = real(
        "tar",
        &[
            "-xf",
            archive.to_str().unwrap(),
            "-C",
            unpacked.to_str().unwrap(),
        ],
    ) {
        assert!(x.status.success(), "{}", String::from_utf8_lossy(&x.stderr));
        assert_eq!(
            fs::read(unpacked.join(HARD_LINK)).unwrap(),
            files[HARD_TARGET]
        );
    }
}

#[test]
fn restores_a_7z() {
    for solid in [false, true] {
        let tmp = TempDir::new().unwrap();
        let files = sample_files();
        let archive = build_7z(tmp.path(), &files, solid);
        let out = tmp.path().join("out");

        crash_extract(&archive, &out, "after-journal", if solid { 1 } else { 15 });
        assert_ok(&restore(&archive, &[]));
        assert_side_files_gone(&archive);

        let listing = read_7z(fs::read(&archive).unwrap());
        assert_split_between(&out, &listing, &files);
        check_with_7z(&archive, &listing);

        assert_ok(&extract(&archive, &out, &[]));
        assert_tree(&out, &files);
        assert_consumed(&archive);
    }
}

/// Stored, sevenz-rust2 gives an empty file a block with no packed bytes,
/// which starts where the next block does. Its file is made with the
/// folders, first, so the new header leaves that block out.
#[test]
fn restores_a_stored_7z_whose_empty_file_has_a_block() {
    let tmp = TempDir::new().unwrap();
    let mut files = Files::new();
    files.insert("game/empty.txt".into(), Vec::new());
    for i in 0..4 {
        files.insert(format!("game/{i}.bin"), noise(2000, i));
    }
    let archive = tmp.path().join("game.7z");
    let mut w = ArchiveWriter::create(&archive).unwrap();
    w.set_content_methods(vec![sevenz_rust2::EncoderMethod::COPY.into()]);
    w.push_archive_entry::<&[u8]>(ArchiveEntry::new_directory(EMPTY_DIR), None)
        .unwrap();
    for (name, data) in &files {
        w.push_archive_entry(ArchiveEntry::new_file(name), Some(data.as_slice()))
            .unwrap();
    }
    w.finish().unwrap();
    let out = tmp.path().join("out");

    crash_extract(&archive, &out, "after-journal", 2);
    assert_ok(&restore(&archive, &[]));
    let listing = read_7z(fs::read(&archive).unwrap());
    assert!(out.join("game/empty.txt").is_file());
    assert_split_between(&out, &listing, &files);
    check_with_7z(&archive, &listing);

    assert_ok(&extract(&archive, &out, &[]));
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

#[test]
fn restores_a_split_7z() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let first = split_into(&build_7z(tmp.path(), &files, false), 4);
    let out = tmp.path().join("out");

    crash_extract(&first, &out, "after-journal", 20);
    assert_ok(&restore(&first, &[]));
    let listing = read_7z(joined(&first));
    assert_split_between(&out, &listing, &files);
    check_with_7z(&first, &listing);
}

type Build = fn(&Path, &Files) -> PathBuf;

#[test]
fn an_interrupted_restore_can_be_run_again_or_extracted() {
    let points = ["restore-appended", "restore-written"];
    let builds: [(&str, Build); 3] = [
        ("zip", |d, f| build_zip(d, f, false)),
        ("tar", build_tar),
        ("7z", |d, f| build_7z(d, f, false)),
    ];
    for (format, build) in builds {
        for point in points {
            for then in ["restore", "extract"] {
                let tmp = TempDir::new().unwrap();
                let files = sample_files();
                let archive = build(tmp.path(), &files);
                let out = tmp.path().join("out");
                let what = format!("{format} {point} then {then}");

                crash_extract(&archive, &out, "after-journal", 8);
                let crashed = restore(&archive, &[("HATCHZIP_CRASH_AT", point)]);
                assert!(!crashed.status.success(), "{what}: should have crashed");
                for p in side_files(&archive) {
                    assert!(p.exists(), "{what}: {} is missing", p.display());
                }

                if then == "restore" {
                    assert_ok(&restore(&archive, &[]));
                    assert_side_files_gone(&archive);
                    let bytes = fs::read(&archive).unwrap();
                    let listing = match format {
                        "zip" => read_zip(bytes),
                        "tar" => read_tar(bytes),
                        _ => read_7z(bytes),
                    };
                    assert_split_between(&out, &listing, &files);
                } else {
                    assert_ok(&extract(&archive, &out, &[]));
                    let mut want = files.clone();
                    if format == "tar" {
                        want.insert(HARD_LINK.into(), files[HARD_TARGET].clone());
                        assert!(out.join(SYMLINK).exists(), "{what}: {SYMLINK}");
                        fs::remove_file(out.join(SYMLINK)).unwrap();
                    }
                    assert_tree(&out, &want);
                    assert_consumed(&archive);
                }
            }
        }
    }
}

#[test]
fn restoring_twice_finds_nothing_the_second_time() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_zip(tmp.path(), &files, false);
    crash_extract(&archive, &tmp.path().join("out"), "after-journal", 5);
    assert_ok(&restore(&archive, &[]));
    let once = fs::read(&archive).unwrap();

    let again = restore(&archive, &[]);
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&again.stderr).contains("nothing to restore"));
    assert_eq!(fs::read(&archive).unwrap(), once);
}

#[test]
fn an_archive_never_extracted_has_nothing_to_restore() {
    let tmp = TempDir::new().unwrap();
    let archive = build_zip(tmp.path(), &sample_files(), false);
    let before = fs::read(&archive).unwrap();

    let run = restore(&archive, &[]);
    assert_eq!(run.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&run.stderr).contains("nothing to restore"));
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert_side_files_gone(&archive);
}

/// A crash before the first commit leaves the archive whole; restore only
/// clears away what the uncommitted batch wrote.
#[test]
fn restoring_before_the_first_commit_leaves_the_original() {
    let tmp = TempDir::new().unwrap();
    let files = sample_files();
    let archive = build_zip(tmp.path(), &files, false);
    let before = fs::read(&archive).unwrap();
    let out = tmp.path().join("out");

    crash_extract(&archive, &out, "after-extract", 0);
    assert!(!files_in(&out).is_empty());
    assert_ok(&restore(&archive, &[]));
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert!(files_in(&out).is_empty());
    assert_side_files_gone(&archive);
}

/// A native split ZIP of `files` and `EMPTY_DIR`, made from [`zip_bytes`]
/// so `unzip -t` can read every method in it.
fn split_of(dir: &Path, files: &Files, s: &Split) -> PathBuf {
    let segments = split_zip_segments(&zip_bytes(files, false), s);
    write_split_zip(dir, "game", &segments, s.upper)
}

/// A native split ZIP of the sample in two segments: `.z01` holds everything
/// up to inside small.pak, the `.zip` the rest and the index. A cut among
/// the last entries stays inside the `.zip`; one below small.pak leaves only
/// `.z01`, and restore writes a new `.zip` holding just the index.
fn two_segment_split(dir: &Path, files: &Files) -> PathBuf {
    let size = 3_200_000;
    let mut z = zip::ZipArchive::new(Cursor::new(zip_bytes(files, false))).unwrap();
    let i = z.index_for_name("game/data/small.pak").unwrap();
    let f = z.by_index_raw(i).unwrap();
    let (header, end) = (
        f.header_start(),
        f.data_start().unwrap() + f.compressed_size(),
    );
    let marker = Split::default().marker.len() as u64;
    assert!(
        header + marker < size && size < end + marker,
        "small.pak is at {header}..{end}: the cut is not inside it"
    );
    split_of(
        dir,
        files,
        &Split {
            size: size as usize,
            ..Split::default()
        },
    )
}

/// How many one-entry commits leave the cut below small.pak, inside `.z01`.
fn commits_into_the_first_segment(files: &Files) -> u32 {
    let z = zip::ZipArchive::new(Cursor::new(zip_bytes(files, false))).unwrap();
    // Everything after the directory, settings.ini and big.pak.
    z.len() as u32 - 3
}

/// The restored split ZIP read back: extracting it gives exactly the files
/// the first run did not, and Info-ZIP, where installed, joins it with
/// `zip -s 0` and `unzip -t` passes the result.
fn check_restored_split(first: &Path, out: &Path, files: &Files) {
    let parts = split_zip_parts(first);
    assert!(
        parts
            .last()
            .is_some_and(|p| p.extension().unwrap() == "zip"),
        "{parts:?}"
    );
    let joined = first.with_file_name("joined.zip");
    let last = parts.last().unwrap().to_str().unwrap();
    if let Some(run) = real(
        "zip",
        &["-q", "-s", "0", last, "--out", joined.to_str().unwrap()],
    ) {
        assert!(
            run.status.success(),
            "zip -s 0: {}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        if let Some(t) = real("unzip", &["-t", joined.to_str().unwrap()]) {
            assert!(t.status.success(), "{}", String::from_utf8_lossy(&t.stdout));
        }
        fs::remove_file(&joined).unwrap();
    } else {
        eprintln!("zip not installed; skipping");
    }

    let again = first.with_file_name("again");
    assert_ok(&extract(first, &again, &[]));
    let mut listing = Listing {
        files: files_in(&again),
        ..Listing::default()
    };
    listing.names = listing.files.keys().cloned().collect();
    if again.join(EMPTY_DIR).is_dir() {
        listing.names.insert(EMPTY_DIR.to_string());
    }
    assert_split_between(out, &listing, files);
    assert!(
        split_zip_parts(first).is_empty(),
        "segments left after extracting"
    );
}

#[test]
fn restores_a_native_split_zip() {
    let files = sample_files();
    let low = commits_into_the_first_segment(&files);
    let many: Build = |d, f| split_of(d, f, &Split::default());
    let zip64: Build = |d, f| {
        split_of(
            d,
            f,
            &Split {
                zip64: true,
                ..Split::default()
            },
        )
    };
    let boundaries: Build = |d, f| {
        split_of(
            d,
            f,
            &Split {
                at_headers: true,
                ..Split::default()
            },
        )
    };
    // With 200 000-byte segments, big.pak's header is in `.z01` and every
    // entry after small.pak in the `.zip`. `low` commits cut at big.pak, so
    // restore shortens `.z01`, deletes every segment after it and writes the
    // index alone as a new `.zip`: two parts are left.
    let builds: [(&str, Build, u32, Option<usize>); 7] = [
        ("cut inside the .zip", two_segment_split, 0, Some(2)),
        ("only .z01 left", two_segment_split, low - 1, Some(2)),
        ("many segments, cut inside the .zip", many, 8, None),
        ("cut in the first of many segments", many, low, Some(2)),
        ("cut at a segment boundary", boundaries, 5, None),
        ("ZIP64 fields, cut inside the .zip", zip64, 8, None),
        (
            "ZIP64 fields, cut in the first of many segments",
            zip64,
            low,
            Some(2),
        ),
    ];
    for (what, build, after, parts) in builds {
        let tmp = TempDir::new().unwrap();
        let first = build(tmp.path(), &files);
        let out = tmp.path().join("out");
        crash_extract(&first, &out, "after-journal", after);
        let run = restore(&first.with_extension("zip"), &[]);
        assert_ok(&run);
        assert_side_files_gone(&first);
        assert!(
            !tmp.path().join("game.zip.hatchzip").exists(),
            "{what}: stub left"
        );
        if let Some(n) = parts {
            assert_eq!(split_zip_parts(&first).len(), n, "{what}");
        }
        check_restored_split(&first, &out, &files);
    }
}

/// Killed after it wrote the new index, a restore is undone by the next
/// run, `restore` or `extract`, whether it appended to the `.zip` or
/// created a new one.
#[test]
fn an_interrupted_restore_of_a_native_split_zip_is_undone() {
    let files = sample_files();
    let low = commits_into_the_first_segment(&files);
    for after in [0, low - 1] {
        for point in ["restore-appended", "restore-written"] {
            for then in ["restore", "extract"] {
                let what = format!("{after} {point} then {then}");
                let tmp = TempDir::new().unwrap();
                let first = two_segment_split(tmp.path(), &files);
                let out = tmp.path().join("out");
                crash_extract(&first, &out, "after-journal", after);

                let crashed = restore(&first, &[("HATCHZIP_CRASH_AT", point)]);
                assert!(!crashed.status.success(), "{what}: should have crashed");
                for p in side_files(&first) {
                    assert!(p.exists(), "{what}: {} is missing", p.display());
                }

                if then == "restore" {
                    assert_ok(&restore(&first, &[]));
                    assert_side_files_gone(&first);
                    check_restored_split(&first, &out, &files);
                } else {
                    assert_ok(&extract(&first, &out, &[]));
                    assert_tree(&out, &files);
                    assert_consumed(&first);
                    assert!(split_zip_parts(&first).is_empty(), "{what}");
                }
            }
        }
    }
}

/// The new `.zip` goes where the extraction deleted the old one. Anything
/// found there now is left alone, and the restore waits for it to move.
#[test]
fn restoring_a_native_split_zip_never_writes_over_a_file() {
    let files = sample_files();
    let low = commits_into_the_first_segment(&files);
    let tmp = TempDir::new().unwrap();
    let first = two_segment_split(tmp.path(), &files);
    let out = tmp.path().join("out");
    // Killed while extracting big.pak: the commits up to small.pak are
    // journaled and applied, so `.z01` is cut and the `.zip` deleted.
    crash_extract(&first, &out, "after-extract", low);
    let last = first.with_extension("zip");
    assert!(!last.exists());
    fs::write(&last, b"someone else's").unwrap();
    let left = fs::read(&first).unwrap();

    let run = restore(&first, &[]);
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("exists"));
    assert_eq!(fs::read(&last).unwrap(), b"someone else's");
    assert_eq!(fs::read(&first).unwrap(), left);
    for p in side_files(&first) {
        assert!(p.exists(), "{} is missing", p.display());
    }

    fs::remove_file(&last).unwrap();
    assert_ok(&restore(&first, &[]));
    check_restored_split(&first, &out, &files);
}
