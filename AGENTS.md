# Project coding standards

HatchZip extracts archives in place: it extracts an archive's units (ZIP entries, 7z solid blocks, tar entries) from the last to the first and truncates the archive behind itself, so the user only needs free space for the output. The engine is the library `crates/core`: `archive/` has one file per format plus `storage.rs`, the only code that shrinks archive bytes; `engine.rs` plans and runs the extraction for every format; `journal.rs` holds the crash-safety state; `session.rs` is what front ends call; `platform/` everything that differs per OS. `crates/cli` is the `hatchzip` command and the end to end tests. Read [docs/how-it-works.md](docs/how-it-works.md) before touching the engine, the journal or storage.

## Communication

- Be succinct. Prefer code over prose.
- Do not summarise your changes afterwards unless asked.
- State what you did not verify. An honest gap costs less than a confident wrong claim.

## Correctness

- The tool destroys its input. The invariant is absolute: the journal never claims a file before it is durable, and the archive is never truncated past an entry the journal doesn't claim.
- Anything that can be refused is refused before the archive is touched.
- Every number in the README and docs is measured on real hardware, never estimated. Paste the measurement when you change one.
- A behavior is not finished until it ran for real: a real archive extracted, the output compared byte for byte, the crash tests passing.
- Never run `extract` on an archive you can't recreate. It deletes it.

## Operating systems

- Everything that differs between systems lives in `crates/core/src/platform/`, one file per system (`linux.rs`, `macos.rs`, `windows.rs`, shared Unix code in `unix.rs`) providing the same items.
- No `cfg(target_os)`, `cfg(windows)` or `cfg!(windows)` outside that module. `cfg(debug_assertions)` for test knobs is fine.
- CI runs the tests on Linux, macOS and Windows, since nobody develops on the latter two.

## Rust

- Rust 1.95, pinned in `rust-toolchain.toml`. MSRV 1.89 for `File::try_lock`.
- `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt --all` formatted.
- `anyhow` for errors, with context on anything touching the filesystem. Errors tell the user what to do next.
- Comments explain why, not what, and read like a person wrote them.

## Traps already fallen into

- The zip crate scans backwards over the whole archive looking for the end record. `storage::Reader` must return zeros, not an error, for regions already truncated away, or every resume fails with "No CDFH found". Tar relies on the same zeros to stop at the truncation point.
- Byte-range locks are mandatory on Windows: locking the archive blocked our own reader threads ("another process has locked a portion of the file"). `platform::open_exclusive` uses the share mode there instead.
- Linux flushes a batch with one `syncfs`, so per-folder bugs only show on macOS and Windows. Folders of link entries don't exist until the end; don't touch them per batch.
- Don't hardcode unit counts in tests. The 7z writer gives some empty files a block of their own, which shifted every count by one; assert invariants (volumes past `committed_len` are gone) instead.
- 7-Zip's default settings can put a whole archive in one solid block, which in-place extraction can't free until the end. Test 7z with `-ms=<size>` archives as well as defaults.
- On resume, files a crashed batch wrote but never committed still occupy space. They are deleted before the space check, or the check refuses a run that fits.

## Translations

The desktop app is translated; `crates/gui/translations/README.md` has the details.

- Every user-facing string in `crates/gui` goes through `@tr()` in a `.slint` file. Text the Rust side needs is a Slint function wrapping one `@tr` (`ui.invoke_...()`), never English built with `format!` and shown as is.
- One whole sentence per message. Don't join translated pieces with spaces or punctuation, and don't run translated text through `sentence()`, which adds a Latin `.`. Counted text uses Slint's plural form, `@tr("{} file" | "{} files" % n, n)`.
- A change that adds or changes a `@tr` message updates every catalog in `crates/gui/translations/` in the same change, drafted from that catalog's existing terms. `cargo test -p hatchzip-gui --test catalog` lists anything missing or wrong; keep it passing.
- A new language is a folder plus its name in `LANGUAGES` (`crates/gui/src/locale.rs`).
- The command line stays English. So does `hatchzip-core`'s error text, which the CLI prints; the app translates what it shows through typed values, not by matching English.

## Copy

- README and docs are clear, neutral, professional English, sentence case headings, no first person.
- No em dashes and no unicode arrows anywhere: code, comments, commits, docs, CLI output.

## Commands

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
cargo run -p hatchzip -- check some.zip
```
