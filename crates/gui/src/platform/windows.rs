use std::path::PathBuf;

/// The settings switch, named after this system's file manager.
pub const SHELL_MENU_NAME: super::ShellMenuName = super::ShellMenuName::Explorer;

/// Explorer, through the per-user registry hive.
pub const SHELL_MENU: bool = true;

/// The menu bar is a native Win32 menu, with its own edge.
pub const IN_WINDOW_MENU: bool = false;

/// The Slint renderer name to select when SLINT_BACKEND is unset. FemtoVG
/// (OpenGL) leaves an empty black window on machines where the driver cannot
/// present the swapchain, so Windows defaults to the software renderer.
pub const DEFAULT_RENDERER: Option<&str> = Some("software");

/// The system's UI face for each script. Left to itself, the software
/// renderer drew Latin text in Arial, and Simplified Chinese in a Japanese
/// face that lacks most of its characters.
pub fn font_family(language: &str) -> &'static str {
    match language {
        "zh_CN" => "Microsoft YaHei UI",
        "hi" => "Nirmala UI",
        _ => "Segoe UI",
    }
}

/// `%APPDATA%\HatchZip`, which roams with the user's profile.
pub fn config_dir() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(appdata).join("HatchZip"))
}

/// Explorer passes the files as arguments here, so there is nothing to
/// listen for.
pub fn forward_opened_files(_on_open: impl Fn(PathBuf) + 'static) {}
