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

/// The overlap is what the next punch steps back to free the block the last
/// one kept, so it can never exceed the margin the space estimate reserves.
#[cfg(test)]
mod punch_tests {
    #[test]
    fn the_overlap_fits_inside_the_margin() {
        let dir = std::env::temp_dir();
        let margin = super::punch_margin(&dir);
        let overlap = super::punch_overlap(&dir);
        assert!(overlap > 0, "the overlap is {overlap}");
        assert!(overlap <= margin, "overlap {overlap} > margin {margin}");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    /// Every filesystem a Linux test run lands on (ext4, XFS, btrfs, tmpfs)
    /// can punch holes, so a probe that says otherwise is broken.
    #[test]
    fn linux_temp_dirs_can_punch() {
        assert!(crate::fsutil::can_punch(&std::env::temp_dir()));
    }

    /// CI runs on ext4, so there a no would be a bug, and the tests that
    /// extract names that differ only in case would quietly test the refusal
    /// instead. A build root on overlayfs or ZFS is rightly unknown.
    #[test]
    fn ci_temp_dirs_keep_case_apart() {
        if std::env::var_os("HATCHZIP_EXPECT_PUNCH").is_some() {
            assert!(super::case_sensitive(&std::env::temp_dir()));
        }
    }
}
