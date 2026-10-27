//! `hatchzip_shell_ext.dll`, the Explorer command handler behind HatchZip's
//! "Extract to" menu entry on Windows. Explorer asks it for the entry's text
//! each time the menu opens, so the entry can name the folder, as in
//! `Extract to "game\"`, and runs `hatchzip-gui.exe --extract-to` through it.
//!
//! `hatchzip-shell` registers it and holds the label text, which is tested
//! there on every system. Only Windows has anything to load this into, so
//! on other systems the library is empty.

mod platform;
