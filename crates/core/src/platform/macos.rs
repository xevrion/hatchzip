use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

pub use super::unix::{
    allocated_bytes, apply_mode, create_new_no_follow, create_symlink, name_problem,
    open_exclusive, open_existing_no_follow, open_folder_no_follow, path_len, same_filesystem,
    sync_dir,
};

/// `PATH_MAX` is 1024 bytes on macOS, including the terminating NUL;
/// `.hatchzip-part` is added to a file's path while it is written.
pub const MAX_PATH_LEN: usize = 1023 - ".hatchzip-part".len();

/// macOS has no `syncfs`. `File::sync_all` issues `F_FULLFSYNC` there, which
/// is the only call that gets data past the drive's own cache.
pub const SYNC_EACH_FILE: bool = true;

pub fn sync_batch(_output: &Path) -> io::Result<()> {
    Ok(())
}

fn block_size(file: &File) -> io::Result<u64> {
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from(st.f_bsize).max(512))
}

/// Frees `len` bytes at `offset` without changing the file's length. Returns
/// `Ok(false)` when the filesystem can't (HFS+, FAT32, exFAT). `F_PUNCHHOLE`
/// refuses ranges that aren't whole blocks, so the range shrinks inwards to
/// block boundaries: punching less than asked is always safe.
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<bool> {
    let bs = block_size(file)?;
    let start = offset.next_multiple_of(bs);
    let end = (offset + len) / bs * bs;
    if start >= end {
        return Ok(true);
    }
    let (Ok(off), Ok(len)) = (i64::try_from(start), i64::try_from(end - start)) else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    let arg = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: off,
        fp_length: len,
    };
    loop {
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &arg) } != -1 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => continue,
            // APFS answers ENOSPC for a file that shares blocks with a clone:
            // punching it would need new space instead of freeing any.
            // macOS 27's FAT32 driver (fskit) answers ENOTTY, where older
            // releases answered ENOTSUP.
            Some(libc::ENOTSUP | libc::EINVAL | libc::ENOSPC | libc::ENOTTY) => {
                return Ok(false);
            }
            _ => return Err(e),
        }
    }
}

/// APFS allocates 4 KiB blocks; 64 KiB leaves a margin for anything coarser.
pub fn punch_granule(_dir: &Path) -> u64 {
    64 << 10
}

/// An archive path as UnRAR's C interface takes it: a wide string here.
#[cfg(feature = "rar")]
pub struct RarName(Vec<unrar_ng_sys::WCHAR>);

#[cfg(feature = "rar")]
impl RarName {
    pub fn new(path: &Path) -> io::Result<Self> {
        // macOS paths are UTF-8, so nothing is lost here.
        let text = path.to_string_lossy();
        if text.contains('\0') {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut wide: Vec<unrar_ng_sys::WCHAR> =
            text.chars().map(|c| c as unrar_ng_sys::WCHAR).collect();
        wide.push(0);
        Ok(Self(wide))
    }

    pub fn open_data(&self, mode: std::ffi::c_uint) -> unrar_ng_sys::OpenArchiveDataEx {
        unrar_ng_sys::OpenArchiveDataEx::new(self.0.as_ptr(), mode)
    }
}
