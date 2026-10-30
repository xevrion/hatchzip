use std::fs::{self, File};
use std::io;
use std::path::{Component, Path};

const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_DELETE: u32 = 0x4;
const ERROR_SHARING_VIOLATION: i32 = 32;

/// Opens the archive for writing so that nobody else can write to it. A
/// byte-range lock would be mandatory here and block our own readers, so the
/// share mode does the job instead: other handles may read, never write.
pub fn open_exclusive(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(path)
        .map_err(|e| match e.raw_os_error() {
            Some(ERROR_SHARING_VIOLATION) => io::ErrorKind::WouldBlock.into(),
            _ => e,
        })
}

/// Creates `path` for reading and writing, failing if anything is already
/// there. With `FILE_FLAG_OPEN_REPARSE_POINT` a symbolic link or junction at
/// `path` counts as something there, never as the way to its target.
pub fn create_new_no_follow(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Opens an existing regular file for reading and writing, failing if
/// `path` is a link, a junction or anything but a file.
pub fn open_existing_no_follow(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    // Opened this way, a link is the link itself, which is not a file.
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "it is not a regular file",
        ));
    }
    Ok(file)
}

/// Why Windows can't create a file or folder with this name, if it can't.
/// Checked for every entry before the archive is touched, so a name made on
/// another system fails up front instead of halfway through a run.
pub fn name_problem(name: &str) -> Option<&'static str> {
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    // `CON.txt` is just as reserved as `CON`.
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        return Some("it is a reserved device name on Windows");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("Windows removes a trailing dot or space from names");
    }
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (c as u32) < 32)
    {
        return Some("it contains a character that Windows does not allow in names");
    }
    // 255 UTF-16 units, including the `.hatchzip-part` a file is first
    // written under.
    if name.encode_utf16().count() > 255 - ".hatchzip-part".len() {
        return Some("the name is longer than Windows allows");
    }
    None
}

/// Paths go through the `\\?\` form, which allows about 32,767 UTF-16 units.
/// Kept below that, with room for the prefix and `.hatchzip-part`.
pub const MAX_PATH_LEN: usize = 32_000;

/// How long `path` is, in UTF-16 units.
pub fn path_len(path: &Path) -> usize {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().count()
}

/// `FlushFileBuffers` per file is the only durable flush Windows offers.
pub const SYNC_EACH_FILE: bool = true;

pub fn sync_batch(_output: &Path) -> io::Result<()> {
    Ok(())
}

/// Directory handles can't be flushed here; NTFS journals renames itself.
pub fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

pub fn same_filesystem(a: &Path, b: &Path) -> bool {
    let volume = |p: &Path| {
        fs::canonicalize(p)
            .ok()
            .and_then(|p| match p.components().next() {
                Some(Component::Prefix(prefix)) => {
                    Some(prefix.as_os_str().to_string_lossy().to_lowercase())
                }
                _ => None,
            })
    };
    volume(a).is_some_and(|v| Some(v) == volume(b))
}

pub fn apply_mode(_file: &File, _mode: Option<u32>) -> io::Result<()> {
    Ok(())
}

/// Windows symlinks need admin rights or developer mode, so the target is
/// stored in a small text file instead, the way Git does it.
pub fn create_symlink(link: &Path, target: &Path) -> io::Result<()> {
    fs::write(link, target.to_string_lossy().as_bytes())
}

const ERROR_INVALID_FUNCTION: i32 = 1;
const ERROR_NOT_SUPPORTED: i32 = 50;

/// The punched range shrinks inwards to this, so NTFS doesn't spend writes
/// zeroing pieces it can't free anyway.
const PUNCH_ALIGN: u64 = 64 << 10;

/// Frees `len` bytes at `offset` without changing the file's length. Returns
/// `Ok(false)` when the filesystem can't (FAT32, exFAT).
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        FILE_ZERO_DATA_INFORMATION, FSCTL_SET_SPARSE, FSCTL_SET_ZERO_DATA,
    };

    let start = offset.next_multiple_of(PUNCH_ALIGN);
    let end = (offset + len) / PUNCH_ALIGN * PUNCH_ALIGN;
    if start >= end {
        return Ok(true);
    }
    let (Ok(start), Ok(end)) = (i64::try_from(start), i64::try_from(end)) else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    let handle = file.as_raw_handle();
    let mut returned = 0u32;
    let failed = |e: io::Error| match e.raw_os_error() {
        Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED) => Ok(false),
        _ => Err(e),
    };
    // Without the sparse flag NTFS answers FSCTL_SET_ZERO_DATA by writing
    // real zeros, which frees nothing.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return failed(io::Error::last_os_error());
    }
    let range = FILE_ZERO_DATA_INFORMATION {
        FileOffset: start,
        BeyondFinalZero: end,
    };
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_ZERO_DATA,
            (&raw const range).cast(),
            size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return failed(io::Error::last_os_error());
    }
    Ok(true)
}

/// NTFS frees sparse ranges in compression units of 16 clusters: 64 KiB with
/// the default 4 KiB clusters, 1 MiB on volumes formatted with 64 KiB ones.
pub fn punch_granule(_dir: &Path) -> u64 {
    1 << 20
}

/// Bytes the file really occupies on disk, which for a sparse file is less
/// than its length.
pub fn allocated_bytes(path: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;

    const INVALID_FILE_SIZE: u32 = u32::MAX;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut high = 0u32;
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
    if low == INVALID_FILE_SIZE {
        // The low half can legitimately be all ones; only an error code says
        // the call failed.
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(0) {
            return Err(e);
        }
    }
    Ok((u64::from(high) << 32) | u64::from(low))
}

/// How many names the file has on its filesystem.
pub fn link_count(path: &Path) -> io::Result<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let file = File::open(path)?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.nNumberOfLinks.into())
}

/// An archive path as UnRAR's C interface takes it: a wide string here.
#[cfg(feature = "rar")]
pub struct RarName(Vec<unrar_ng_sys::WCHAR>);

#[cfg(feature = "rar")]
impl RarName {
    pub fn new(path: &Path) -> io::Result<Self> {
        use std::os::windows::ffi::OsStrExt;
        let mut wide: Vec<unrar_ng_sys::WCHAR> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        wide.push(0);
        Ok(Self(wide))
    }

    pub fn open_data(&self, mode: std::ffi::c_uint) -> unrar_ng_sys::OpenArchiveDataEx {
        unrar_ng_sys::OpenArchiveDataEx::new(self.0.as_ptr(), mode)
    }
}

// UnRAR reads a registry key for its data path. The binding's build script
// asks for this library only when the machine building is Windows, which a
// cross build from elsewhere is not.
#[cfg(feature = "rar")]
#[link(name = "advapi32")]
unsafe extern "C" {}
