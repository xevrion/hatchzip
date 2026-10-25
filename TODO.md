# TODO

Ticked when it is merged and tested, not when it is started.

## Launch: before announcing anywhere

- [x] ZIP, 7z, tar and split archive extraction, freeing space as it goes
- [x] Crash-safe resume, tested by killing the process at every commit point
- [x] Space check before touching anything, with a kept-free margin
- [x] CLI: help with examples, shell completions, man page, exit codes, `--quiet`, `list`, `test`
- [x] MIT OR Apache-2.0, repository public
- [x] CI green on Linux, macOS and Windows (the full suite, crash tests included, on real Windows and macOS runners)
- [x] Ctrl+C message tells the truth: it stops mid-file and redoes that batch on resume
- [x] Extract into a folder that already has other files ("Extract here"), never overwriting one
- [x] `hatchzip restore`: turn a half-extracted archive back into a normal ZIP, 7z or tar any tool can open (tested with `unzip`, `7z` and GNU `tar`, including interrupted restores)
- [ ] Decide whether `--verify-first` becomes the default
- [x] Release workflow: static binaries for Windows x64, macOS arm64 and x64, Linux x64 and arm64, smoke tested, with SHA-256 checksums (dry run passed on all five)
- [x] First release: v0.1.0 on GitHub Releases, 11 downloads with checksums; the one-line installer tested against it
- [x] One-line install: `install.sh` for Linux and macOS (tested locally, including a tampered checksum and uninstall), `install.ps1` for Windows
- [ ] Test `install.ps1` on a real Windows machine against a published release
- [x] Archive over 4 GB with 64-bit offsets, including a crash and resume past 4 GiB (sparse test archive)
- [ ] Real run on a big real-world archive and on a real Windows machine
- [ ] Demo GIF at the top of the README: `unzip` fails on a nearly full disk, HatchZip finishes
- [x] Publish to crates.io so `cargo install hatchzip` works (`hatchzip` and `hatchzip-core` 0.1.0; installed from crates.io in a clean folder)
- [ ] Open a handful of `good first issue` issues from the lists below

## Requested after launch

- [x] `.tar.gz`, for datasets
- [ ] `.tar.zst` and `.tar.xz` (see Formats)
- [ ] RAR, single files and volume sets (`.part1.rar`, `.rar` with `.r00`, ...) (implemented, awaiting merge: freed file by file, volumes deleted as they empty, a big stored file freed while it is written; solid RAR only with `--keep-archive`; behind the `rar` cargo feature, on in the release builds; encrypted file names (`-hp`) with the password, but no `restore` for them; no links; see Formats)
- [ ] Native split ZIP (`.z01`, ...) (see Formats)
- [ ] Check the volumes against an `.sfv` file next to the archive before starting, and stop if one doesn't match
- [ ] Extract several archives one after another from one command or one right-click, checking the space for each (implemented, awaiting merge: `extract`, `check`, `test` and `list` take several archives, `extract` stops at the first that does not complete with a summary and its exit code; the right-click task window queues several on Linux, where Dolphin, GNOME Files and Nemo now hand over the whole selection. Explorer still starts one window per file, so queueing there needs a single-instance app)
- [ ] Android app (the engine is a library; Android's storage access rules are the open question)
- [ ] The other direction: compress in place when there is no room for a copy. A folder: delete each file once its compressed copy is safely on disk. One huge file: free its start with hole punching as it is compressed, with resume points like extraction has
- [ ] Keep the archive when there is room: an option (CLI, GUI, right-click) to extract normally without removing anything, and say so in the space check (implemented, awaiting merge: `--keep-archive` on `extract` and `check`, "Remove the archive during extraction" in the app, "Extract here (keep archive)" in every right-click menu; resumable, journal version 4; tested for ZIP, 7z, tar, `.tar.gz` and split sets, with crashes)
- [ ] Opening a huge archive must never sit silent: show progress while it is read and checked (a user saw "checking disk space" for 20 minutes). In review: a progress bar in the CLI and "Reading archive… N%" in the GUI while opening reads the archive; still to do: the GUI reads a `.tar.gz` in full twice before extracting (once to list it, once more under "Checking disk space"), which could reuse the first pass
- [ ] Nested archives: offer to extract the archives found inside, in place too
- [ ] Extract while downloading: read the archive from a URL and never store it (for formats that can be read front to back)
- [ ] Windows Defender quarantining a file mid-run: read each file back before its bytes are cut (see Safety and polish)
- [ ] Packages: winget, Homebrew, AUR (`pacman -S hatchzip`), Gentoo GURU, Flatpak; Steam Deck notes (the AppImage runs in desktop mode)
- [ ] File-manager menus: Windows 11 top-level menu, COSMIC Files, Thunar
- [ ] Antivirus false positives: signed Windows builds, a notarized macOS app (unsigned builds get flagged by VirusTotal engines and moved to the Trash by macOS), and an FAQ until then
- [ ] Android (several requests); iOS is out of reach for in-place work
- [ ] `.dmg` disk images (requested): read-only UDIF images (zlib, bzip2, LZFSE, LZMA chunks) holding HFS+ or APFS; extracting the files means reading the file system inside, so first check whether a Rust reader exists

## GUI, like WinRAR and 7-Zip

- [x] Split the code into a workspace: `core` library, `cli`, `shell`, `gui`
- [x] Choose the GUI framework: Slint (native, no web view, software renderer when there is no GPU)
- [x] Open an archive and browse its contents (a flat list with sizes; a folder tree is still to do)
- [x] Show the space plan before extracting: fits or not, how much is needed, free after
- [x] Task windows for right-click jobs with progress, speed, time left, pause and resume
- [x] Detect half-extracted archives and offer to continue
- [x] Drag and drop archives onto the window (Windows, macOS, X11; Wayland has no file drops in winit yet)
- [x] Settings in the window: kept-free space, verify first, output folder
- [x] Remember settings between runs (keep-free and verify-first, in the per-OS config folder)
- [x] Password dialog, asking again on a wrong password
- [x] "Restore archive" on half-extracted and paused archives
- [x] App icon in the window, and embedded in the Windows exe with a version resource
- [ ] Contents as a folder tree
- [x] Light and dark theme following the system
- [x] Falls back to the software renderer when OpenGL won't start (tested with a broken EGL on Linux)
- [ ] Run and click through the GUI on real Windows and macOS
- [ ] Optional file association: open `.zip`, `.7z` and `.tar` in HatchZip (the installers already offer HatchZip under "Open with"; on macOS the app still has to handle the open-documents Apple Event)

## Right-click menu and OS integration

- [x] Windows Explorer: HatchZip submenu with "Extract here", "Extract to a new folder" and "Test", per user without admin rights (`crates/shell`; registry keys tested in CI, not yet clicked in a real Explorer)
- [x] Linux: Dolphin service menu, GNOME Files scripts and Nemo actions (`crates/shell`; files tested, not yet clicked)
- [x] Wire `crates/shell` into the GUI: `--register-shell` / `--unregister-shell` (no window, for installers) and a switch in the Settings menu (AppImage registers `$APPIMAGE`)
- [ ] Thunar custom actions
- [ ] Windows 11 top-level context menu (the classic one sits under "Show more options"); needs a packaged shell extension
- [ ] macOS: Finder Quick Action (needs a Mac to test)
- [x] App icon: `assets/icon/` (SVG, PNGs 16 to 1024, `.ico`, `.icns`, hicolor set), regenerated by `assets/icon/build.py`; also the site favicon
- [x] Linux desktop entry and AppStream metadata (`packaging/linux/`, pass `desktop-file-validate` and `appstreamcli validate`)
- [x] Windows setup `HatchZip-x64-setup.exe` (Inno Setup, per user, PATH, `hz.exe`, Start menu, "Open with", right-click menu via `--register-shell`): installed, checked and uninstalled on a real Windows runner in the release dry run
- [x] macOS `HatchZip-<arch>.dmg` (ad hoc signed, "Open With" for zip, 7z and tar, CLI and man page inside): built for both architectures; the arm64 one opened, verified and run on a real Mac runner
- [x] Linux `.deb`, `.rpm` (CLI, `hz`, app, man pages, completions, menu entry, icons) and AppImage: installed, run and removed in clean containers in the release dry run
- [ ] Click through the installed apps by hand on real Windows and macOS desktops
- [ ] winget, Homebrew, AUR, Flatpak
- [ ] Code signing for Windows SmartScreen and macOS notarization (paid certificates)

## Formats

- [x] ZIP, ZIP64, self-extracting `.exe` ZIPs
- [x] 7z, freed one solid block at a time
- [x] Uncompressed tar, including long names, symlinks and hard links
- [x] Split archives (`.001`, `.002`, ...), volumes deleted as they empty
- [x] Free space inside one large file while extracting it (hole punching plus durable resume points): stored and Deflate ZIP entries, tar entries; space really freed on ext4, APFS and NTFS (CI requires it)
- [ ] The same inside a 7z solid block (LZMA2 resume points); fixes 7z made with 7-Zip's default settings
- [x] `.tar.gz` and `.tgz`, freeing the compressed bytes behind the decoder, with resume points at deflate block and gzip member boundaries; space really freed on ext4, APFS and NTFS (CI)
- [ ] `.tar.xz`: every xz block start is a free resume point (xz 5.8 writes 24 MiB blocks by default); a single-block file needs the LZMA state saved. See `docs/formats.md`
- [ ] `.tar.zst`: easy for multi-frame files (`pzstd`), but the usual single frame needs a decoder that can save its window. See `docs/formats.md`
- [ ] RAR (implemented, awaiting merge: per file, not per volume, with the crash tests at every commit for RAR 5 and RAR 4 sets and single files; RAR 5 passwords proven by the check value, RAR 4 by decoding the smallest encrypted file; `restore`; fixtures from RAR 6.24 including real RAR 4; tests pass on Linux, CI runs them on macOS and Windows; x86_64 musl release build linked and run locally, aarch64 linked locally but only run in CI)
- [ ] RAR: symbolic links and hard links, and solid archives in place, which needs a decoder whose state can be saved (UnRAR's can't)
- [ ] RAR encrypted file names (`-hp`) (implemented, awaiting merge: headers decrypted in Rust, RAR 5 with AES-256 and the check value of the archive encryption header, RAR 4 with AES-128 and RAR 3's SHA-1 key derivation; fixtures from RAR 6.24, single and multi-volume, crash tests at every commit; Windows and macOS only in CI)
- [ ] RAR: `restore` for encrypted file names. It needs a password (`restore` takes none today) to list what is left and to encrypt the new end of archive header
- [ ] RAR: run the aarch64 musl release build on Arm once (the `linux-arm` release job does it on every release)
- [ ] RAR: with the first release that has it, the manifests that repackage release builds (AUR `hatchzip-bin`, the Homebrew formula and cask, Scoop, winget) should install `LICENSE-UNRAR` and name its license; `hatchzip-bin` also needs `libstdc++`, which the desktop app now links. The source packages (AUR `hatchzip`, Homebrew core) build without the `rar` feature, yet the `.desktop` file offers RAR to them too
- [ ] Native split ZIP (`.z01`, `.z02`, ..., `.zip`)
- [x] Password-protected ZIP (ZipCrypto, AES) and 7z (AES, encrypted headers too), proven before anything is deleted, never saved (CLI; tested including a real `zip -P` archive through a terminal prompt)
- [x] Codec tests with archives from 7-Zip 25.01: Deflate64, bzip2, LZMA, PPMd in ZIP; LZMA, BCJ+LZMA2, bzip2, Deflate, PPMd in 7z
- [x] Archives over 4 GiB: entries past the 4 GiB mark, crashed and resumed there (sparse test archive, Linux and macOS)
- [ ] Tests for XZ in ZIP and Zstandard in 7z (7-Zip itself can't write those)

## Website

- [x] Plain static page in the spirit of 7-zip.org: downloads, what it does, measured results, limits (`site/`, about 11 KB, no JavaScript, strict CSP)
- [ ] Pick and buy the domain, then fill in the canonical URL, sitemap and Open Graph URL
- [ ] Deploy (GitHub Pages workflow is ready and manual-only; Cloudflare Pages also works)
- [ ] Screenshots of the GUI once it exists

## Safety and polish

- [ ] `--skip-damaged`: save what can be saved as `name.damaged` and keep going
- [ ] Read each file back before cutting its bytes from the archive, to catch antivirus quarantine and bad writes
- [x] Windows: reserved names (`CON`, `NUL`, ...), trailing dots and spaces, and forbidden characters are refused before anything is touched
- [x] Paths over 260 characters extract and resume (test runs on Windows CI too)
- [ ] Refuse links where other entries need a folder or that others would be written through (#10), and duplicate ZIP names (#11), before anything is touched; a run already stuck making its links says which and why, and finishes once the way is clear. In review
- [ ] Benchmarks on more hardware, including a hard drive and a USB stick
- [ ] macOS CI flake: once in a few runs, `archives_from_tar_gzip_and_pigz_extract_and_resume` finds nothing freed after a punch on a fresh `tar -czf` archive (passes on rerun). Find whether `F_PUNCHHOLE` refused (which silently turns freeing off for the rest of the run) or APFS reported the space late, and log it either way
