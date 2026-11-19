//! Dolphin (KDE) service menu, GNOME Files scripts, Nemo actions and Thunar
//! custom actions. Thunar keeps every custom action in one file the user
//! edits too, so only HatchZip's own `<action>` blocks are ever touched there.

use std::fs;
use std::io;
use std::ops::Range;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

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

const THUNAR_ID_PREFIX: &str = "hatchzip-";
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

/// Where the parts HatchZip edits sit in a Thunar actions file.
struct Uca {
    /// The root's start tag.
    root_open: Range<usize>,
    /// `<actions/>`, with no end tag to add to.
    self_closing: bool,
    /// Where `</actions>` starts.
    root_close: usize,
    /// Each element directly under the root named `action`, start to end, and
    /// whether its own `<unique-id>` child names it as HatchZip's.
    actions: Vec<(Range<usize>, bool)>,
}

/// Reads just enough XML to know where the root and its actions are. Comments,
/// processing instructions, CDATA and the doctype are skipped, so a
/// `</actions>` inside a comment is not mistaken for the real one. `None` for
/// anything that isn't a well formed `<actions>` document, since writing into
/// a broken file would only break it further.
fn parse_uca(xml: &str) -> Option<Uca> {
    let mut stack: Vec<(&str, usize, usize)> = Vec::new();
    let mut root: Option<(Range<usize>, bool)> = None;
    let mut root_close = None;
    let mut actions = Vec::new();
    let mut ours = false;
    let mut i = 0;
    while let Some(off) = xml[i..].find('<') {
        let at = i + off;
        // Outside the root only whitespace is allowed.
        if stack.is_empty() && !xml[i..at].trim().is_empty() {
            return None;
        }
        // Thunar's parser refuses a bare `&`, and would then drop the file.
        if !entities_ok(&xml[i..at]) {
            return None;
        }
        let rest = &xml[at..];
        // Markup that holds no elements.
        if rest.starts_with("<!--") {
            i = at + rest.find("-->")? + 3;
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            if stack.is_empty() {
                return None;
            }
            i = at + rest.find("]]>")? + 3;
            continue;
        }
        if rest.starts_with("<?") {
            i = at + rest.find("?>")? + 2;
            continue;
        }
        if rest.starts_with("<!DOCTYPE") {
            if root.is_some() {
                return None;
            }
            i = at + doctype_len(rest)?;
            continue;
        }
        // An element's start or end tag.
        let end = at + tag_len(rest)?;
        let tag = &xml[at + 1..end - 1];
        if !entities_ok(tag) {
            return None;
        }
        if let Some(name) = tag.strip_prefix('/') {
            let (open, start, content) = stack.pop()?;
            if open != name.trim_end() {
                return None;
            }
            match stack.len() {
                0 => root_close = Some(at),
                1 if open == "action" => actions.push((start..end, ours)),
                // Only the action's own id counts, not text elsewhere in it.
                2 if open == "unique-id" && stack[1].0 == "action" => {
                    ours = xml[content..at].trim().starts_with(THUNAR_ID_PREFIX);
                }
                _ => {}
            }
        } else {
            let self_closing = tag.ends_with('/');
            let name = tag
                .trim_end_matches('/')
                .split(|c: char| c.is_whitespace())
                .next()
                .unwrap_or("");
            if name.is_empty() {
                return None;
            }
            if stack.is_empty() {
                if root.is_some() || name != "actions" {
                    return None;
                }
                root = Some((at..end, self_closing));
                if self_closing {
                    root_close = Some(end);
                }
            }
            if stack.len() == 1 && name == "action" {
                ours = false;
            }
            if !self_closing {
                stack.push((name, at, end));
            } else if stack.len() == 1 && name == "action" {
                actions.push((at..end, false));
            }
        }
        i = end;
    }
    if !stack.is_empty() || !xml[i..].trim().is_empty() {
        return None;
    }
    let (root_open, self_closing) = root?;
    Some(Uca {
        root_open,
        self_closing,
        root_close: root_close?,
        actions,
    })
}

/// Whether every `&` starts a reference like `&amp;` or `&#38;`.
fn entities_ok(text: &str) -> bool {
    text.split('&').skip(1).all(|after| {
        let Some(semi) = after.find(';') else {
            return false;
        };
        let name = &after[..semi];
        match name.strip_prefix('#') {
            Some(n) => match n.strip_prefix('x') {
                Some(h) => !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()),
                None => !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()),
            },
            None => {
                name.chars()
                    .next()
                    .is_some_and(|c| c.is_alphabetic() || c == '_')
                    && name
                        .chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
            }
        }
    })
}

/// Length of a tag up to and including its `>`, which may sit in a quoted
/// attribute value.
fn tag_len(rest: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in rest.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(i + 1),
            (None, '<') if i > 0 => return None,
            _ => {}
        }
    }
    None
}

/// Length of `<!DOCTYPE ...>`, whose internal subset holds `>` of its own.
fn doctype_len(rest: &str) -> Option<usize> {
    let gt = rest.find('>')?;
    match rest.find('[') {
        Some(lb) if lb < gt => {
            let rb = lb + rest[lb..].find(']')?;
            Some(rb + rest[rb..].find('>')? + 1)
        }
        _ => Some(gt + 1),
    }
}

/// The file with HatchZip's `<action>` blocks taken out, together with the
/// whitespace they sat on. Everything else stays byte for byte, including
/// actions the user wrote and whatever Thunar added when it saved the file.
fn thunar_without_ours(xml: &str, uca: &Uca) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut from = 0;
    for (r, _) in uca.actions.iter().filter(|(_, ours)| *ours) {
        let line = xml[..r.start].rfind('\n').map_or(0, |i| i + 1).max(from);
        let indent_only = xml[line..r.start].trim().is_empty();
        out.push_str(&xml[from..if indent_only { line } else { r.start }]);
        from = r.end;
        if indent_only && xml[from..].starts_with('\n') {
            from += 1;
        }
    }
    out.push_str(&xml[from..]);
    out
}

fn not_an_actions_file(dirs: &Dirs) -> anyhow::Error {
    anyhow!(
        "{} is not a Thunar actions file HatchZip can edit; it was left as it is. \
         Fix it or move it away, then try again",
        thunar_file(dirs).display()
    )
}

/// Left by a register that created the user's `uca.xml`, so unregister knows
/// the file is HatchZip's to remove. Its content alone can't tell: the user
/// may keep an empty file on purpose, to hide the system's actions.
fn thunar_created_mark(dirs: &Dirs) -> PathBuf {
    dirs.config.join("hatchzip/thunar-uca-created")
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
    let (current, own) = thunar_current(dirs)?;
    let uca = parse_uca(&current).ok_or_else(|| not_an_actions_file(dirs))?;
    let current_had_ours = uca.actions.iter().any(|(_, ours)| *ours);
    let mut xml = thunar_without_ours(&current, &uca);
    let uca = parse_uca(&xml).ok_or_else(|| not_an_actions_file(dirs))?;
    let ours = thunar_actions(gui);
    if uca.self_closing {
        let open = &xml[uca.root_open.clone()];
        let open = open[..open.len() - 1].trim_end_matches('/').trim_end();
        let replaced = format!("{open}>\n{ours}</actions>");
        xml.replace_range(uca.root_open, &replaced);
    } else {
        xml.insert_str(uca.root_close, &ours);
    }
    write_atomic(&thunar_file(dirs), &xml)?;
    let mark = thunar_created_mark(dirs);
    if !own {
        write(&mark, "", false)?;
    } else if !current_had_ours {
        // A file HatchZip made would still hold its actions, so a mark next to
        // one without them is left from a file the user deleted since.
        remove_mark(&mark)?;
    }
    Ok(())
}

fn thunar_unregister(dirs: &Dirs) -> Result<()> {
    let (current, own) = thunar_current(dirs)?;
    let mark = thunar_created_mark(dirs);
    if !own || !current.contains(THUNAR_ID_PREFIX) {
        return remove_mark(&mark);
    }
    let uca = parse_uca(&current).ok_or_else(|| not_an_actions_file(dirs))?;
    if !uca.actions.iter().any(|(_, ours)| *ours) {
        return remove_mark(&mark);
    }
    let left = thunar_without_ours(&current, &uca);
    // A file HatchZip started from nothing goes away with its last action,
    // so Thunar falls back to what it read before.
    if left == THUNAR_EMPTY && mark.exists() {
        fs::remove_file(thunar_file(dirs))
            .with_context(|| format!("removing {}", thunar_file(dirs).display()))?;
    } else {
        write_atomic(&thunar_file(dirs), &left)?;
    }
    remove_mark(&mark)
}

fn remove_mark(mark: &Path) -> Result<()> {
    match fs::remove_file(mark) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", mark.display()))
        }
        _ => Ok(()),
    }
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
