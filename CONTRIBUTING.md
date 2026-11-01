# Contributing to HatchZip

Thank you for your interest in contributing. HatchZip deletes people's archives while it works, so every change is judged first on whether it can lose data, and only then on whether it is faster or nicer.

## Table of contents

- [Before you start](#before-you-start)
- [Contribution terms](#contribution-terms)
- [Development setup](#development-setup)
- [Project structure](#project-structure)
- [Engineering standards](#engineering-standards)
- [Documentation style](#documentation-style)
- [Pull requests](#pull-requests)
- [Reporting bugs](#reporting-bugs)
- [Code of conduct](#code-of-conduct)

## Before you start

- **Small, self-evident fixes** (typos, obvious bugs) can be submitted directly as a pull request.
- **New formats, and any change to the journal, the batch order or truncation,** start with an issue describing the approach, so the crash-safety argument is settled before code is written.
- **Testing on systems CI doesn't cover** is as valuable as code: other filesystems (exFAT and FAT32 drives, ReFS, ZFS, network shares), very large archives, slow USB disks.

## Contribution terms

HatchZip is dual licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at the user's option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in HatchZip by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

## Development setup

You need [Rust](https://rustup.rs). The version is pinned in `rust-toolchain.toml` and installs itself on the first build.

```sh
git clone https://github.com/xevrion/hatchzip.git
cd hatchzip
git config core.hooksPath .githooks
```

| Command | Description |
|---|---|
| `cargo run -p hatchzip -- check <archive>` | Show the space plan for an archive |
| `cargo run -p hatchzip -- extract <archive> -o <dir>` | Extract from source. **This deletes the archive.** Work on a copy. |
| `cargo test` | Run all tests |
| `cargo clippy --all-targets -- -D warnings` | Lint; warnings are errors |
| `cargo fmt --all` | Format |
| `cargo deny check`, and again with `cargo deny --features rar check` | Advisories, licenses, sources and the network ban list in `deny.toml`, as CI runs them. Needs [cargo-deny](https://github.com/EmbarkStudios/cargo-deny). |

### Git hooks

| Hook | Checks |
|---|---|
| `pre-commit` | `cargo fmt --check` |
| `pre-push` | Formatting, clippy and the test suite, matching CI |

### Test knobs

Debug builds read these environment variables. Release builds ignore them.

| Variable | Effect |
|---|---|
| `HATCHZIP_BATCH_ENTRIES=<n>` | Cap batches at `n` entries, to exercise many commits on a small archive |
| `HATCHZIP_CRASH_AT=after-extract` or `after-journal` | Abort the process at that point of a batch |
| `HATCHZIP_CRASH_AT=restore-appended` or `restore-written` | Abort `restore` after it appended the new index, or after it also rewrote the 7z start header |
| `HATCHZIP_CRASH_AT=stream-after-sync`, `stream-after-journal` or `stream-after-punch` | Abort at that step of a resume point while a large file is streamed |
| `HATCHZIP_CRASH_AFTER=<n>` | Let that point pass `n` times before aborting |
| `HATCHZIP_STREAM_STEP=<bytes>` | Make a resume point every `<bytes>` of output instead of 256 MiB, and stream every file of at least twice that, even when it would fit whole |
| `HATCHZIP_NO_PUNCH=1` | Act as if the filesystem couldn't punch holes, as on FAT32 and exFAT |
| `HATCHZIP_IGNORE_CASE=1` | Act as if no folder were known to keep apart names that differ only in case |

### Testing against a nearly full disk

On Linux, an unprivileged user can create a tiny private filesystem to watch HatchZip work where `unzip` fails:

```sh
mkdir -p /tmp/hz && unshare -rm sh -c '
  mount -t tmpfs -o size=220m tmpfs /tmp/hz &&
  cp big.zip /tmp/hz/ &&
  ./target/release/hatchzip extract /tmp/hz/big.zip --keep-free 5MB -y'
```

## Project structure

The workspace has two crates: `crates/core` (`hatchzip-core`, the engine as a library) and `crates/cli` (`hatchzip`, the command). Front ends only talk to the engine through `session` and `engine`.

| Path | Contents |
|---|---|
| `crates/core/src/session.rs` | Opening an archive fresh or half extracted, the space plan, starting a run; shared by every front end |
| `crates/core/src/restore.rs` | Turning a half-extracted archive back into a normal one, and undoing an interrupted restore |
| `crates/core/src/engine.rs` | Format-independent planning, batch extraction, commit and truncation |
| `crates/core/src/journal.rs` | The journal and the saved tail |
| `crates/core/src/archive/mod.rs` | The `Format`, `Worker` and `Sink` interface every format implements, format detection, path safety |
| `crates/core/src/archive/storage.rs` | One file or a split set, the reader that serves the saved tail, and the only code that shrinks or deletes archive bytes |
| `crates/core/src/archive/{zip,sevenz,tar}.rs` | One file per format: list its units, decode one |
| `crates/core/src/archive/targz.rs` | `.tar.gz`: one unit decoded front to back, the tar parser and its resume points |
| `crates/core/src/archive/resumable.rs` | Decoders that restart mid-stream: stored data, raw deflate, gzip members |
| `crates/core/src/platform/` | Everything that differs between Linux, macOS and Windows |
| `crates/core/src/fsutil.rs` | Free space, atomic writes, timestamps |
| `crates/cli/src/cli.rs` | Command line definition, help text and exit codes; also the source of the completions and man page |
| `crates/cli/src/main.rs` | Commands, the space report, confirmation and progress display |
| `crates/cli/tests/extract.rs` | ZIP end to end tests, including the crash tests |
| `crates/cli/tests/formats.rs` | 7z, tar and split archives through the same checks |
| `crates/cli/tests/streaming.rs`, `crates/cli/tests/targz.rs` | Freeing space inside a large file and inside a `.tar.gz`, killed at every step of a resume point |
| `crates/cli/tests/cli.rs` | Exit codes, quiet mode, listing, completions and man page |
| `crates/cli/tests/restore.rs` | `restore` for every format, checked by independent readers and the real `unzip`, `7z` and `tar` |
| `docs/` | [How it works](docs/how-it-works.md), [format notes](docs/formats.md) |

### Adding a format

A format is one file in `crates/core/src/archive/` that parses the archive into units (each with the offset it starts at and the files it holds) and decodes one unit on request. The engine does everything else: batching, the space check, the journal, crash safety, truncation. A new format needs its own section in `crates/cli/tests/formats.rs`, including `crash_and_resume`, and the archives it is tested with should come from the format's own tools as well as from a Rust writer.

## Engineering standards

### Never lose a file

- The order in [How it works](docs/how-it-works.md#one-batch) is the core invariant: the journal never claims a file before it is durable, and the archive is never truncated past an entry the journal doesn't claim.
- A change that touches extraction, the journal or truncation must keep `resumes_after_a_crash_at_any_point` passing, and must add a crash point if it introduces a new step.
- Anything that can be refused should be refused before the archive is touched, never halfway through.

### Measured claims

Every figure in the README and docs was measured on real hardware. Changes that affect speed must include before and after numbers in the pull request, with the hardware. "Not measured" is an acceptable entry; an estimate is not.

### Operating systems

- Everything that differs between systems lives in `crates/core/src/platform/`, one file per system providing the same items. No `cfg(target_os)` or `cfg(windows)` outside it.
- CI runs the full test suite on Linux, macOS and Windows.

### Rust

- `cargo clippy --all-targets -- -D warnings` clean and `cargo fmt --all` formatted.
- `anyhow` for errors, with context on anything touching the filesystem. Error messages tell the user what happened and what to do next.
- Comments explain why, not what.

## Documentation style

- Clear, neutral, professional English, sentence case headings, no first person in the README and docs.
- No em dashes and no unicode arrows anywhere: code, comments, commit messages, docs.

## Pull requests

- Commit messages and pull request titles follow [Conventional Commits](https://www.conventionalcommits.org): `fix(journal): ...`, `feat: ...`, `docs: ...`. Pull requests are squash merged, so the title becomes the commit on `main`.
- One logical change per pull request.
- Fill in the pull request template, including how the change was verified.

## Reporting bugs

Use the [bug report form](https://github.com/xevrion/hatchzip/issues/new/choose). If an extraction failed midway, keep the `.hatchzip` and `.hatchzip-tail` files next to the archive: they are needed to resume, and they help diagnose what happened.

## Code of conduct

This project follows its [Code of Conduct](CODE_OF_CONDUCT.md).
