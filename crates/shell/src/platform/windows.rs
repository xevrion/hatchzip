//! Explorer's classic context menu, through per-user registry keys under
//! `HKCU\Software\Classes\SystemFileAssociations`. They add a HatchZip
//! submenu to each archive type without taking over its file association.
//! Windows 11 shows the classic menu under "Show more options"; a top-level
//! entry there needs a packaged shell extension.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use crate::{ACTIONS, ALL_EXTENSIONS, Dirs, EXTENSIONS};

const BASE: &str = r"Software\Classes\SystemFileAssociations";

fn menu_key(ext: &str) -> String {
    format!(r"{BASE}\.{ext}\shell\HatchZip")
}

pub fn current_dirs() -> Result<Dirs> {
    Ok(Dirs {
        data: std::env::temp_dir(),
        config: PathBuf::new(),
        system_config: Vec::new(),
    })
}

pub fn register(_dirs: &Dirs, gui: &Path) -> Result<Vec<String>> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let exe = gui.to_string_lossy();
    let mut written = Vec::new();
    for ext in EXTENSIONS {
        let path = menu_key(ext);
        // Items are numbered in their key names, so an older registration
        // whose numbers have shifted would leave duplicates behind.
        let _ = hkcu.delete_subkey_all(&path);
        let (menu, _) = hkcu
            .create_subkey(&path)
            .with_context(|| format!("creating HKCU\\{path}"))?;
        menu.set_value("MUIVerb", &"HatchZip")?;
        menu.set_value("Icon", &format!("\"{exe}\",0"))?;
        // An empty SubCommands value turns the entry into a submenu built
        // from its own `shell` key.
        menu.set_value("SubCommands", &"")?;
        for (i, a) in ACTIONS.iter().enumerate() {
            // Explorer orders submenu items by key name.
            let (item, _) = menu.create_subkey(format!(r"shell\{:02}{}", i + 1, a.id))?;
            item.set_value("MUIVerb", &a.label)?;
            let (command, _) = item.create_subkey("command")?;
            command.set_value("", &format!("\"{exe}\" {} \"%1\"", a.flag))?;
        }
        written.push(format!("Explorer: .{ext} files"));
    }
    Ok(written)
}

pub fn unregister(_dirs: &Dirs) -> Result<Vec<String>> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for ext in ALL_EXTENSIONS {
        match hkcu.delete_subkey_all(menu_key(ext)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing HKCU\\{}", menu_key(ext)));
            }
            _ => {}
        }
    }
    Ok(Vec::new())
}

pub fn is_registered(_dirs: &Dirs) -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(menu_key(EXTENSIONS[0]))
        .is_ok()
}
