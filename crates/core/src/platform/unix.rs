//! Shared by Linux and macOS.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

pub fn same_filesystem(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    }
}

/// Opens the archive for writing and makes sure no other HatchZip run has it.
/// The lock is advisory, so it never gets in the way of our own readers.
pub fn open_exclusive(path: &Path) -> io::Result<File> {
    let file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Creates `path` for reading and writing, failing if anything is already
/// there. `O_EXCL` alone refuses a link at `path`; `O_NOFOLLOW` says so
/// twice, so no change elsewhere can make this write through one.
pub fn create_new_no_follow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Opens an existing regular file for reading and writing, failing if
/// `path` is a link or anything but a file.
pub fn open_existing_no_follow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "it is not a regular file",
        ));
    }
    Ok(file)
}

/// Bytes the file really occupies on disk. Punching a hole lowers it while
/// the length stays the same.
pub fn allocated_bytes(path: &Path) -> io::Result<u64> {
    Ok(fs::metadata(path)?.blocks() * 512)
}

/// How many names the file has on its filesystem.
pub fn link_count(path: &Path) -> io::Result<u64> {
    Ok(fs::metadata(path)?.nlink())
}

/// Unix filesystems take any name without `/` or NUL, which path parsing
/// already rules out, up to 255 bytes. A file is first written under its
/// name plus `.hatchzip-part`, so that has to fit too.
pub fn name_problem(name: &str) -> Option<&'static str> {
    if name.len() > 255 - ".hatchzip-part".len() {
        return Some("the name is longer than this file system allows");
    }
    None
}

/// How long `path` is, in the units [`MAX_PATH_LEN`](super::MAX_PATH_LEN)
/// counts: bytes on Unix.
pub fn path_len(path: &Path) -> usize {
    path.as_os_str().len()
}

pub fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Never recreate setuid, setgid or sticky bits from an archive.
pub fn apply_mode(file: &File, mode: Option<u32>) -> io::Result<()> {
    match mode {
        Some(mode) => file.set_permissions(fs::Permissions::from_mode(mode & 0o777)),
        None => Ok(()),
    }
}

pub fn create_symlink(link: &Path, target: &Path) -> io::Result<()> {
    match fs::symlink_metadata(link) {
        Ok(_) => fs::remove_file(link)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::os::unix::fs::symlink(target, link)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Neither helper reaches a file through a link, and creating never
    /// replaces what is already there.
    #[test]
    fn links_and_existing_files_are_never_followed_or_replaced() {
        let dir = std::env::temp_dir().join(format!("hatchzip-nofollow-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file");
        fs::write(&file, b"kept").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let dangling = dir.join("dangling");
        std::os::unix::fs::symlink(dir.join("nowhere"), &dangling).unwrap();

        for p in [&file, &link, &dangling] {
            let e = create_new_no_follow(p).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{}", p.display());
        }
        assert!(!dir.join("nowhere").exists());
        assert!(open_existing_no_follow(&link).is_err());
        assert!(open_existing_no_follow(&dir).is_err());
        assert!(open_existing_no_follow(&file).is_ok());
        assert_eq!(fs::read(&file).unwrap(), b"kept");
        assert!(create_new_no_follow(&dir.join("new")).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }
}
