//! Everything in the app that differs between operating systems. Each file
//! provides the same items; nothing outside this module may use
//! `cfg(target_os)`.

// Each variant is used by a different platform module.
#[allow(dead_code)]
pub enum ShellMenuName {
    Explorer,
    Finder,
    Files,
}

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
compile_error!("hatchzip-gui has no platform module for this system yet; add one in src/platform/");
