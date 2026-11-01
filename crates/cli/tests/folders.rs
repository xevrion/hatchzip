//! Folder entries keep their permissions and modified times, set once
//! everything inside them is made.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::*;
use tempfile::TempDir;

fn when() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_000_000_000)
}

fn modified(p: &Path) -> SystemTime {
    fs::metadata(p).unwrap().modified().unwrap()
}

#[cfg(unix)]
fn assert_mode(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let got = fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(got, mode, "{}", p.display());
}

#[cfg(not(unix))]
fn assert_mode(_: &Path, _: u32) {}

/// `docs` (0o750) holding `docs/old` (0o700), each with a file inside, all
/// dated `when()`. `extra` is added as a folder entry at the end.
fn tar_with_folders(dir: &Path, extra: Option<(&str, u32)>) -> PathBuf {
    let mut b = tar::Builder::new(Vec::new());
    let folder = |b: &mut tar::Builder<Vec<u8>>, path: &str, mode: u32| {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(mode);
        h.set_mtime(1_000_000_000);
        h.set_size(0);
        b.append_data(&mut h, path, std::io::empty()).unwrap();
    };
    folder(&mut b, "docs", 0o750);
    folder(&mut b, "docs/old", 0o700);
    for (i, path) in ["docs/a.txt", "docs/old/b.txt"].iter().enumerate() {
        let data = noise(20_000, i as u64);
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(1_000_000_000);
        b.append_data(&mut h, path, data.as_slice()).unwrap();
    }
    if let Some((path, mode)) = extra {
        folder(&mut b, path, mode);
        let mut h = tar::Header::new_gnu();
        h.set_size(4);
        h.set_mode(0o644);
        b.append_data(&mut h, format!("{path}/c.txt"), &b"data"[..])
            .unwrap();
    }
    let path = dir.join("docs.tar");
    fs::write(&path, b.into_inner().unwrap()).unwrap();
    path
}

fn assert_folders(out: &Path) {
    for (path, mode) in [("docs", 0o750), ("docs/old", 0o700)] {
        assert_eq!(modified(&out.join(path)), when(), "{path}");
        assert_mode(&out.join(path), mode);
    }
}

#[test]
fn tar_folders_keep_their_permissions_and_times() {
    let tmp = TempDir::new().unwrap();
    let archive = tar_with_folders(tmp.path(), None);
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &[("HATCHZIP_BATCH_ENTRIES", "1")]));
    assert_folders(&out);
    assert_consumed(&archive);
}

#[test]
fn a_kept_archive_sets_folders_too() {
    let tmp = TempDir::new().unwrap();
    let archive = tar_with_folders(tmp.path(), None);
    let out = tmp.path().join("out");
    let (a, o) = (archive.to_str().unwrap(), out.to_str().unwrap());
    assert_ok(&hatchzip(
        &[
            "extract",
            a,
            "-o",
            o,
            "--keep-archive",
            "--keep-free",
            "0",
            "--yes",
        ],
        &[],
    ));
    assert_folders(&out);
    assert!(archive.exists());
    assert_no_side_files(&archive);
}

/// A folder listed twice takes the mode and time of its last entry.
#[test]
fn a_folder_listed_twice_takes_its_last_entry() {
    let tmp = TempDir::new().unwrap();
    let mut b = tar::Builder::new(Vec::new());
    for (mode, mtime) in [(0o755, 946_684_800), (0o750, 1_000_000_000)] {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(mode);
        h.set_mtime(mtime);
        h.set_size(0);
        b.append_data(&mut h, "docs", std::io::empty()).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_size(4);
        h.set_mode(0o644);
        b.append_data(&mut h, format!("docs/{mtime}.txt"), &b"data"[..])
            .unwrap();
    }
    let archive = tmp.path().join("docs.tar");
    fs::write(&archive, b.into_inner().unwrap()).unwrap();
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[]));
    assert_eq!(modified(&out.join("docs")), when());
    assert_mode(&out.join("docs"), 0o750);
}

/// A crash after the last link and before the folders are set leaves the
/// journal, and the resume sets them.
#[test]
fn a_crash_before_the_folders_are_set_resumes() {
    let tmp = TempDir::new().unwrap();
    let archive = tar_with_folders(tmp.path(), None);
    let out = tmp.path().join("out");
    let crashed = extract(&archive, &out, &[("HATCHZIP_CRASH_AT", "before-folders")]);
    assert!(!crashed.status.success(), "should have crashed");
    assert!(side_files(&archive)[0].exists());
    assert!(folders_file(&archive).exists());

    assert_ok(&extract(&archive, &out, &[]));
    assert_folders(&out);
    assert_consumed(&archive);
}

/// A folder that takes no new files is only made read-only once everything
/// inside it is extracted.
#[cfg(unix)]
#[test]
fn a_read_only_folder_still_gets_its_files() {
    let tmp = TempDir::new().unwrap();
    let archive = tar_with_folders(tmp.path(), Some(("locked", 0o500)));
    let out = tmp.path().join("out");
    assert_ok(&extract(&archive, &out, &[]));
    assert_eq!(fs::read(out.join("locked/c.txt")).unwrap(), b"data");
    assert_mode(&out.join("locked"), 0o500);
    assert_folders(&out);

    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(out.join("locked"), fs::Permissions::from_mode(0o700)).unwrap();
}

/// A crash once the folders are set leaves only them to do. The resume makes
/// no link again inside a folder that no longer takes new entries, and opens
/// folders it already set that it can't read or look into.
#[cfg(unix)]
#[test]
fn a_crash_after_the_folders_are_set_resumes() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let mut b = tar::Builder::new(Vec::new());
    let folder = |b: &mut tar::Builder<Vec<u8>>, path: &str, mode: u32| {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(mode);
        h.set_mtime(1_000_000_000);
        h.set_size(0);
        b.append_data(&mut h, path, std::io::empty()).unwrap();
    };
    let file = |b: &mut tar::Builder<Vec<u8>>, path: &str| {
        let mut h = tar::Header::new_gnu();
        h.set_size(4);
        h.set_mode(0o644);
        b.append_data(&mut h, path, &b"data"[..]).unwrap();
    };
    folder(&mut b, "ro", 0o555);
    file(&mut b, "ro/a.txt");
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_size(0);
    b.append_link(&mut h, "ro/l", "a.txt").unwrap();
    folder(&mut b, "noread", 0o311);
    file(&mut b, "noread/b.txt");
    folder(&mut b, "nosearch", 0o600);
    folder(&mut b, "nosearch/inner", 0o700);
    file(&mut b, "nosearch/inner/c.txt");
    let archive = tmp.path().join("modes.tar");
    fs::write(&archive, b.into_inner().unwrap()).unwrap();
    let out = tmp.path().join("out");

    let crashed = extract(&archive, &out, &[("HATCHZIP_CRASH_AT", "after-folders")]);
    assert!(!crashed.status.success(), "should have crashed");
    assert!(folders_file(&archive).exists());

    assert_ok(&extract(&archive, &out, &[]));
    for (path, mode) in [("ro", 0o555), ("noread", 0o311), ("nosearch", 0o600)] {
        assert_mode(&out.join(path), mode);
        assert_eq!(modified(&out.join(path)), when(), "{path}");
        fs::set_permissions(out.join(path), fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert_mode(&out.join("nosearch/inner"), 0o700);
    assert_eq!(fs::read_link(out.join("ro/l")).unwrap(), Path::new("a.txt"));
    assert_eq!(fs::read(out.join("nosearch/inner/c.txt")).unwrap(), b"data");
    assert_no_side_files(&archive);
}

/// A folder that was in the output before the extraction may belong to
/// someone else, and is left as it was, like an existing file.
#[cfg(unix)]
#[test]
fn a_folder_already_in_the_output_is_left_as_it_is() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let archive = tar_with_folders(tmp.path(), None);
    let out = tmp.path().join("out");
    fs::create_dir_all(out.join("docs")).unwrap();
    fs::set_permissions(out.join("docs"), fs::Permissions::from_mode(0o775)).unwrap();

    assert_ok(&extract(&archive, &out, &[]));
    assert_mode(&out.join("docs"), 0o775);
    assert_mode(&out.join("docs/old"), 0o700);
    assert_eq!(modified(&out.join("docs/old")), when());
    assert_consumed(&archive);
}

/// The saved folders are checked with the rest of the journal, so a resume
/// with the file gone stops before it frees any more of the archive.
#[test]
fn a_missing_folders_file_stops_the_resume_before_anything_is_freed() {
    let tmp = TempDir::new().unwrap();
    let archive = tar_with_folders(tmp.path(), None);
    let out = tmp.path().join("out");
    let crashed = extract(
        &archive,
        &out,
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
        ],
    );
    assert!(!crashed.status.success(), "should have crashed");
    fs::remove_file(folders_file(&archive)).unwrap();
    let before = snapshot(&archive);

    let resumed = extract(&archive, &out, &[]);
    assert!(!resumed.status.success(), "should have refused");
    let err = String::from_utf8_lossy(&resumed.stderr);
    assert!(err.contains("hatchzip-folders"), "{err}");
    assert_unchanged(&before);
}

/// The DOS time of a ZIP folder entry is read the way a file's is, so both
/// end up the same.
#[test]
fn zip_folders_keep_their_permissions_and_times() {
    use std::io::Write;
    use zip::DateTime;
    use zip::write::{SimpleFileOptions, ZipWriter};

    let tmp = TempDir::new().unwrap();
    let time = DateTime::from_date_and_time(2001, 9, 9, 1, 46, 40).unwrap();
    let opts = SimpleFileOptions::default().last_modified_time(time);
    let mut w = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    w.add_directory("docs/", opts.unix_permissions(0o750))
        .unwrap();
    w.start_file("docs/a.txt", opts).unwrap();
    w.write_all(&noise(20_000, 1)).unwrap();
    let archive = tmp.path().join("docs.zip");
    fs::write(&archive, w.finish().unwrap().into_inner()).unwrap();
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[]));
    assert_eq!(
        modified(&out.join("docs")),
        modified(&out.join("docs/a.txt"))
    );
    assert_mode(&out.join("docs"), 0o750);
}

#[test]
fn sevenz_folders_keep_their_permissions_and_times() {
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter, NtTime};

    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("docs.7z");
    let mut w = ArchiveWriter::create(&archive).unwrap();
    let mut docs = ArchiveEntry::new_directory("docs");
    docs.has_last_modified_date = true;
    docs.last_modified_date = NtTime::try_from(when()).unwrap();
    // FILE_ATTRIBUTE_DIRECTORY, with the Unix mode in the high bits.
    docs.has_windows_attributes = true;
    docs.windows_attributes = 0x10 | 0x8000 | ((0o040750) << 16);
    w.push_archive_entry::<&[u8]>(docs, None).unwrap();
    let data = noise(20_000, 1);
    w.push_archive_entry(ArchiveEntry::new_file("docs/a.txt"), Some(data.as_slice()))
        .unwrap();
    w.finish().unwrap();
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[]));
    assert_eq!(modified(&out.join("docs")), when());
    assert_mode(&out.join("docs"), 0o750);
}
