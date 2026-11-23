# hatchzip-gui

The HatchZip desktop app, built with [Slint](https://slint.dev) under its Royalty-free 2.0 license. It drives the same engine as the `hatchzip` command, through `hatchzip-core`'s `session`, `engine` and `restore` modules.

```sh
cargo run -p hatchzip-gui                         # main window
cargo run -p hatchzip-gui -- game.zip             # main window with an archive open
cargo run -p hatchzip-gui -- --extract-here game.zip
cargo run -p hatchzip-gui -- --keep-archive --extract-here game.zip
cargo run -p hatchzip-gui -- --extract-to game.zip
cargo run -p hatchzip-gui -- --test game.zip
cargo run -p hatchzip-gui -- --extract-here a.zip b.7z c.tar.gz   # one after another
cargo run -p hatchzip-gui -- --register-shell     # add the right-click menu, print where, exit
cargo run -p hatchzip-gui -- --unregister-shell   # remove it, exit
```

The task forms are what file manager menus call, and installers run the two shell flags, so none of them change. `--keep-archive` goes with `--extract-here` or `--extract-to`. Each task form takes several archives, which one window works through in turn: each is checked just before it starts, and one that fails, pauses or doesn't fit holds the rest until it is resumed and done; "Restore archive" drops the rest. Dolphin, GNOME Files and Nemo hand over the whole selection; Explorer starts one window per file. The shell flags open no window. Inside an AppImage they register `$APPIMAGE`, not the temporary mount the app runs from.

## Behavior

| | |
|---|---|
| Passwords | Asked in the window when an archive needs one, and again after a wrong one. Kept in memory for this process only; a run continued later asks again. |
| Existing files | When files the archive would write already exist in the destination, the main window and the task window ask once, with the number of files, whether to replace them, before anything is touched. "Replace" extracts as `hatchzip extract --replace` does; "Cancel" stops with nothing modified. Settings, "When a file already exists" can answer for every file instead: Replace as `--replace`, Skip as `--skip-existing`. A resumed run follows its journal and does not ask again. |
| Updates | Help > "Check for updates" opens the latest release page on GitHub in the browser. The app makes no network connections itself. |
| Giving up | A paused or partially extracted archive offers "Restore archive", which runs `hatchzip_core::restore` on a worker thread. Not for an extraction that keeps its archive, which was never changed. |
| Keeping the archive | "Remove the archive during extraction", on by default. Cleared, the space plan is for every file, in bold next to what removing the archive would need, and the panel says when only removing it fits. A half-extracted archive continues the way it started. |
| Settings | A window of its own (`ui/settings-window.slint`, wired in `main_window.rs`), opened from File > Settings… or the gear, one instance. Sections: General (language, appearance, after extracting, closing the progress window), Extraction (removing the archive, verifying first, reserved space, default destination, existing files), Integration (the right-click menu, where `platform::SHELL_MENU`), Advanced (the file's path, Reset). Every change applies and is saved at once, and the main window's reserve, verify and remove controls follow it both ways. Saved to `~/.config/hatchzip/settings.json`, `%APPDATA%\HatchZip\settings.json` or `~/Library/Application Support/HatchZip/settings.json`. A missing or broken file means the defaults; a value this version can't read means the default for that one setting. The right-click menu switch is hidden on macOS, where hatchzip-shell can't add Finder entries yet. |
| Identity | App id and X11 class `io.github.xevrion.hatchzip`, matching the desktop file. On Windows `build.rs` embeds `assets/icon/hatchzip.ico` and the version into the exe. |

## Drawing

| | |
|---|---|
| Style | Picked at build time in `build.rs`: Fluent on Windows and Linux, Cupertino on macOS. `SLINT_STYLE` overrides it. |
| Renderer | FemtoVG (OpenGL) first on Linux and macOS; software renderer on Windows. `SLINT_BACKEND=winit-femtovg` uses OpenGL on Windows; `SLINT_BACKEND=winit-software` forces the software renderer anywhere. |
| Theme | Light or dark, following the system. |

Files dropped on the window open on Windows, macOS and X11. winit 0.30 doesn't report drops on Wayland, nor files opened from the Finder on macOS, which arrive as Apple Events; only the command line works there.

## Tests

`cargo test -p hatchzip-gui` runs the unit tests, `tests/cli.rs` for the shell flags, and `tests/ui.rs`, which drives the real windows headlessly through Slint's testing backend. `tests/ui_space.rs` does the same on a disk faked to have room for extracting in place but not for keeping the archive. Set `HATCHZIP_GUI_SHOTS=<dir>` to also save every state they pass through as a PNG, in light and dark.

## Screenshots

`examples/screenshots.rs` opens every window through the real window system and saves each state as a PNG: the main window (start, an open archive with its contents and the disk space plan, a plan that does not fit, running, done, failed), every Settings section and the Reset prompt, About, and the right-click window running and done. Each is drawn in light and dark and in every bundled language, at the default size, and the main and Settings windows also at their minimum size, as `<scene>-<language>-<theme>[-min].png`.

```sh
SLINT_BACKEND=winit-software cargo run --example screenshots -p hatchzip-gui -- shots
```

The `screenshots` workflow runs it on Linux (under Xvfb), macOS and Windows for every pull request that changes `crates/gui`, and on demand from the Actions tab. The images are in the run's `screenshots-<os>` artifacts, kept for 14 days. It is not a required check.
