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

/// Opens an existing folder to set its time, failing if `path` is a link, a
/// junction or anything but a folder. Windows needs backup semantics to open
/// a folder at all.
pub fn open_folder_no_follow(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_WRITE_ATTRIBUTES,
    };
    let dir = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    // Opened this way, a link is the link itself, which is not a folder.
    if !dir.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "it is not a folder",
        ));
    }
    Ok(dir)
}

/// Windows has no folder mode to lock the owner out of a folder.
pub fn unlock_folder(_path: &Path) -> io::Result<()> {
    Ok(())
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

/// Truncates through `SetEndOfFile`, which makes NTFS free the clusters at
/// once. Rust's `File::set_len` goes through `SetFileInformationByHandle`
/// instead, and the freed space stays invisible to free-space queries until
/// that handle closes.
pub fn set_len_now(file: &File, len: u64) -> io::Result<()> {
    use std::io::{Seek, SeekFrom};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{FILE_BEGIN, SetEndOfFile, SetFilePointerEx};

    let mut f = file;
    let at = f.stream_position()?;
    let h = f.as_raw_handle() as HANDLE;
    if unsafe { SetFilePointerEx(h, len as i64, std::ptr::null_mut(), FILE_BEGIN) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { SetEndOfFile(h) } == 0 {
        let e = io::Error::last_os_error();
        let _ = f.seek(SeekFrom::Start(at));
        return Err(e);
    }
    f.seek(SeekFrom::Start(at))?;
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

/// NTFS tracks sparse ranges in units of 16 clusters: 64 KiB with the default
/// 4 KiB clusters, 1 MiB on volumes formatted with 64 KiB ones. `set zero
/// data` rounds inwards to the unit, so a range must start and end on one to
/// free anything, and the overlap has to be the same number: a punch that
/// started later than the unit it kept would leave that unit allocated until
/// the commit truncates.
const PUNCH_UNIT_CLUSTERS: u64 = 16;

/// 64 KiB, the unit with the default clusters, for when the volume can't be
/// asked.
const PUNCH_UNIT_FALLBACK: u64 = PUNCH_UNIT_CLUSTERS * 4096;

/// The unit NTFS frees sparse ranges in, on the volume holding `path`.
fn punch_unit(path: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceW, GetVolumePathNameW};

    const MAX_WIDE_PATH: usize = 261;

    // `GetDiskFreeSpaceW` wants the volume's root, and every path inside a
    // volume names the same volume, including one mounted in a folder.
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut root = [0u16; MAX_WIDE_PATH];
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), MAX_WIDE_PATH as u32) };
    if ok == 0 {
        return PUNCH_UNIT_FALLBACK;
    }
    let (mut sectors, mut bytes, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    let ok = unsafe {
        GetDiskFreeSpaceW(
            root.as_ptr(),
            &mut sectors,
            &mut bytes,
            &mut free,
            &mut total,
        )
    };
    let unit = sectors as u64 * bytes as u64 * PUNCH_UNIT_CLUSTERS;
    // A network stack that answers zeros would round every range away, and
    // `next_multiple_of` divides by this.
    if ok == 0 || unit == 0 {
        return PUNCH_UNIT_FALLBACK;
    }
    unit
}

/// The same, for the archive itself: the volume is taken from its handle,
/// since the caller has no path to give.
fn punch_unit_of(file: &File) -> u64 {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, VOLUME_NAME_DOS};

    let handle = file.as_raw_handle();
    let needed =
        unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, VOLUME_NAME_DOS) };
    if needed == 0 {
        return PUNCH_UNIT_FALLBACK;
    }
    let mut buf = vec![0u16; needed as usize];
    let len = unsafe {
        GetFinalPathNameByHandleW(handle, buf.as_mut_ptr(), buf.len() as u32, VOLUME_NAME_DOS)
    };
    if len == 0 || len >= buf.len() as u32 {
        return PUNCH_UNIT_FALLBACK;
    }
    let name = plain_path(&String::from_utf16_lossy(&buf[..len as usize]));
    punch_unit(Path::new(&name))
}

/// A path from `GetFinalPathNameByHandleW` without the verbatim prefix the
/// volume queries don't take everywhere: `\\?\C:\a` is `C:\a`, and
/// `\\?\UNC\host\share\a` is `\\host\share\a`.
fn plain_path(path: &str) -> String {
    match path.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => path.strip_prefix(r"\\?\").unwrap_or(path).to_string(),
    }
}

/// Frees `len` bytes at `offset` without changing the file's length. Returns
/// `Ok(false)` when the filesystem can't (FAT32, exFAT).
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        FILE_ZERO_DATA_INFORMATION, FSCTL_SET_SPARSE, FSCTL_SET_ZERO_DATA,
    };

    let unit = punch_unit_of(file).max(4096);
    let start = offset.next_multiple_of(unit);
    let end = (offset + len) / unit * unit;
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
pub fn punch_margin(_dir: &Path) -> u64 {
    1 << 20
}

/// Both ends of a punch round inwards to the unit the volume frees in, so the
/// next punch reaches the start of the unit the last one kept. Where clusters
/// are 64 KiB that unit is 1 MiB, which only the commit's truncation can free.
pub fn punch_overlap(dir: &Path) -> u64 {
    punch_unit(dir).max(4096)
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

/// Whether this system can treat different names as one file on a case-sensitive
/// filesystem. Moot here, since no folder counts as one.
pub const NORMALIZES_NAMES: bool = false;

/// Whether `dir` is known to keep apart names that differ only in case. NTFS
/// can, folder by folder, but that isn't checked, so the answer is no.
pub fn case_sensitive(_dir: &Path) -> bool {
    false
}

/// The moment a wall-clock time names in the local zone, with the offset in
/// effect at that date rather than today's.
pub fn local_time(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
) -> Option<std::time::SystemTime> {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{
        SystemTimeToFileTime, TzSpecificLocalTimeToSystemTimeEx,
    };

    let local = SYSTEMTIME {
        wYear: year,
        wMonth: month.into(),
        wDayOfWeek: 0,
        wDay: day.into(),
        wHour: hour.into(),
        wMinute: minute.into(),
        wSecond: second.into(),
        wMilliseconds: 0,
    };
    let mut utc = SYSTEMTIME::default();
    // SAFETY: both pointers are to locals. A null zone means the active one.
    // The Ex variant is the one with dynamic daylight saving settings.
    if unsafe { TzSpecificLocalTimeToSystemTimeEx(std::ptr::null(), &local, &mut utc) } == 0 {
        return None;
    }
    let mut ticks = FILETIME::default();
    // SAFETY: both pointers are to locals.
    if unsafe { SystemTimeToFileTime(&utc, &mut ticks) } == 0 {
        return None;
    }
    crate::fsutil::from_filetime(
        (u64::from(ticks.dwHighDateTime) << 32) | u64::from(ticks.dwLowDateTime),
    )
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

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::Write;

    /// `GetFinalPathNameByHandleW` answers verbatim paths, which the volume
    /// queries below don't take everywhere.
    #[test]
    fn verbatim_paths_lose_their_prefix() {
        assert_eq!(super::plain_path(r"\\?\C:\a\b"), r"C:\a\b");
        assert_eq!(
            super::plain_path(r"\\?\UNC\host\share\a"),
            r"\\host\share\a"
        );
        assert_eq!(super::plain_path(r"C:\a"), r"C:\a");
        assert_eq!(super::plain_path(r"\\host\share\a"), r"\\host\share\a");
    }

    /// The archive handle stays open for the whole run, so a truncation has
    /// to show up in free space before that handle closes. Rust's own
    /// `File::set_len` only makes NTFS release the clusters on close, which
    /// made the next batch believe the disk was full (#79).
    #[test]
    fn truncating_returns_space_before_the_handle_closes() {
        let dir = std::env::temp_dir().join(format!("hatchzip-set-len-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("archive.bin");
        let mut file = File::create(&path).unwrap();
        file.write_all(&vec![0u8; 32 << 20]).unwrap();
        file.sync_all().unwrap();

        let before = crate::fsutil::available_space(&dir).unwrap();
        super::set_len_now(&file, 0).unwrap();
        let after = crate::fsutil::available_space(&dir).unwrap();

        drop(file);
        let _ = fs::remove_dir_all(&dir);
        assert!(
            after >= before + (16 << 20),
            "free space stayed at {before} (now {after}) until the handle closed"
        );
    }
}
