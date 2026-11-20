//! The few choices remembered between runs. A missing or broken file is
//! never an error: the app starts with the defaults and overwrites it on the
//! next change.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::platform;

pub const DEFAULT_KEEP_FREE: u64 = 2_000_000_000;

/// Light or dark, for every window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

/// What happens once an extraction completes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfterExtracting {
    #[default]
    Nothing,
    OpenFolder,
}

/// Where an archive is extracted unless the person picks a folder for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    /// In a folder named after the archive, next to it.
    #[default]
    NextToArchive,
    /// In a folder named after the archive, inside `destination_folder`.
    Folder,
}

/// What to do with files already at the paths an extraction writes, as the
/// command line's `--replace` and `--skip-existing` choose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Existing {
    /// Show the prompt for each file.
    #[default]
    Ask,
    Replace,
    Skip,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    #[serde(deserialize_with = "keep_free_or_default")]
    pub keep_free: u64,
    #[serde(deserialize_with = "or_default")]
    pub verify_first: bool,
    /// Extract without removing the archive, in the main window and from
    /// every right-click entry. Off, the right-click menu still has an entry
    /// that keeps it.
    #[serde(deserialize_with = "or_default")]
    pub keep_archive: bool,
    /// Close the right-click window once everything in it is extracted, as
    /// other extractors do. A test, a pause or a failure keeps it open.
    #[serde(deserialize_with = "true_or_default")]
    pub close_when_done: bool,
    /// The language picked in Settings: `en` or a folder name in
    /// `translations/`. `None` follows the system.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "or_default"
    )]
    pub language: Option<String>,
    #[serde(deserialize_with = "or_default")]
    pub appearance: Appearance,
    #[serde(deserialize_with = "or_default")]
    pub after_extracting: AfterExtracting,
    #[serde(deserialize_with = "or_default")]
    pub destination: Destination,
    /// Kept when `destination` goes back to next to the archive, so picking
    /// a folder again starts from the last one.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "or_default"
    )]
    pub destination_folder: Option<PathBuf>,
    #[serde(deserialize_with = "or_default")]
    pub existing_files: Existing,
}

/// A value this version can't read, such as a choice added by a newer one,
/// falls back on its own instead of costing every other setting.
fn or_else<'de, D: Deserializer<'de>, T: DeserializeOwned>(
    d: D,
    fallback: impl FnOnce() -> T,
) -> std::result::Result<T, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(T::deserialize(value).unwrap_or_else(|_| fallback()))
}

fn or_default<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(
    d: D,
) -> std::result::Result<T, D::Error> {
    or_else(d, T::default)
}

fn keep_free_or_default<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
    or_else(d, || DEFAULT_KEEP_FREE)
}

fn true_or_default<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<bool, D::Error> {
    or_else(d, || true)
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            keep_free: DEFAULT_KEEP_FREE,
            verify_first: false,
            keep_archive: false,
            close_when_done: true,
            language: None,
            appearance: Appearance::System,
            after_extracting: AfterExtracting::Nothing,
            destination: Destination::NextToArchive,
            destination_folder: None,
            existing_files: Existing::Ask,
        }
    }
}

/// The real settings file. Tests use their own, so they never touch it.
pub fn file() -> Option<PathBuf> {
    Some(platform::config_dir()?.join("settings.json"))
}

impl Settings {
    /// The folder archives go into, each in a folder named after it, or
    /// `None` for next to the archive. A folder that is gone, or on a drive
    /// that was unplugged, counts as none: extracting would make the path
    /// again on whatever disk holds its parent, which is not where the
    /// person asked for.
    pub fn default_folder(&self) -> Option<&Path> {
        match self.destination {
            Destination::Folder => self.destination_folder.as_deref().filter(|d| d.is_dir()),
            Destination::NextToArchive => None,
        }
    }

    /// Where an archive goes whose own default, a folder named after it
    /// next to it, is `next_to`.
    pub fn output_for(&self, next_to: &Path) -> PathBuf {
        match (self.default_folder(), next_to.file_name()) {
            (Some(dir), Some(name)) => dir.join(name),
            _ => next_to.to_path_buf(),
        }
    }

    /// The saved settings, or the defaults.
    pub fn saved() -> Self {
        file().map(|f| Self::load_from(&f)).unwrap_or_default()
    }

    /// Reads `path`. It is a few bytes, read once before the first window
    /// exists, so this is the one disk read on the UI thread.
    pub fn load_from(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        // Written aside and renamed over, so a crash never leaves half a file.
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(self)?;
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    }

    /// Saves to `path` on a background thread. Saves are queued in order, so
    /// the last change always wins.
    pub fn save(&self, path: &Path) {
        type Job = (Settings, PathBuf);
        static SAVER: OnceLock<Sender<Job>> = OnceLock::new();
        let saver = SAVER.get_or_init(|| {
            let (tx, rx) = channel::<Job>();
            std::thread::spawn(move || {
                while let Ok(mut job) = rx.recv() {
                    // Only the newest of a burst of edits is worth writing.
                    while let Ok(newer) = rx.try_recv() {
                        job = newer;
                    }
                    if let Err(e) = job.0.save_to(&job.1) {
                        eprintln!("hatchzip-gui: could not save settings: {e:#}");
                    }
                }
            });
            tx
        });
        let _ = saver.send((self.clone(), path.to_path_buf()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep/hatchzip/settings.json");
        let s = Settings {
            keep_free: 500_000_000,
            verify_first: true,
            keep_archive: true,
            close_when_done: false,
            language: Some("zh_CN".into()),
            appearance: Appearance::Dark,
            after_extracting: AfterExtracting::OpenFolder,
            destination: Destination::Folder,
            destination_folder: Some(dir.path().join("Extracted")),
            existing_files: Existing::Skip,
        };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), s);
        assert!(!path.with_extension("json.tmp").exists());
        let json = std::fs::read_to_string(&path).unwrap();
        for written in [
            r#""appearance": "dark""#,
            r#""after_extracting": "open_folder""#,
            r#""destination": "folder""#,
            r#""existing_files": "skip""#,
        ] {
            assert!(json.contains(written), "{written} in {json}");
        }
        // Every other choice survives the trip too.
        for (appearance, after, existing) in [
            (
                Appearance::Light,
                AfterExtracting::Nothing,
                Existing::Replace,
            ),
            (Appearance::System, AfterExtracting::Nothing, Existing::Ask),
        ] {
            let s = Settings {
                appearance,
                after_extracting: after,
                existing_files: existing,
                ..Settings::default()
            };
            s.save_to(&path).unwrap();
            assert_eq!(Settings::load_from(&path), s);
        }
    }

    #[test]
    fn the_defaults_write_no_folder_or_language() {
        // Absent and `null` mean the same, so the file only names a folder
        // or a language someone chose.
        let json = serde_json::to_string(&Settings::default()).unwrap();
        assert!(!json.contains("destination_folder"), "{json}");
        assert!(!json.contains("language"), "{json}");
    }

    #[test]
    fn unreadable_values_fall_back_one_by_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // Choices from a newer version, and values of the wrong type: each
        // gets its default, and the rest of the file still counts.
        std::fs::write(
            &path,
            br#"{
                "keep_free": "lots",
                "verify_first": true,
                "keep_archive": 3,
                "close_when_done": null,
                "language": 7,
                "appearance": "sepia",
                "after_extracting": "open_both",
                "destination": "cloud",
                "destination_folder": 12,
                "existing_files": "rename"
            }"#,
        )
        .unwrap();
        assert_eq!(
            Settings::load_from(&path),
            Settings {
                verify_first: true,
                ..Settings::default()
            }
        );
        std::fs::write(
            &path,
            br#"{"appearance": "light", "existing_files": "replace", "keep_free": 0}"#,
        )
        .unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s.appearance, Appearance::Light);
        assert_eq!(s.existing_files, Existing::Replace);
        assert_eq!(s.keep_free, 0);
    }

    #[test]
    fn the_default_destination_names_a_folder_after_the_archive() {
        let next_to = Path::new("/home/me/Downloads/game");
        let dir = tempfile::tempdir().unwrap();
        let mut s = Settings::default();
        assert_eq!(s.output_for(next_to), next_to);
        // A folder chosen once and then turned off is remembered, not used.
        s.destination_folder = Some(dir.path().to_path_buf());
        assert_eq!(s.default_folder(), None);
        assert_eq!(s.output_for(next_to), next_to);
        s.destination = Destination::Folder;
        assert_eq!(s.output_for(next_to), dir.path().join("game"));
        // Always in a folder, with none chosen yet: next to the archive.
        s.destination_folder = None;
        assert_eq!(s.output_for(next_to), next_to);
    }

    #[test]
    fn a_default_folder_that_is_gone_falls_back_to_next_to_the_archive() {
        let next_to = Path::new("/home/me/Downloads/game");
        let dir = tempfile::tempdir().unwrap();
        let s = Settings {
            destination: Destination::Folder,
            destination_folder: Some(dir.path().to_path_buf()),
            ..Settings::default()
        };
        assert_eq!(s.output_for(next_to), dir.path().join("game"));
        drop(dir);
        assert_eq!(s.default_folder(), None);
        assert_eq!(s.output_for(next_to), next_to);
        // The choice itself is kept, for when the drive comes back.
        assert_eq!(s.destination, Destination::Folder);
        assert!(s.destination_folder.is_some());
    }

    #[test]
    fn missing_or_broken_files_give_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(Settings::load_from(&path), Settings::default());
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
        // A file from another version keeps what it has in common.
        std::fs::write(&path, br#"{"verify_first": true, "theme": "dark"}"#).unwrap();
        assert_eq!(
            Settings::load_from(&path),
            Settings {
                verify_first: true,
                ..Settings::default()
            }
        );
        // Files saved before keeping the archive was a choice remove it, as
        // that version always did. Closing when done is new, so it is on.
        std::fs::write(&path, br#"{"keep_free": 0, "verify_first": false}"#).unwrap();
        assert!(!Settings::load_from(&path).keep_archive);
        assert!(Settings::load_from(&path).close_when_done);
        assert_eq!(Settings::load_from(&path).language, None);
        std::fs::write(&path, br#"{"language": "hi"}"#).unwrap();
        assert_eq!(Settings::load_from(&path).language.as_deref(), Some("hi"));
        // A file from before the Settings window has none of its choices;
        // each starts at the default, which is how the app behaved then.
        std::fs::write(
            &path,
            br#"{"keep_free": 500000000, "verify_first": true, "keep_archive": true, "close_when_done": false, "language": "es"}"#,
        )
        .unwrap();
        let old = Settings::load_from(&path);
        assert_eq!(
            old,
            Settings {
                keep_free: 500_000_000,
                verify_first: true,
                keep_archive: true,
                close_when_done: false,
                language: Some("es".into()),
                ..Settings::default()
            }
        );
        assert_eq!(old.appearance, Appearance::System);
        assert_eq!(old.after_extracting, AfterExtracting::Nothing);
        assert_eq!(old.destination, Destination::NextToArchive);
        assert_eq!(old.existing_files, Existing::Ask);
    }
}
