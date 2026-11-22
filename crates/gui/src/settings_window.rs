//! What the Settings window shows for each choice in [`Settings`], and back.
//! The window's lists are indexed in the order of `settings-window.slint`;
//! the main window wires its callbacks, since every change also reaches the
//! main window's own controls.

use slint::language::ColorScheme;
use slint::{ModelRc, SharedString, VecModel};

use crate::settings::{AfterExtracting, Appearance, Destination, Existing, Settings};
use crate::text::gb_text;
use crate::{FileManager, SettingsWindow, locale, platform};

/// The palette's scheme for a choice. `Unknown` lets the style follow the
/// system again, light or dark, as it does when nothing was ever set.
pub fn color_scheme(appearance: Appearance) -> ColorScheme {
    match appearance {
        Appearance::System => ColorScheme::Unknown,
        Appearance::Light => ColorScheme::Light,
        Appearance::Dark => ColorScheme::Dark,
    }
}

pub fn appearance(index: i32) -> Appearance {
    match index {
        1 => Appearance::Light,
        2 => Appearance::Dark,
        _ => Appearance::System,
    }
}

fn appearance_index(a: Appearance) -> i32 {
    match a {
        Appearance::System => 0,
        Appearance::Light => 1,
        Appearance::Dark => 2,
    }
}

pub fn after_extracting(index: i32) -> AfterExtracting {
    match index {
        1 => AfterExtracting::OpenFolder,
        _ => AfterExtracting::Nothing,
    }
}

fn after_index(a: AfterExtracting) -> i32 {
    match a {
        AfterExtracting::Nothing => 0,
        AfterExtracting::OpenFolder => 1,
    }
}

pub fn destination(index: i32) -> Destination {
    match index {
        1 => Destination::Folder,
        _ => Destination::NextToArchive,
    }
}

fn destination_index(d: Destination) -> i32 {
    match d {
        Destination::NextToArchive => 0,
        Destination::Folder => 1,
    }
}

pub fn existing(index: i32) -> Existing {
    match index {
        1 => Existing::Replace,
        2 => Existing::Skip,
        _ => Existing::Ask,
    }
}

fn existing_index(e: Existing) -> i32 {
    match e {
        Existing::Ask => 0,
        Existing::Replace => 1,
        Existing::Skip => 2,
    }
}

/// The language code for an entry of the list: `""` for "Use the system
/// language", which comes first, then the codes of [`locale::NAMES`].
pub fn language_code(index: i32) -> &'static str {
    usize::try_from(index)
        .ok()
        .and_then(|i| i.checked_sub(1))
        .and_then(|i| locale::NAMES.get(i))
        .map_or("", |(code, _)| *code)
}

fn language_index(saved: Option<&str>) -> i32 {
    locale::chosen(saved)
        .and_then(|chosen| locale::NAMES.iter().position(|(code, _)| *code == chosen))
        .map_or(0, |i| i32::try_from(i + 1).unwrap_or(0))
}

/// Built again after every language change: the first entry is worded in
/// the language showing, the names never are.
pub fn show_languages(w: &SettingsWindow, saved: Option<&str>) {
    let mut names = vec![w.invoke_system_language()];
    names.extend(
        locale::NAMES
            .iter()
            .map(|(_, name)| SharedString::from(*name)),
    );
    w.set_language_names(ModelRc::new(VecModel::from(names)));
    w.set_language_index(language_index(saved));
}

/// Everything but the reserve's text and the language list, which would
/// undo typing or reopen the list if set on every change.
pub fn show_choices(w: &SettingsWindow, s: &Settings) {
    w.set_appearance_index(appearance_index(s.appearance));
    w.set_after_index(after_index(s.after_extracting));
    w.set_close_when_done(s.close_when_done);
    w.set_remove_archive(!s.keep_archive);
    w.set_verify_first(s.verify_first);
    w.set_destination_index(destination_index(s.destination));
    w.set_destination_folder(
        s.destination_folder
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_default()
            .into(),
    );
    w.set_existing_index(existing_index(s.existing_files));
}

/// Fills a new window, or one whose settings were all reset.
pub fn show_all(w: &SettingsWindow, s: &Settings) {
    w.set_shell_menu_available(platform::SHELL_MENU);
    w.set_file_manager(match platform::SHELL_MENU_NAME {
        platform::ShellMenuName::Explorer => FileManager::Explorer,
        platform::ShellMenuName::Finder => FileManager::Finder,
        platform::ShellMenuName::Files => FileManager::Files,
    });
    show_languages(w, s.language.as_deref());
    show_choices(w, s);
    w.set_keep_free(gb_text(s.keep_free).into());
    w.set_keep_free_valid(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_list_entry_maps_to_its_choice_and_back() {
        for a in [Appearance::System, Appearance::Light, Appearance::Dark] {
            assert_eq!(appearance(appearance_index(a)), a);
        }
        for a in [AfterExtracting::Nothing, AfterExtracting::OpenFolder] {
            assert_eq!(after_extracting(after_index(a)), a);
        }
        for d in [Destination::NextToArchive, Destination::Folder] {
            assert_eq!(destination(destination_index(d)), d);
        }
        for e in [Existing::Ask, Existing::Replace, Existing::Skip] {
            assert_eq!(existing(existing_index(e)), e);
        }
        // Anything out of range is the default, never a panic.
        assert_eq!(appearance(-1), Appearance::System);
        assert_eq!(existing(9), Existing::Ask);
    }

    #[test]
    fn the_language_list_starts_with_the_system() {
        assert_eq!(language_code(0), "");
        assert_eq!(language_code(-1), "");
        assert_eq!(language_index(None), 0);
        assert_eq!(language_index(Some("fr")), 0, "unknown follows the system");
        for (i, (code, _)) in locale::NAMES.iter().enumerate() {
            let index = i32::try_from(i + 1).unwrap();
            assert_eq!(language_code(index), *code);
            assert_eq!(language_index(Some(code)), index);
        }
        assert_eq!(language_code(99), "");
    }
}
