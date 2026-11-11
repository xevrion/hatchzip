use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

pub use super::unix::{
    allocated_bytes, apply_mode, create_new_no_follow, create_symlink, link_count, local_time,
    name_problem, open_exclusive, open_existing_no_follow, open_folder_no_follow, path_len,
    same_filesystem, sync_dir, unlock_folder,
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
pub fn punch_margin(dir: &Path) -> u64 {
    match filesystem(dir) {
        Some((BTRFS_MAGIC, _)) => 128 << 20,
        Some((ZFS_MAGIC, _)) => 16 << 20,
        Some((_, block)) => block.max(64 << 10),
        None => 128 << 20,
    }
}

/// How far before the last punch ended the next one starts to free the block
/// it kept: one filesystem block. The kernel zeroes the partial blocks at
/// either end and frees the whole ones in between, so only the block holding
/// the end stays, and the next punch frees it by reaching its start.
///
/// ZFS zeroes partial records instead of freeing them, and a record is not
/// `f_bsize`, so its margin stays the overlap until someone measures one.
pub fn punch_overlap(dir: &Path) -> u64 {
    match filesystem(dir) {
        Some((ZFS_MAGIC, _)) => punch_margin(dir),
        // A block size of zero would round every range away, and some FUSE
        // filesystems report one.
        Some((_, block)) => block.max(4096),
        // Nothing is known about it, so keep the old value rather than
        // punch a range that leaves a block behind.
        None => 64 << 10,
    }
}

/// The filesystem type where `dir` lives, and its block size.
fn filesystem(dir: &Path) -> Option<(i64, u64)> {
    let f = File::open(dir).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(f.as_raw_fd(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_type as i64, st.f_bsize as u64))
}

/// Whether this system can treat different names as one file on a case-sensitive
/// filesystem. Linux leaves names as they are.
pub const NORMALIZES_NAMES: bool = false;

/// Whether `dir` is known to keep apart names that differ only in case. What
/// isn't known counts as not: only btrfs always does, and the rest can be
/// asked. A filesystem this doesn't list can ignore case on its own terms,
/// such as a ZFS dataset. bcachefs stays out until it has been run: it would
/// have to report the casefold flag here, and pass it on to new folders.
pub fn case_sensitive(dir: &Path) -> bool {
    use std::os::unix::fs::OpenOptionsExt;

    const EXT: u32 = 0xEF53;
    const F2FS: u32 = 0xF2F5_2010;
    const TMPFS: u32 = 0x0102_1994;
    const XFS: u32 = 0x5846_5342;

    // Folders only: opening a FIFO found here would wait for a writer.
    let Ok(f) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(dir)
    else {
        return false;
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(f.as_raw_fd(), &mut st) } != 0 {
        return false;
    }
    // The magic numbers are 32 bits, and `f_type` is signed on some systems.
    match st.f_type as u32 {
        magic if magic == BTRFS_MAGIC as u32 => true,
        EXT | F2FS | TMPFS => folder_ignores_case(&f) == Some(false),
        XFS => xfs_ignores_case(&f) == Some(false),
        _ => false,
    }
}

/// Whether a folder on ext4, f2fs or tmpfs has the casefold flag, which
/// folders made inside it inherit. `None` when it can't be asked.
fn folder_ignores_case(dir: &File) -> Option<bool> {
    const FS_CASEFOLD_FL: libc::c_uint = 0x4000_0000;

    let mut flags: libc::c_uint = 0;
    // SAFETY: the kernel writes an int here, whatever the size in the number.
    let got = unsafe { libc::ioctl(dir.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) };
    (got == 0).then_some(flags & FS_CASEFOLD_FL != 0)
}

/// Whether an XFS filesystem was made with ASCII case-insensitive names,
/// which `mkfs.xfs -n version=ci` does for all of it.
fn xfs_ignores_case(any: &File) -> Option<bool> {
    /// `struct xfs_fsop_geom_v1`: the first version, which kernels still take.
    #[repr(C)]
    struct Geometry {
        _sizes: [u32; 8],
        _blocks: [u64; 4],
        _uuid: [u8; 16],
        _stripes_and_version: [u32; 3],
        flags: u32,
        _more_sizes: [u32; 3],
    }
    const FSGEOMETRY_V1: libc::Ioctl = libc::_IOR::<Geometry>('X' as u32, 100);
    const FLAGS_DIRV2CI: u32 = 1 << 12;

    // SAFETY: all integers. The size the kernel writes is in the number, and
    // the layout above follows `xfs_fs.h`.
    let mut geometry: Geometry = unsafe { std::mem::zeroed() };
    let got = unsafe { libc::ioctl(any.as_raw_fd(), FSGEOMETRY_V1, &mut geometry) };
    (got == 0).then_some(geometry.flags & FLAGS_DIRV2CI != 0)
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

#[cfg(test)]
mod tests {
    /// The overlap is the block the next punch reaches back to free, so it has
    /// to be that block. The old 64 KiB margin passed every other test and
    /// punched up to 64 KiB of extra hole at every point.
    #[test]
    fn the_overlap_is_one_block() {
        let dir = std::env::temp_dir();
        let Some((fs, block)) = super::filesystem(&dir) else {
            return;
        };
        // ZFS uses its margin here on purpose: no record size was measured.
        if fs == super::ZFS_MAGIC {
            return;
        }
        assert_eq!(
            super::punch_overlap(&dir),
            block.max(4096),
            "block size {block}"
        );
    }
}
