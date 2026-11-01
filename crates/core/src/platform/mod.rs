//! Everything that differs between operating systems. Each file provides the
//! same items; nothing outside this module may use `cfg(target_os)`.

#[cfg(unix)]
mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
compile_error!("HatchZip has no platform module for this system yet; add one in src/platform/");

#[cfg(all(test, target_os = "linux"))]
mod tests {
    /// Every filesystem a Linux test run lands on (ext4, XFS, btrfs, tmpfs)
    /// can punch holes, so a probe that says otherwise is broken.
    #[test]
    fn linux_temp_dirs_can_punch() {
        assert!(crate::fsutil::can_punch(&std::env::temp_dir()));
    }

    /// Those filesystems keep names apart unless made not to, so a check
    /// that says otherwise is broken, and the tests that extract names that
    /// differ only in case would quietly test the refusal instead.
    #[test]
    fn linux_temp_dirs_keep_case_apart() {
        assert!(super::case_sensitive(&std::env::temp_dir()));
    }
}
