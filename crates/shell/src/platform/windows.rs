//! Explorer's classic context menu, through per-user registry keys under
//! `HKCU\Software\Classes\SystemFileAssociations`. They add a HatchZip
//! submenu to each archive type without taking over its file association.
//! Windows 11 shows the classic menu under "Show more options"; a top-level
//! entry there needs a packaged shell extension.
//!
//! When `hatchzip_shell_ext.dll` sits next to the exe, the "Extract to"
//! entry also names a command handler from it (`crates/shell-ext`), which
//! Explorer asks for the entry's text, so it can show the folder name. The
//! handler is a per-user COM class under `HKCU\Software\Classes\CLSID`.

use std::path::Path;

use anyhow::{Context, Result};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use crate::explorer::{HANDLER_DLL, clsid_string};
use crate::{ACTIONS, ALL_EXTENSIONS, Dirs, EXTENSIONS};

const BASE: &str = r"Software\Classes\SystemFileAssociations";

/// The one entry the handler takes over.
const HANDLED: &str = "extract-to";

fn menu_key(ext: &str) -> String {
    format!(r"{BASE}\.{ext}\shell\HatchZip")
}

fn clsid_key() -> String {
    format!(r"Software\Classes\CLSID\{}", clsid_string())
}

pub fn current_dirs() -> Result<Dirs> {
    Ok(Dirs {
        data: std::env::temp_dir(),
    })
}

pub fn register(_dirs: &Dirs, gui: &Path) -> Result<Vec<String>> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let exe = gui.to_string_lossy();
    let mut written = Vec::new();

    // Without the DLL, as in a build from source, the entry keeps its static
    // label. A handler registered earlier from somewhere else is removed, so
    // it can't point Explorer at a file that is gone.
    let dll = gui
        .parent()
        .map(|dir| dir.join(HANDLER_DLL))
        .filter(|p| p.is_file());
    remove_handler(&hkcu)?;
    if let Some(dll) = &dll {
        let path = clsid_key();
        let (class, _) = hkcu
            .create_subkey(&path)
            .with_context(|| format!("creating HKCU\\{path}"))?;
        class.set_value("", &"HatchZip Explorer command")?;
        let (server, _) = class.create_subkey("InprocServer32")?;
        server.set_value("", &dll.to_string_lossy().into_owned())?;
        // Explorer builds its menus in a single-threaded apartment, and the
        // handler shares nothing between threads but an atomic count.
        server.set_value("ThreadingModel", &"Apartment")?;
        written.push(format!("Explorer: command handler {}", dll.display()));
    }

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
            if a.id == HANDLED && dll.is_some() {
                item.set_value("ExplorerCommandHandler", &clsid_string())?;
            }
            // Kept with a handler too, so the key is still a complete
            // static verb.
            let (command, _) = item.create_subkey("command")?;
            command.set_value("", &format!("\"{exe}\" {} \"%1\"", a.flag))?;
        }
        written.push(format!("Explorer: .{ext} files"));
    }
    Ok(written)
}

/// Deletes the handler's class. Explorer may still have the DLL loaded; it
/// stops creating new objects from it once the class is gone.
fn remove_handler(hkcu: &RegKey) -> Result<()> {
    match hkcu.delete_subkey_all(clsid_key()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing HKCU\\{}", clsid_key()))
        }
        _ => Ok(()),
    }
}

pub fn unregister(_dirs: &Dirs) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    // The menu keys first, since they name the class.
    for ext in ALL_EXTENSIONS {
        match hkcu.delete_subkey_all(menu_key(ext)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing HKCU\\{}", menu_key(ext)));
            }
            _ => {}
        }
    }
    remove_handler(&hkcu)
}

pub fn is_registered(_dirs: &Dirs) -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(menu_key(EXTENSIONS[0]))
        .is_ok()
}
