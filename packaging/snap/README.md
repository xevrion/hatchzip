# Snap

The `hatchzip` snap: the CLI and the desktop app, built from the tagged
source on core24, for amd64 and arm64. `snapcraft.yaml` is the whole of it.

| Command | Runs |
|---|---|
| `hatchzip` | The CLI |
| `hatchzip.hz` | The CLI under its short name. Plain `hz` needs a store alias, below |
| `hatchzip.gui` | The desktop app, also in the applications menu |

## What differs from the release builds

- **No RAR.** The `rar` feature, which compiles UnRAR's source under a
  license that is not a free software license, is left out. A RAR archive is
  refused before anything is touched, and the desktop entry drops the RAR
  types. The release downloads keep RAR support.
- **The right-click menu switch does nothing.** Settings, "Show in the file
  manager's context menu" writes its entries under `$XDG_DATA_HOME`, which
  for a snap is inside `~/snap/hatchzip/`, and points them at a path inside
  the snap. Hiding the switch when `SNAP` is set would need a change in
  `crates/gui`.
- **No man page.** Snaps don't export man pages; bash completion is
  installed through `completer`.

## Confinement

Strict, with two interfaces besides what the `gnome` extension adds to the
app:

| Interface | Gives | Connected |
|---|---|---|
| `home` | Read and write to non-hidden files in the home folder that the user owns | Automatically |
| `removable-media` | Read and write under `/media`, `/run/media` and `/mnt` | By the user: `sudo snap connect hatchzip:removable-media` |

In-place extraction needs write access where the archive is, not only to
the file a dialog returns: HatchZip truncates the archive as it extracts
and deletes it at the end, writes `<archive>.hatchzip` and
`<archive>.hatchzip-tail` next to it so an extraction can resume, reads the
other volumes of a split archive from the same folder, and creates the
output folder beside it. `home` and `removable-media` cover that for
archives in the home folder and on mounted drives. The engine's system
calls (`ftruncate`, `fallocate` for punching holes, `syncfs`, `flock`) are
all in snapd's default seccomp template.

What strict confinement leaves out: hidden folders directly in the home
folder (such as `~/.cache`), files in the home folder owned by another
user, and drives mounted anywhere other than the three folders above.
Classic confinement would lift that, but the store grants it only after a
review against a list of supported kinds of software (compilers, IDEs,
terminal emulators and similar), and an archive extractor is not on it.

To have `removable-media` connected automatically, ask for it on the
[Snapcraft forum](https://forum.snapcraft.io/c/store-requests/19) under
store requests once the snap is registered, with the reasons above.

## Build and try locally

Needs [snapcraft](https://snapcraft.io/snapcraft) and LXD (or Multipass),
which snapcraft builds in:

```sh
sudo snap install snapcraft --classic
sudo snap install lxd && sudo lxd init --auto
cd packaging/snap
snapcraft pack
sudo snap install --dangerous ./hatchzip_0.1.8_amd64.snap
sudo snap connect hatchzip:removable-media
hatchzip --version
hatchzip.gui
```

The part clones the tag named in `source-tag`, not this checkout, so a
local build always builds a published release. To build local changes,
point `source` at `../..` and remove `source-tag` and `source-depth` for
that build.

Before publishing, try at least: extracting a ZIP in the home folder and one
on a USB drive, resuming an extraction stopped halfway, a split 7z
(`.7z.001`), the file dialogs in the app, the app on Wayland and on X11, and
that a RAR archive is refused with nothing changed. `snapcraft pack` runs
snapcraft's linter on the result; `review-tools` (`sudo snap install
review-tools`, then `review-tools.snap-review hatchzip_0.1.8_amd64.snap`)
runs the store's automated review locally.

## Publish

1. Create an account at <https://snapcraft.io/account> and log in:
   `snapcraft login`.
2. Register the name: `snapcraft register hatchzip`.
3. Build for both architectures. `snapcraft remote-build` builds on
   Launchpad, which needs a Launchpad account and makes the build public;
   or build on an amd64 and an arm64 machine.
4. Upload and release to a channel:

   ```sh
   snapcraft upload --release=edge hatchzip_0.1.8_amd64.snap
   snapcraft upload --release=edge hatchzip_0.1.8_arm64.snap
   ```

   Then, after trying the edge build, promote it:
   `snapcraft release hatchzip <revision> stable`.
5. On the snap's page in the dashboard (<https://snapcraft.io/snaps>), add
   the icon (`assets/icon/png/hatchzip-512.png`), the screenshots
   (<https://hatchzip.dev/img/app-light.png>, `app-dark.png`) and the
   category.
6. Optional, on the forum under store requests: the `hz` alias, so `hz`
   works without the `hatchzip.` prefix, and auto-connection of
   `removable-media`.

Uploads are reviewed automatically by the store's review tools, the same
ones `review-tools` runs locally. Plugging `home` and `removable-media`
needs no store approval; auto-connecting `removable-media` and the `hz`
alias each need the forum request above.

## New version

`update-manifests.py` sets `version` and `source-tag`:

```sh
python3 packaging/update-manifests.py v0.1.9
git diff packaging/snap
```

By hand: set `version` and `source-tag` in `snapcraft.yaml` to the new
release. Then build, upload and release as above.
