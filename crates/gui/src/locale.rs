//! Which bundled translation the app shows.
//!
//! Slint picks one by itself, but from the first preferred language only, and
//! by the language part alone, so Traditional Chinese (`zh-TW`, `zh-Hant-*`)
//! would get the Simplified catalog. This walks the whole list the system
//! reports, in order, and keeps the scripts apart.

/// The folders under `translations/`. `every_translation_folder_is_listed`
/// below checks that they match.
pub const LANGUAGES: &[&str] = &["es", "hi", "zh_CN"];

/// Settings > Language lists these, each in its own language and script so
/// a reader finds theirs whatever the app is showing. `en` is the source
/// text; every other code is one of [`LANGUAGES`].
pub const NAMES: &[(&str, &str)] = &[
    ("en", "English"),
    ("es", "Español"),
    ("hi", "हिन्दी"),
    ("zh_CN", "简体中文"),
];

/// The saved choice, if it still names a language this build has. One from
/// a newer version, or a catalog since removed, means following the system.
pub fn chosen(saved: Option<&str>) -> Option<&str> {
    saved.filter(|code| NAMES.iter().any(|(c, _)| c == code))
}

/// The translation to show: the saved choice, else what `system` picks.
/// `""` is English, as for [`pick`].
pub fn resolve(saved: Option<&str>, system: impl FnOnce() -> String) -> String {
    match chosen(saved) {
        Some("en") => String::new(),
        Some(code) => code.to_string(),
        None => system(),
    }
}

/// The translation for a list of preferred locales, most preferred first, as
/// the system reports them (`zh-Hans-CN`, `zh-CN`, `zh_CN.UTF-8`, `hi-IN`).
/// `""` is English: the first language with a translation wins, and English
/// ahead of any of them keeps the app in English.
pub fn pick(preferred: impl IntoIterator<Item = String>, available: &[&str]) -> String {
    // Linux lists a Traditional locale with a bare `zh` after it as the
    // fallback (`LANGUAGE=zh_TW:zh`): that `zh` means the same reader.
    let mut traditional_seen = false;
    for locale in preferred {
        let locale = locale.to_ascii_lowercase().replace('_', "-");
        let locale = locale.split(['.', '@']).next().unwrap_or_default();
        let mut parts = locale.split('-');
        let language = parts.next().unwrap_or_default();
        // `C` and `POSIX` are how a Unix user asks for untranslated output.
        if matches!(language, "en" | "c" | "posix") {
            return String::new();
        }
        if language == "zh" {
            let rest: Vec<&str> = parts.collect();
            let traditional = rest.contains(&"hant")
                || (!rest.contains(&"hans")
                    && (rest.iter().any(|p| matches!(*p, "tw" | "hk" | "mo"))
                        || (rest.is_empty() && traditional_seen)));
            traditional_seen |= traditional;
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

/// Selects the saved language, or the one for the system's preferred
/// languages, when the app asked for that with [`follow_system`]. Called
/// right after a window is created, before any text is filled in.
pub fn apply(saved: Option<&str>) {
    if FROM_SYSTEM.load(std::sync::atomic::Ordering::Relaxed) {
        select(saved);
    }
}

/// Switches to `saved` now, or back to the system's language for `None`.
/// Tests never follow the system, so for them that is English.
pub fn select(saved: Option<&str>) {
    let language = resolve(saved, || {
        if FROM_SYSTEM.load(std::sync::atomic::Ordering::Relaxed) {
            pick(sys_locale::get_locales(), LANGUAGES)
        } else {
            String::new()
        }
    });
    // Every name in LANGUAGES is bundled, so this only fails if the list and
    // the folders disagree, which `every_translation_folder_is_listed` catches.
    let _ = slint::select_bundled_translation(&language);
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = language;
}

/// The language last selected, `""` for English.
static CURRENT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Sets a window's font and script settings for the language on screen.
/// Each window has its own `Theme`, so this runs for every window when it
/// is built and again when the language changes.
pub fn style(theme: crate::Theme<'_>) {
    let language = CURRENT.lock().unwrap_or_else(|e| e.into_inner()).clone();
    theme.set_font_family(crate::platform::font_family(&language).into());
    theme.set_script_boost(if language == "hi" { 1.0 } else { 0.0 });
    theme.set_menu_edge(crate::platform::IN_WINDOW_MENU);
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
        // Ubuntu's fallback chains for Taiwan and Hong Kong end in a bare `zh`.
        assert_eq!(first(&["zh-TW", "zh"]), "");
        assert_eq!(first(&["zh-HK", "zh-TW", "zh"]), "");
        assert_eq!(first(&["zh-Hant", "zh", "hi-IN"]), "hi");
        // A bare `zh` on its own is still Simplified.
        assert_eq!(first(&["zh"]), "zh_CN");
    }

    #[test]
    fn c_and_posix_mean_english() {
        assert_eq!(first(&["C", "zh-CN"]), "");
        assert_eq!(first(&["POSIX", "hi-IN"]), "");
        assert_eq!(first(&["C.UTF-8", "es-ES"]), "");
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
    fn every_language_has_a_name_in_the_menu() {
        for language in LANGUAGES {
            assert!(
                NAMES.iter().any(|(code, _)| code == language),
                "{language} is missing from NAMES"
            );
        }
        for (code, name) in NAMES {
            assert!(*code == "en" || LANGUAGES.contains(code), "{code}");
            assert!(!name.is_empty(), "{code}");
        }
        assert_eq!(NAMES.len(), LANGUAGES.len() + 1);
    }

    #[test]
    fn a_saved_language_wins_over_the_system() {
        let system = || "hi".to_string();
        assert_eq!(resolve(Some("en"), system), "");
        assert_eq!(resolve(Some("zh_CN"), system), "zh_CN");
        assert_eq!(resolve(None, system), "hi");
        // Unknown, perhaps from a newer version: follow the system.
        assert_eq!(resolve(Some("fr"), system), "hi");
        assert_eq!(resolve(Some(""), system), "hi");
        assert_eq!(chosen(Some("fr")), None);
        assert_eq!(chosen(Some("es")), Some("es"));
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
