# GUI translations

Slint bundles the `.po` files under `<language>/LC_MESSAGES/hatchzip-gui.po` into the application at build time. The language directory is a locale such as `hi`; Slint selects the system locale, including a language-only match for regional locales, and falls back to the English source text where a translation is missing.

The catalog uses no message context. Keep placeholders such as `{}` in translations, and keep the English source strings unchanged unless the corresponding UI text changes. `tests/translations.rs` exercises the bundled Hindi catalog and English fallback. Counted messages use Slint's plural `@tr` syntax and the catalog's `msgid_plural`/`msgstr[n]` entries; keep every plural form translated. `tests/catalog.rs` checks both singular and plural messages against the marked source text, and checks for missing catalog entries (except the deliberately untranslated `GB`).

The main window and its shared controls are being translated incrementally. Space-plan details and errors from core remain in English; the main window's hint that names the translated extraction checkbox is translated. The separate task window is not fully translated in this first change.
