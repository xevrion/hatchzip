<h1 align="center">HatchZip</h1>

<p align="center">
  Extract huge archives in place. The archive shrinks as your files hatch,<br>
  so you only need free space for the output, not for the output <em>and</em> the archive.
</p>

<h4 align="center">
  <a href="#installation">Installation</a> |
  <a href="#usage">Usage</a> |
  <a href="#is-it-safe">Safety</a> |
  <a href="docs/how-it-works.md">How it works</a> |
  <a href="CONTRIBUTING.md">Contributing</a>
</h4>

<p align="center">
  <a href="https://github.com/xevrion/hatchzip/actions/workflows/ci.yml"><img src="https://github.com/xevrion/hatchzip/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg" alt="MIT or Apache-2.0 licensed" /></a>
  <img src="https://img.shields.io/badge/platform-linux%20%7C%20windows%20%7C%20macos-blue.svg" alt="Linux, Windows and macOS" />
</p>

<p align="center">
  <img src="docs/media/hatchzip.gif" width="720" alt="HatchZip extracts the last part of the archive, then cuts it off the archive file, so the disk never holds the archive and all the files at once" />
</p>

A 180 GB game download that unpacks to 200 GB needs 380 GB of disk with WinRAR, 7-Zip or `unzip`, because the archive and everything extracted from it have to exist at the same time. HatchZip needs a little over 200 GB. It extracts the archive from the end and cuts each finished part off the archive file as it goes, so the space the archive gives back is reused for the next files.

It checks that the extraction will fit before it changes anything, keeps a safety margin of free space the whole way through, and can be stopped or crash at any moment and pick up where it left off. It reads ZIP, 7z, tar, `.tar.gz`, RAR volume sets and split archives.

### Get it

No coding needed. **[Download for Windows](https://github.com/xevrion/hatchzip/releases/latest/download/HatchZip-x64-setup.exe)** (Windows 10 and 11), or for [macOS (Apple Silicon)](https://github.com/xevrion/hatchzip/releases/latest/download/HatchZip-aarch64.dmg), [macOS (Intel)](https://github.com/xevrion/hatchzip/releases/latest/download/HatchZip-x86_64.dmg) and [Linux](#installation).

1. Run the installer. Windows may show "Windows protected your PC" because HatchZip is not code-signed yet: choose **More info**, then **Run anyway**.
2. Right-click the archive, choose **HatchZip**, then **Extract here**. On Windows 11 the menu is under **Show more options**.
3. Or open HatchZip from the Start menu, open the archive, and choose **Extract**.

HatchZip shows whether the files will fit before it changes anything. The archive is used up as the files come out, so when it finishes, only the extracted files are left.

## Highlights

The same 200 MB of game data (21 files), packed by the real tools, extracted on a 220 MiB disk with about 50 MB free:

| Archive | `unzip` / 7-Zip | HatchZip |
|---|---|---|
| ZIP, 180 MB | `unzip` ran out of space at file 6 of 21 | Finished, every file byte-identical |
| 7z from 7-Zip 25.01, `-ms=20m` (10 solid blocks) | Needs 380 MB | Finished in 0.64 s, every file byte-identical |
| 7z split into 5 volumes of 40 MB | Needs 380 MB | Finished; volumes deleted one at a time, peak 210 MB used |
| tar from GNU tar, 200 MB | Needs 400 MB | Finished in 0.12 s, every file byte-identical |
| 7z from 7-Zip with default settings (1 solid block) | Needs 380 MB | Refused before touching anything: one solid block can't be freed until all of it is out, so it needs 200 MB free |

| | Result |
|---|---|
| ZIP extraction speed, same data | 0.25 to 0.35 s, against 1.48 to 1.50 s for `unzip` 6.00, including an fsync `unzip` skips |
| Crash safety | Killed at both unsafe moments of a batch at many points of each format's run; every resume finished with identical output |
| Works on | Any filesystem that can shrink a file: ext4, btrfs, XFS, NTFS, APFS, and also exFAT and FAT32 USB drives |

Measured on Fedora 43, Intel Core i7-14700HX, SK Hynix NVMe SSD, warm page cache; the disk-space runs used a 220 MiB tmpfs to stand in for a full laptop disk, the speed runs ext4. Speed comes from decompressing files in parallel, so an archive holding one giant file extracts at the speed of a single core.

The extra space HatchZip needs beyond the output is about the size of the largest single unit it can free at once: one file for ZIP and tar, one solid block for 7z. On filesystems that can punch holes (ext4, XFS, btrfs, tmpfs; NTFS and APFS not yet verified), a large file stored or deflated in a ZIP, or in a tar, frees its archive bytes every 256 MiB while it is extracted: a ZIP holding one 600 MiB stored file extracted on a 950 MiB tmpfs with 367 MB free, where it needed 629 MB before. `hatchzip check` computes the exact figure for any archive.

## Installation

### Desktop app

Download from the [latest release](https://github.com/xevrion/hatchzip/releases/latest):

| System | File |
|---|---|
| Windows 10 and 11 (x64) | [`HatchZip-x64-setup.exe`](https://github.com/xevrion/hatchzip/releases/latest/download/HatchZip-x64-setup.exe): installs per user, no admin rights, adds the right-click menu |
| macOS, Apple Silicon | [`HatchZip-aarch64.dmg`](https://github.com/xevrion/hatchzip/releases/latest/download/HatchZip-aarch64.dmg) |
| macOS, Intel | [`HatchZip-x86_64.dmg`](https://github.com/xevrion/hatchzip/releases/latest/download/HatchZip-x86_64.dmg) |
| Debian, Ubuntu | [`hatchzip_amd64.deb`](https://github.com/xevrion/hatchzip/releases/latest/download/hatchzip_amd64.deb) |
| Fedora, openSUSE | [`hatchzip-x86_64.rpm`](https://github.com/xevrion/hatchzip/releases/latest/download/hatchzip-x86_64.rpm) |
| Any Linux | [`HatchZip-x86_64.AppImage`](https://github.com/xevrion/hatchzip/releases/latest/download/HatchZip-x86_64.AppImage) |

The installers also put the `hatchzip` command (and its short name `hz`) on your `PATH`. The Windows setup and the macOS apps are not code-signed yet: on Windows choose **More info**, then **Run anyway**; on macOS 15 and later, open the app once, then allow it under System Settings, Privacy & Security.

### Command line only

Linux and macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/xevrion/hatchzip/main/install.sh | sh
```

Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/xevrion/hatchzip/main/install.ps1 | iex
```

Both download the build for your machine from the latest release, check it against its published SHA-256 checksum, and install `hatchzip` and `hz` for your user only, with the man page and shell completions. No root or administrator rights.

### With Cargo

```sh
cargo install hatchzip
```

This builds the command line tool from [crates.io](https://crates.io/crates/hatchzip). The engine is a library too: [`hatchzip-core`](https://crates.io/crates/hatchzip-core).

### From source

With [Rust](https://rustup.rs) (the version is pinned in `rust-toolchain.toml` and installed automatically):

```sh
git clone https://github.com/xevrion/hatchzip.git
cd hatchzip
cargo build --release
```

The binaries are `target/release/hatchzip` and `target/release/hatchzip-gui` (`.exe` on Windows). Building the desktop app on Linux needs the fontconfig headers (`libfontconfig1-dev` or `fontconfig-devel`).

### Shell completions and man page

```sh
hatchzip completions bash > ~/.local/share/bash-completion/completions/hatchzip
hatchzip completions zsh > "${fpath[1]}/_hatchzip"
hatchzip completions fish > ~/.config/fish/completions/hatchzip.fish
hatchzip completions powershell >> $PROFILE

hatchzip man > ~/.local/share/man/man1/hatchzip.1    # then: man hatchzip
```

## Usage

### See whether it fits

```sh
hatchzip check game.zip
```

```
game.zip  180.2 MB zip, 200.0 MB uncompressed in 22 entries
  available                    50.5 MB
  standard extraction needs    200.0 MB
  required                     29.9 MB (+5.0 MB reserved)
  available after              30.6 MB
```

`check` reads the archive's index and the disk's free space and changes nothing.

### Extract

```sh
hatchzip extract game.zip                 # into ./game/
hatchzip extract game.zip -o D:\Games\Foo # somewhere else
hatchzip x game.7z.001                    # split archive; any part's name works
```

HatchZip prints the same space plan, asks for confirmation, then extracts. When the extraction is complete, the archive has been removed and the files are in the output folder. For a split archive, each volume is deleted as soon as everything in it is extracted.

| Option | Effect |
|---|---|
| `-o, --output <dir>` | Output folder. Defaults to a folder named after the archive, next to it. It must be empty or not exist yet. |
| `--keep-free <size>` | Free space HatchZip never uses, such as `500MB` or `10GB`. Default `2GB`, so the disk never fills to zero. |
| `--verify-first` | Read the whole archive and check every file's checksum before deleting anything. Takes extra time, recommended for downloads you can't easily get again. |
| `-y, --yes` | Skip the confirmation prompt. Required when not run from a terminal. |
| `-q, --quiet` | Print only errors and prompts. |

### Stop and resume

Press `Ctrl+C` and HatchZip finishes the files it is writing, saves its progress and exits. A second `Ctrl+C` quits immediately, which is also safe. To continue, run the same command again:

```sh
hatchzip extract game.zip
```

The same applies after a crash, a power cut, or running out of space because something else filled the disk. Progress lives in two small files next to the archive, `game.zip.hatchzip` and `game.zip.hatchzip-tail`. **Do not delete them while an extraction is unfinished**: they hold the archive's index, which is no longer inside the archive itself.

### Stop for good

To give up on an unfinished extraction instead, restore the archive:

```sh
hatchzip restore game.zip
```

What is left of the archive becomes a normal archive again, holding exactly the entries not yet extracted, which 7-Zip, WinRAR, `unzip` or `tar` open as usual. The files already extracted stay in the output folder. Restore only appends a new index, so it needs only the free space that index takes, and if it is interrupted, running it again finishes it.

### List and test

```sh
hatchzip list game.7z    # what's inside, with sizes
hatchzip test game.7z    # read every file and verify its checksum, writing nothing
```

Plain tar has no checksums for file contents, so `test` on a tar only proves every file can be read in full.

### Exit status

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Failure. If extraction had started, progress is saved. |
| 2 | Invalid command line |
| 3 | Not enough free space. Nothing was changed. |
| 4 | The archive is damaged. Nothing was deleted. |
| 130 | Paused with Ctrl+C. Run the same command to continue. |

## Is it safe

HatchZip destroys the archive as it extracts, so it is built around never losing a file that has not safely landed on disk.

- **Nothing changes until the plan fits.** The space check runs before the first byte is touched, and it replays the whole extraction to find the moment that needs the most free space, not just the total.
- **A file is written, flushed to the physical disk and renamed into place before the part of the archive holding it is cut off.** A crash at any point leaves either the file or its bytes in the archive, never neither.
- **Damage stops the run where it is.** Every file's CRC-32 is checked as it is extracted. A damaged file is not written, and it stays in the archive along with everything before it.
- **Hostile archives are refused before anything is deleted:** paths that escape the output folder, entries that overlap each other, entries that appear twice. Setuid and setgid bits are never restored, and symlinks are created only after every regular file is written, so nothing can be written through one.

What it cannot protect against: if the download itself is damaged, the files before the damaged spot are extracted and the archive is partly gone. Use `--verify-first` when the archive can't be downloaded again. On btrfs or APFS, space held by a snapshot that includes the archive is not freed until the snapshot is deleted.

## Supported formats

| Format | Status | Freed in steps of |
|---|---|---|
| ZIP, ZIP64, self-extracting `.exe` ZIPs | Supported. Tested: Stored, Deflate, Deflate64, bzip2, LZMA, PPMd and Zstandard, including archives made by 7-Zip itself, and entries past the 4 GiB mark. XZ is decoded by the same library but not yet tested here. | One file |
| 7z, solid and non-solid | Supported. Tested with archives made by 7-Zip 25.01: LZMA2 (its default), LZMA, BCJ with LZMA2, bzip2, Deflate and PPMd. Zstandard is decoded by the same library but not yet tested here. | One solid block |
| tar (uncompressed) | Supported, including long names, symlinks and hard links | One file |
| `.tar.gz`, `.tgz` | Supported, including multi-member gzip (`bgzip`, `pigz -i`) and split `.tar.gz.001` sets. Extracted from the front, freeing the compressed bytes behind the decoder where the filesystem can punch holes; every gzip CRC-32 and length is checked before anything is touched. Tested with archives from GNU tar with gzip, and pigz. Elsewhere (FAT32, exFAT) it still extracts and resumes, but needs room for the archive and every file at once, and `check` says so. | 256 MiB of output |
| Split archives (`.001`, `.002`, ...) of ZIP or 7z, as 7-Zip makes them | Supported | One file or block; each volume is deleted once emptied |
| Password-protected ZIP (ZipCrypto and AES) and 7z (AES, including encrypted file lists) | Supported. HatchZip asks for the password, or takes `--password` or `HATCHZIP_PASSWORD`, and proves it on one file before anything is deleted. It is never saved, so a resumed run asks again. | Same as unencrypted |
| `.tar.zst`, `.tar.xz` | Refused before extraction, planned; see [the format notes](docs/formats.md#tarzst-and-tarxz). | |
| One huge solid 7z block | Works, but needs free space for the whole block, as the table above shows. Freeing space inside a block is the same planned technique. | |
| RAR volume sets (`.part1.rar`, `.part2.rar`, ... or `.rar`, `.r00`, ...), RAR 5 and RAR 4, not solid | Supported, decoded by UnRAR. Password-protected files work, with the password asked for as for ZIP. Refused before anything is touched: solid archives, encrypted file names, links, and sets with a volume missing. Tested on Windows with sets made by WinRAR 7.23 (RAR 5) and, for RAR 4, volumes written by the test suite (WinRAR 7 can no longer write RAR 4). `restore` does not support RAR yet. | One volume, once every file that reaches into it is out |
| Native split ZIP (`.z01`) | Planned | |

## How it works

ZIP, 7z and tar all store their contents as a sequence of independent pieces: files for ZIP and tar, solid blocks for 7z. ZIP and 7z keep their index at the very end. HatchZip saves that index to a side file, then extracts pieces starting from the last one in the archive. After each batch it truncates the archive to where that batch began, which every operating system and filesystem supports; for a split archive, that means deleting whole volumes and shortening one. A RAR set is freed a whole volume at a time, since each volume is a complete RAR file. A `.tar.gz` is one compressed stream that can only be decoded from the front, so HatchZip extracts it front to back and punches holes in the compressed bytes it has used, at resume points it can restart from after a crash. The full design, including the exact order of writes that makes a crash safe, is in [How it works](docs/how-it-works.md).

## Star history

<a href="https://star-history.com/#xevrion/hatchzip&Date">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=xevrion/hatchzip&type=Date&theme=dark" />
    <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=xevrion/hatchzip&type=Date" />
    <img alt="Star history of xevrion/hatchzip" src="https://api.star-history.com/svg?repos=xevrion/hatchzip&type=Date" />
  </picture>
</a>

## License

HatchZip is licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.

RAR archives are decoded by the UnRAR source code, which is freeware under its own [license](LICENSE-UNRAR): free to use for extracting RAR archives, but not to build a RAR compatible archiver.

## Contributing

Bug reports, especially from filesystems and operating systems not covered by CI, are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) first.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in HatchZip by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
