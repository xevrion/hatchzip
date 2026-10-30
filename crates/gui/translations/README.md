# GUI translations

Slint bundles the `.po` files under `<language>/LC_MESSAGES/hatchzip-gui.po` into the application at build time. The language directory is a locale such as `hi`; Slint selects the system locale, including a language-only match for regional locales, and falls back to the English source text where a translation is missing.

The catalog uses no message context. Keep placeholders such as `{}` in translations, and keep the English source strings unchanged unless the corresponding UI text changes. `tests/translations.rs` exercises the bundled Hindi catalog and English fallback.

The main window and its shared controls are being translated incrementally. Space-plan details and errors from core remain in English. The separate task window is not fully translated in this first change.
