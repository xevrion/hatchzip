//! Dolphin (KDE) service menu, GNOME Files scripts, Nemo actions and Thunar
//! custom actions. Thunar keeps every custom action in one file the user
//! edits too, so only HatchZip's own `<action>` blocks are ever touched there.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::{ACTIONS, Dirs, EXTENSIONS};

#[cfg(not(feature = "rar"))]
const MIME_TYPES: &str = "application/zip;application/x-zip-compressed;application/x-7z-compressed;\
                          application/x-tar;application/x-compressed-tar;\
                          application/x-bzip2-compressed-tar;application/x-bzip-compressed-tar;\
                          application/x-xz-compressed-tar;application/zstd;\
                          application/x-zstd-compressed-tar;";
#[cfg(feature = "rar")]
const MIME_TYPES: &str = "application/zip;application/x-zip-compressed;application/x-7z-compressed;\
                          application/x-tar;application/x-compressed-tar;\
                          application/x-bzip2-compressed-tar;application/x-bzip-compressed-tar;\
                          application/x-xz-compressed-tar;application/zstd;\
                          application/x-zstd-compressed-tar;application/vnd.rar;\
                          application/x-rar;application/x-rar-compressed;";
const MARK: &str = "Added by HatchZip; `hatchzip-gui --unregister-shell` removes it.";

pub fn current_dirs() -> Result<Dirs> {
    let data = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => std::env::home_dir()
            .ok_or_else(|| anyhow!("no home folder to install the menu into"))?
            .join(".local/share"),
    };
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => std::env::home_dir()
            .ok_or_else(|| anyhow!("no home folder to install the menu into"))?
            .join(".config"),
    };
    let system_config = std::env::var_os("XDG_CONFIG_DIRS")
        .filter(|v| !v.is_empty())
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_else(|| vec![PathBuf::from("/etc/xdg")]);
    Ok(Dirs {
        data,
        config,
        system_config,
    })
}

fn kde_file(dirs: &Dirs) -> PathBuf {
    dirs.data.join("kio/servicemenus/hatchzip.desktop")
}

fn nautilus_file(dirs: &Dirs, label: &str) -> PathBuf {
    dirs.data
        .join("nautilus/scripts")
        .join(format!("HatchZip - {label}"))
}

fn nemo_file(dirs: &Dirs, id: &str) -> PathBuf {
    dirs.data
        .join("nemo/actions")
        .join(format!("hatchzip-{id}.nemo_action"))
}

fn thunar_file(dirs: &Dirs) -> PathBuf {
    dirs.config.join("Thunar/uca.xml")
}

const THUNAR_ID: &str = "<unique-id>hatchzip-";
const THUNAR_EMPTY: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n</actions>\n";

/// What Thunar reads today. It reads only the first `uca.xml` it finds, the
/// user's before the system's, so a user file holding only HatchZip's actions
/// would hide the defaults such as "Open Terminal Here". They are copied in.
/// The `bool` says whether the user had a file of their own.
fn thunar_current(dirs: &Dirs) -> Result<(String, bool)> {
    let own = thunar_file(dirs);
    match fs::read_to_string(&own) {
        Ok(text) => return Ok((text, true)),
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("reading {}", own.display()));
        }
        Err(_) => {}
    }
    let system = dirs
        .system_config
        .iter()
        .find_map(|d| fs::read_to_string(d.join("Thunar/uca.xml")).ok());
    Ok((system.unwrap_or_else(|| THUNAR_EMPTY.to_owned()), false))
}

/// The file with HatchZip's `<action>` blocks taken out, together with the
/// whitespace they sat on. Everything else stays byte for byte, including
/// actions the user wrote and whatever Thunar added when it saved the file.
fn thunar_without_ours(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(start) = rest.find("<action>") {
        let Some(len) = rest[start..].find("</action>") else {
            break;
        };
        let end = start + len + "</action>".len();
        if !rest[start..end].contains(THUNAR_ID) {
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        let line = rest[..start].rfind('\n').map_or(0, |i| i + 1);
        let indent_only = rest[line..start].trim().is_empty();
        out.push_str(&rest[..if indent_only { line } else { start }]);
        rest = &rest[end..];
        if indent_only {
            rest = rest.strip_prefix('\n').unwrap_or(rest);
        }
    }
    out.push_str(rest);
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn thunar_actions(gui: &Path) -> String {
    // Thunar expands `%` codes in the command and then splits it the way a
    // shell would, so the program is shell quoted and a literal `%` doubled.
    let program = sh_quote(gui).replace('%', "%%");
    let patterns = EXTENSIONS
        .iter()
        .map(|e| format!("*.{e}"))
        .collect::<Vec<_>>()
        .join(";");
    let mut out = String::new();
    for a in ACTIONS {
        out.push_str(&format!(
            "  <action>\n    <icon>archive-extract</icon>\n    <name>{}</name>\n    \
             <submenu>HatchZip</submenu>\n    <unique-id>hatchzip-{}</unique-id>\n    \
             <command>{}</command>\n    <description>{} with HatchZip</description>\n    \
             <patterns>{patterns}</patterns>\n    <other-files/>\n  </action>\n",
            xml_escape(a.label),
            a.id,
            xml_escape(&format!("{program} {} %F", a.flag)),
            xml_escape(a.label),
        ));
    }
    out
}

/// Writes through a temporary file, so Thunar or a crash never sees half of
/// the user's actions.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("xml.hatchzip-tmp");
    fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

fn thunar_register(dirs: &Dirs, gui: &Path) -> Result<()> {
    let (current, _) = thunar_current(dirs)?;
    let mut xml = thunar_without_ours(&current).replace("<actions/>", "<actions>\n</actions>");
    let Some(close) = xml.rfind("</actions>") else {
        bail!(
            "{} is not a Thunar actions file HatchZip can add to; it was left as it is",
            thunar_file(dirs).display()
        );
    };
    xml.insert_str(close, &thunar_actions(gui));
    write_atomic(&thunar_file(dirs), &xml)
}

fn thunar_unregister(dirs: &Dirs) -> Result<()> {
    let (current, own) = thunar_current(dirs)?;
    if !own || !current.contains(THUNAR_ID) {
        return Ok(());
    }
    let left = thunar_without_ours(&current);
    // A file HatchZip started from nothing goes away with its last action,
    // so Thunar falls back to what it read before.
    if left == THUNAR_EMPTY {
        return fs::remove_file(thunar_file(dirs))
            .with_context(|| format!("removing {}", thunar_file(dirs).display()));
    }
    write_atomic(&thunar_file(dirs), &left)
}

/// Desktop entry `Exec` keys want the program quoted, with `"`, `` ` ``, `$`
/// and `\` escaped.
fn exec_quote(path: &Path) -> String {
    let mut out = String::from("\"");
    for c in path.to_string_lossy().chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn write(path: &Path, contents: &str, executable: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    // Dolphin ignores service menus in a user's folder unless they are
    // executable, and GNOME Files only runs executable scripts.
    let mode = if executable { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

pub fn register(dirs: &Dirs, gui: &Path) -> Result<Vec<String>> {
    // First, as it is the one that can refuse: a Thunar file HatchZip can't
    // add to leaves no other menu half registered.
    thunar_register(dirs, gui)?;
    let exec = exec_quote(gui);

    let mut kde = format!(
        "# {MARK}\n[Desktop Entry]\nType=Service\nMimeType={MIME_TYPES}\nActions={}\n\
         X-KDE-Submenu=HatchZip\nIcon=archive-extract\n",
        ACTIONS.iter().map(|a| a.id).collect::<Vec<_>>().join(";") + ";"
    );
    // Every selected archive goes to one window, which works through them
    // one after another, checking the space for each just before it starts.
    for a in ACTIONS {
        kde.push_str(&format!(
            "\n[Desktop Action {}]\nName={}\nIcon=archive-extract\nExec={exec} {} %F\n",
            a.id, a.label, a.flag
        ));
    }
    write(&kde_file(dirs), &kde, true)?;

    for a in ACTIONS {
        // GNOME Files hands over every selected path, one per line. Split on
        // newlines only, with globbing off, so spaces and stars in names
        // survive.
        let script = format!(
            "#!/bin/sh\n# {MARK}\n[ -n \"$NAUTILUS_SCRIPT_SELECTED_FILE_PATHS\" ] || exit 0\n\
             IFS='\n'\nset -f\nexec {} {} $NAUTILUS_SCRIPT_SELECTED_FILE_PATHS\n",
            sh_quote(gui),
            a.flag
        );
        write(&nautilus_file(dirs, a.label), &script, true)?;

        let nemo = format!(
            "# {MARK}\n[Nemo Action]\nName={} with HatchZip\nIcon-Name=archive-extract\n\
             Exec={exec} {} %F\nSelection=notnone\nExtensions={};\n",
            a.label,
            a.flag,
            EXTENSIONS.join(";")
        );
        write(&nemo_file(dirs, a.id), &nemo, false)?;
    }

    Ok(vec![
        format!("Dolphin: {}", kde_file(dirs).display()),
        format!(
            "GNOME Files: {}",
            dirs.data.join("nautilus/scripts").display()
        ),
        format!("Nemo: {}", dirs.data.join("nemo/actions").display()),
        format!("Thunar: {}", thunar_file(dirs).display()),
    ])
}

pub fn unregister(dirs: &Dirs) -> Result<()> {
    let mut paths = vec![kde_file(dirs)];
    for a in ACTIONS {
        paths.push(nautilus_file(dirs, a.label));
        paths.push(nemo_file(dirs, a.id));
    }
    for p in paths {
        match fs::remove_file(&p) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", p.display()));
            }
            _ => {}
        }
    }
    thunar_unregister(dirs)
}

pub fn is_registered(dirs: &Dirs) -> bool {
    kde_file(dirs).exists()
}
