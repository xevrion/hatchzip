use std::path::PathBuf;

/// The settings switch, named after this system's file manager.
pub const SHELL_MENU_NAME: super::ShellMenuName = super::ShellMenuName::Files;

/// The file managers hatchzip-shell knows (Dolphin, GNOME Files, Nemo).
pub const SHELL_MENU: bool = true;

/// `$XDG_CONFIG_HOME/hatchzip`, or `~/.config/hatchzip`.
pub fn config_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => std::env::home_dir()?.join(".config"),
    };
    Some(base.join("hatchzip"))
}

/// File managers pass the files as arguments here, so there is nothing to
/// listen for.
pub fn forward_opened_files(_on_open: impl Fn(PathBuf) + 'static) {}
