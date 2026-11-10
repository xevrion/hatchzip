//! Which bundled translation the app shows.
//!
//! Slint picks one by itself, but from the first preferred language only, and
//! by the language part alone, so Traditional Chinese (`zh-TW`, `zh-Hant-*`)
//! would get the Simplified catalog. This walks the whole list the system
//! reports, in order, and keeps the scripts apart.

/// The folders under `translations/`. `tests/catalog.rs` checks that they
/// match.
pub const LANGUAGES: &[&str] = &["es", "hi", "zh_CN"];

/// The translation for a list of preferred locales, most preferred first, as
/// the system reports them (`zh-Hans-CN`, `zh-CN`, `zh_CN.UTF-8`, `hi-IN`).
/// `""` is English: the first language with a translation wins, and English
/// ahead of any of them keeps the app in English.
pub fn pick(preferred: impl IntoIterator<Item = String>, available: &[&str]) -> String {
    for locale in preferred {
        let locale = locale.to_ascii_lowercase().replace('_', "-");
        let locale = locale.split(['.', '@']).next().unwrap_or_default();
        let mut parts = locale.split('-');
        let language = parts.next().unwrap_or_default();
        if language == "en" {
            return String::new();
        }
        if language == "zh" {
            let rest: Vec<&str> = parts.collect();
            let traditional = rest.contains(&"hant")
                || (!rest.contains(&"hans")
                    && rest.iter().any(|p| matches!(*p, "tw" | "hk" | "mo")));
            let wanted = if traditional { "zh_TW" } else { "zh_CN" };
            if available.contains(&wanted) {
                return wanted.to_string();
            }
            // Never Simplified for a Traditional reader: try the next one.
            continue;
        }
        if let Some(found) = available
            .iter()
            .find(|a| a.split(['_', '-']).next() == Some(language))
        {
            return (*found).to_string();
        }
    }
    String::new()
}

/// Whether windows pick the language themselves. The app turns it on before
/// its first window; tests leave it off and select a language directly.
static FROM_SYSTEM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Lets the windows built from now on pick the system's language.
pub fn follow_system() {
    FROM_SYSTEM.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Selects the translation for the system's preferred languages, when the
/// app asked for that with [`follow_system`]. Called right after a window is
/// created, before any text is filled in.
pub fn apply() {
    if !FROM_SYSTEM.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let language = pick(sys_locale::get_locales(), LANGUAGES);
    // Every name in LANGUAGES is bundled, so this only fails if the catalog
    // list and the folders disagree, which tests/catalog.rs catches.
    let _ = slint::select_bundled_translation(&language);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first(locales: &[&str]) -> String {
        pick(locales.iter().map(|l| (*l).to_string()), LANGUAGES)
    }

    #[test]
    fn simplified_chinese_is_found_however_it_is_spelled() {
        for l in [
            "zh-Hans-CN",
            "zh-Hans",
            "zh-CN",
            "zh-SG",
            "zh",
            "zh_CN.UTF-8",
        ] {
            assert_eq!(first(&[l]), "zh_CN", "{l}");
        }
    }

    #[test]
    fn traditional_chinese_never_gets_simplified() {
        for l in [
            "zh-Hant-TW",
            "zh-Hant",
            "zh-TW",
            "zh-HK",
            "zh-MO",
            "zh_TW.UTF-8",
        ] {
            assert_eq!(first(&[l]), "", "{l}");
        }
        assert_eq!(first(&["zh-TW", "hi-IN"]), "hi");
    }

    #[test]
    fn every_translation_folder_is_listed() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("translations");
        let mut folders: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        folders.sort();
        assert_eq!(folders, LANGUAGES);
    }

    #[test]
    fn the_whole_list_is_read_in_order() {
        assert_eq!(first(&["fr-FR", "zh-Hans-CN"]), "zh_CN");
        assert_eq!(first(&["en-US", "zh-CN"]), "");
        assert_eq!(first(&["hi-IN"]), "hi");
        assert_eq!(first(&["es-MX"]), "es");
        assert_eq!(first(&["fr-FR"]), "");
        assert_eq!(first(&[]), "");
    }
}
