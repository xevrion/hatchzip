use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

pub use super::unix::{
    allocated_bytes, apply_mode, create_new_no_follow, create_symlink, link_count, name_problem,
    open_exclusive, open_existing_no_follow, path_len, same_filesystem, sync_dir,
};

/// `PATH_MAX` is 4096 bytes including the terminating NUL; `.hatchzip-part`
/// is added to a file's path while it is written.
pub const MAX_PATH_LEN: usize = 4095 - ".hatchzip-part".len();

/// One `syncfs` per batch replaces an fsync per file, which matters when a
/// batch holds thousands of small files.
pub const SYNC_EACH_FILE: bool = false;

pub fn sync_batch(output: &Path) -> io::Result<()> {
    let f = File::open(output)?;
    if unsafe { libc::syncfs(f.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Frees `len` bytes at `offset` without changing the file's length. Returns
/// `Ok(false)` when the filesystem can't, as FAT32 and exFAT can't. The
/// kernel zeroes partial blocks at either end and frees the whole ones in
/// between, so the range needs no alignment.
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<bool> {
    if len == 0 {
        return Ok(true);
    }
    let (Ok(off), Ok(len)) = (i64::try_from(offset), i64::try_from(len)) else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    loop {
        let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
        if unsafe { libc::fallocate(file.as_raw_fd(), mode, off, len) } == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EOPNOTSUPP | libc::ENOSYS) => return Ok(false),
            _ => return Err(e),
        }
    }
}

const BTRFS_MAGIC: i64 = 0x9123_683E;
const ZFS_MAGIC: i64 = 0x2FC1_2FC1;

/// The most of a punched range the filesystem may keep allocated at each end.
/// btrfs only frees an extent once nothing references any of it, and its
/// data extents reach 128 MiB; ZFS frees whole records, up to 16 MiB.
/// Everything else frees whole blocks, and 64 KiB covers every block size in
/// use.
pub fn punch_granule(dir: &Path) -> u64 {
    let Ok(f) = File::open(dir) else {
        return 128 << 20;
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(f.as_raw_fd(), &mut st) } != 0 {
        return 128 << 20;
    }
    match st.f_type as i64 {
        BTRFS_MAGIC => 128 << 20,
        ZFS_MAGIC => 16 << 20,
        _ => (st.f_bsize as u64).max(64 << 10),
    }
}

/// An archive path as UnRAR's C interface takes it: a narrow string here.
#[cfg(feature = "rar")]
pub struct RarName(std::ffi::CString);

#[cfg(feature = "rar")]
impl RarName {
    pub fn new(path: &Path) -> io::Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::CString::new(path.as_os_str().as_bytes())
            .map(Self)
            .map_err(|_| io::ErrorKind::InvalidInput.into())
    }

    pub fn open_data(&self, mode: std::ffi::c_uint) -> unrar_ng_sys::OpenArchiveDataEx {
        unrar_ng_sys::OpenArchiveDataEx::new(self.0.as_ptr(), mode)
    }
}
