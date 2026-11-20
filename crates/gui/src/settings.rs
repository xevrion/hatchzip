//! The few choices remembered between runs. A missing or broken file is
//! never an error: the app starts with the defaults and overwrites it on the
//! next change.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::platform;

pub const DEFAULT_KEEP_FREE: u64 = 2_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub keep_free: u64,
    pub verify_first: bool,
    /// Extract without removing the archive, in the main window and from
    /// every right-click entry. Off, the right-click menu still has an entry
    /// that keeps it.
    pub keep_archive: bool,
    /// Close the right-click window once everything in it is extracted, as
    /// other extractors do. A test, a pause or a failure keeps it open.
    pub close_when_done: bool,
    /// The language picked in Settings > Language: `en` or a folder name in
    /// `translations/`. `None` follows the system.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            keep_free: DEFAULT_KEEP_FREE,
            verify_first: false,
            keep_archive: false,
            close_when_done: true,
            language: None,
        }
    }
}

/// The real settings file. Tests use their own, so they never touch it.
pub fn file() -> Option<PathBuf> {
    Some(platform::config_dir()?.join("settings.json"))
}

impl Settings {
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
        };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), s);
        assert!(!path.with_extension("json.tmp").exists());
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
                keep_free: DEFAULT_KEEP_FREE,
                verify_first: true,
                keep_archive: false,
                close_when_done: true,
                language: None,
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
    }
}
