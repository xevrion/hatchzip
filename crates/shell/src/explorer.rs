//! What the Explorer command handler in `crates/shell-ext` and its
//! registration share: the handler's class ID, its DLL's file name, and the
//! text of the "Extract to" entry, which names the folder the GUI creates.
//! Nothing here touches the system, so it builds and is tested everywhere.

use crate::ACTIONS;

/// The handler's COM class ID, `{CB3BCA83-08A0-4CC7-8219-C514FAC7D51C}`.
/// Never change it: unregistering finds what an older version registered by
/// it. Written without digit separators, which clippy wants in groups of
/// equal size and a GUID doesn't have.
pub const HANDLER_CLSID: u128 = 0xCB3BCA8308A04CC78219C514FAC7D51C;

/// The handler's file, installed next to `hatchzip-gui.exe`.
pub const HANDLER_DLL: &str = "hatchzip_shell_ext.dll";

/// The entry's text for several archives at once. Each goes to a folder of
/// its own, named as for one.
pub const SEVERAL: &str = "Extract each to its own folder";

/// Names longer than this, in characters, are cut in the middle. Explorer
/// sizes the whole menu to its widest entry.
const MAX_NAME: usize = 50;

/// [`HANDLER_CLSID`] the way the registry writes it, braces included.
pub fn clsid_string() -> String {
    let v = HANDLER_CLSID;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:04X}-{:012X}}}",
        (v >> 96) as u32,
        (v >> 80) as u16,
        (v >> 64) as u16,
        (v >> 48) as u16,
        (v as u64) & 0xFFFF_FFFF_FFFF,
    )
}

/// The "Extract to" entry's text. `count` is how many items are selected
/// and `first` the file name of the first, if it could be read. One archive
/// gets `Extract to "game\"`; with nothing readable, the entry keeps the
/// label it has without the handler.
pub fn extract_to_title(count: usize, first: Option<&str>) -> String {
    match (count, first) {
        (1, Some(name)) => format!(
            "Extract to \"{}\\\"",
            menu_text(&shorten(&folder_name(name)))
        ),
        (2.., _) => SEVERAL.to_string(),
        _ => static_label().to_string(),
    }
}

/// The label the registry gives the entry, which the handler replaces.
pub fn static_label() -> &'static str {
    ACTIONS
        .iter()
        .find(|a| a.id == "extract-to")
        .map_or("Extract to a new folder", |a| a.label)
}

/// The name of the folder "Extract to" creates next to the archive called
/// `file_name`. It follows `hatchzip_core::session::locate` and then
/// `default_output`, which the GUI uses, by name alone: the handler runs
/// inside Explorer, and the core crate would bring every decoder into it.
/// The `tests/folder_name.rs` suite checks that the two agree.
pub fn folder_name(file_name: &str) -> String {
    // `game.r00` is a later volume of `game.rar`, and the GUI opens the set
    // from its first volume. The core checks that `game.rar` exists; a menu
    // label can assume it, since the set can't be extracted without it.
    let lower = file_name.to_ascii_lowercase();
    if let Some((stem, ext)) = lower.rsplit_once('.')
        && is_old_rar_volume(ext)
    {
        return default_name(&format!("{}.rar", &file_name[..stem.len()]));
    }
    default_name(file_name)
}

/// `r00` to `z99`, the extensions of an old style RAR set after the first.
fn is_old_rar_volume(ext: &str) -> bool {
    let b = ext.as_bytes();
    b.len() == 3 && (b'r'..=b'z').contains(&b[0]) && b[1..].iter().all(u8::is_ascii_digit)
}

/// `default_output`'s file name, for a first volume called `file_name`.
fn default_name(file_name: &str) -> String {
    let mut name = file_name.to_string();
    if let Some((stem, digits)) = name.rsplit_once('.')
        && digits.len() >= 3
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        name = stem.to_string();
    }
    let lower = name.to_ascii_lowercase();
    if let Some(stem) = lower.strip_suffix(".rar")
        && let Some((_, digits)) = stem.rsplit_once(".part")
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        name.truncate(stem.len() - digits.len() - ".part".len());
        return name;
    }
    for ext in [".zip", ".7z", ".tar", ".tar.gz", ".tgz", ".rar", ".exe"] {
        if lower.ends_with(ext) && name.len() > ext.len() {
            name.truncate(name.len() - ext.len());
            break;
        }
    }
    if name == file_name {
        name.push_str("-extracted");
    }
    name
}

/// Keeps the start and the end of a long name, where versions and part
/// numbers usually are.
fn shorten(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= MAX_NAME {
        return name.to_string();
    }
    let head = 32;
    let tail = MAX_NAME - head - 3;
    let mut short: String = chars[..head].iter().collect();
    short.push_str("...");
    short.extend(&chars[chars.len() - tail..]);
    short
}

/// Menus take `&` as the mark of a keyboard shortcut, so a literal one is
/// doubled.
fn menu_text(s: &str) -> String {
    s.replace('&', "&&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_archive_names_its_folder() {
        let one = |n: &str| extract_to_title(1, Some(n));
        assert_eq!(one("game.zip"), r#"Extract to "game\""#);
        assert_eq!(one("game.tar.gz"), r#"Extract to "game\""#);
        assert_eq!(one("Game.TGZ"), r#"Extract to "Game\""#);
        assert_eq!(one("game.7z.001"), r#"Extract to "game\""#);
        assert_eq!(one("game.part01.rar"), r#"Extract to "game\""#);
        assert_eq!(one("game.r00"), r#"Extract to "game\""#);
        assert_eq!(one("notes.gz"), r#"Extract to "notes.gz-extracted\""#);
    }

    #[test]
    fn several_or_unreadable_selections_get_a_general_label() {
        assert_eq!(extract_to_title(2, Some("a.zip")), SEVERAL);
        assert_eq!(extract_to_title(40, None), SEVERAL);
        assert_eq!(extract_to_title(1, None), "Extract to a new folder");
        assert_eq!(extract_to_title(0, None), "Extract to a new folder");
    }

    #[test]
    fn ampersands_stay_visible() {
        assert_eq!(
            extract_to_title(1, Some("Tom & Jerry.zip")),
            r#"Extract to "Tom && Jerry\""#
        );
    }

    #[test]
    fn long_names_are_cut_in_the_middle() {
        let name = format!("{}-v2.zip", "a".repeat(80));
        let title = extract_to_title(1, Some(&name));
        let folder = format!("{}...{}-v2", "a".repeat(32), "a".repeat(12));
        assert_eq!(title, format!("Extract to \"{folder}\\\""));
        assert_eq!(folder.chars().count(), MAX_NAME);
        // Counted in characters, so a name in any script is cut cleanly.
        let wide = format!("{}.7z", "\u{4e2d}".repeat(60));
        assert_eq!(shorten(&folder_name(&wide)).chars().count(), MAX_NAME);
        assert_eq!(shorten("short"), "short");
    }

    #[test]
    fn old_style_rar_volumes_are_recognised_by_extension() {
        assert!(is_old_rar_volume("r00"));
        assert!(is_old_rar_volume("z99"));
        assert!(!is_old_rar_volume("rar"));
        assert!(!is_old_rar_volume("q00"));
        assert!(!is_old_rar_volume("r0"));
    }

    #[test]
    fn the_class_id_is_written_like_the_registry_writes_it() {
        assert_eq!(clsid_string(), "{CB3BCA83-08A0-4CC7-8219-C514FAC7D51C}");
    }
}
