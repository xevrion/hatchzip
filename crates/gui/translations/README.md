# GUI translations

Slint bundles the `.po` files under `<language>/LC_MESSAGES/hatchzip-gui.po` into the application at build time. The language directory is a locale such as `hi`; Slint selects the system locale, including a language-only match for regional locales, and falls back to the English source text where a translation is missing.

The catalog uses no message context. Keep placeholders such as `{}` in translations, and keep the English source strings unchanged unless the corresponding UI text changes. `tests/translations.rs` exercises the bundled Hindi and Simplified Chinese catalogs and English fallback. Counted messages use Slint's plural `@tr` syntax and the catalog's `msgid_plural`/`msgstr[n]` entries; keep every plural form translated. `tests/catalog.rs` checks both singular and plural messages against the marked source text, and checks that the Simplified Chinese catalog covers all marked messages. Other catalogs may fall back to English for newly marked messages.

The main window, task window, space-plan details and About window use the bundled catalogs. The two task views share presentation code and messages. Errors from the core, decoders and operating system are preserved as technical details so that paths and recovery instructions remain available. These diagnostics and third-party Slint branding text are not translated by this catalog.
