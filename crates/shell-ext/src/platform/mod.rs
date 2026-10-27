//! The handler is Windows only. Other systems get an empty library, so the
//! workspace still builds there.

#[cfg(windows)]
mod windows;
