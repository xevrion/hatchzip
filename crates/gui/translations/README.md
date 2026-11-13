# GUI translations

The desktop app is translated with Slint's `@tr()` and gettext catalogs. Each language is one file, `<language>/LC_MESSAGES/hatchzip-gui.po`, bundled into the app at build time, so there is nothing to install next to it.

| Language | Folder |
|---|---|
| Español | `es` |
| हिन्दी (Hindi) | `hi` |
| 简体中文 (Simplified Chinese) | `zh_CN` |

## How the language is chosen

`src/locale.rs` reads the system's preferred languages in order and takes the first one with a catalog; anything without one falls back to the English source text. English ahead of a translated language keeps the app in English. Chinese is matched by script: `zh-CN`, `zh-Hans`, `zh-SG` and a bare `zh` get `zh_CN`, while `zh-TW`, `zh-HK` and `zh-Hant` never get Simplified Chinese (they move on to the next preferred language until a `zh_TW` catalog exists).

## Adding a language

1. Copy `es/LC_MESSAGES/hatchzip-gui.po` to `<language>/LC_MESSAGES/hatchzip-gui.po`. Use the language code (`fr`, `de`, `ja`), or language and region when the script or spelling differs (`pt_BR`, `zh_TW`).
2. In the header, set `Language:` to the folder name and `Plural-Forms:` to your language's rule. The [gettext plural forms](https://www.gnu.org/software/gettext/manual/html_node/Plural-forms.html) list has them; Chinese, Japanese and Korean use `nplurals=1; plural=0;`.
3. Translate every `msgstr`. Leave each `msgid` exactly as it is.
4. Add the folder name to `LANGUAGES` in `src/locale.rs`, and a row to the table above.
5. Run `cargo test -p hatchzip-gui --test catalog`. It lists every entry that needs fixing.
6. Open a pull request. Screenshots of the main window in your language are welcome.

## Rules a catalog must follow

`tests/catalog.rs` checks every folder, so CI catches these:

- Every message marked `@tr` in `ui/*.slint` has a translation, and the catalog has nothing the source no longer marks. `GB` is the one exception; it stays English on purpose.
- Placeholders stay: a translation uses the same arguments as its source. It may reorder them with numbers, `{1} / {0}` for `{} of {}`.
- Plural entries have one `msgstr[n]` per form your `Plural-Forms` header names. The last form uses all of the plural's placeholders; earlier forms may leave the number out ("one file").
- No translation is empty or marked `#, fuzzy`.
- Leading and trailing spaces match the source. A message starting with a space is a sentence that follows another; keep the space.

## Changing or adding messages

A pull request that adds or changes a `@tr` message updates every catalog in the same change. The test fails otherwise. Draft the new entries for each language from its existing terms, and list the new messages in the pull request so native speakers can check them.

## What is translated

The main window and its shared controls are translated. Still English for now: the separate task window that the right-click menu opens, the About window, and messages that come from the extraction engine (errors and the disk-space verdict), which are being made translatable step by step.
