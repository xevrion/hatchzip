# Flatpak

The Flathub package of HatchZip: the desktop app and the `hatchzip` command,
built from the tagged source.

| File | What it is |
|---|---|
| `io.github.xevrion.hatchzip.yml` | The manifest |
| `io.github.xevrion.hatchzip.metainfo.xml` | AppStream metadata for this build, with screenshots and releases |
| `cargo-sources.json` | Every crate in the tag's `Cargo.lock`, for the offline build. Generated, not committed: see below |

The app ID is the one the desktop entry, the metainfo and the window's
Wayland app ID already use. The desktop entry and the icons come from
`packaging/linux/` in the tagged source.

## What differs from the release builds

- **No RAR.** The build uses the default features, so the `rar` feature and
  the UnRAR source it compiles are left out. UnRAR's license is not a free
  software license. A RAR archive is refused before anything is touched,
  and the desktop entry drops the RAR types (`desktop-file-edit` in the
  manifest). `cargo-sources.json` still lists `unrar-ng-sys`, because it
  covers the whole `Cargo.lock`; it is downloaded but not compiled.
- **The right-click menu switch does nothing.** Settings, "Show in the file
  manager's context menu" writes its entries under `$XDG_DATA_HOME`, which
  inside the sandbox is `~/.var/app/io.github.xevrion.hatchzip/data`, and
  points them at `/app/bin/hatchzip-gui`, which does not exist outside the
  sandbox. "Open with" from the file manager works through the desktop
  entry. Hiding the switch when `FLATPAK_ID` is set would need a change in
  `crates/gui`.
- **The CLI is not on the host PATH.** It runs with
  `flatpak run --command=hatchzip io.github.xevrion.hatchzip`. No man page
  or completions are installed, since Flatpak exports neither.

## Permissions

| Permission | Why |
|---|---|
| `--socket=wayland`, `--socket=fallback-x11`, `--share=ipc` | The window, natively on Wayland and through X11 elsewhere. Flatpak recommends `fallback-x11` with `wayland` for apps that support Wayland, and `ipc` for X11 shared memory |
| `--device=dri` | OpenGL drawing. Without it the app falls back to its software renderer |
| `--filesystem=home` | See below |
| `--filesystem=/run/media`, `--filesystem=/media`, `--filesystem=/mnt` | Removable and secondary drives: udisks2 mounts them under `/run/media` on most distributions and `/media` on Debian and Ubuntu, and `/mnt` is the usual manual mount point |

No network permission: the app makes no network connections. "Check for
updates" opens the release page in the browser through the OpenURI portal.

Flathub's requirements say static permissions "must be kept to an absolute
minimum", and that a portal is mandatory "when a suitable XDG portal exists,
is supported across the ecosystem, and covers the application's use case"
([Requirements, Permissions](https://docs.flathub.org/docs/for-app-authors/requirements#permissions)).
Flatpak's own guidance is to prefer portals, and to use specific folders
rather than the whole home folder where possible
([Sandbox permissions, Filesystem access](https://docs.flatpak.org/en/latest/sandbox-permissions.html#filesystem-access)).

The file chooser portal does not cover in-place extraction. It grants
access to the one file the user picked, through the document portal, while
HatchZip also needs:

- to truncate the archive as each part is extracted, and delete it at the
  end;
- to create `<archive>.hatchzip` and `<archive>.hatchzip-tail` next to the
  archive, the progress files that let an extraction resume;
- to read the other volumes of a split archive (`.001`, `.002`, `.z01` and
  so on) from the same folder;
- to create the default output folder next to the archive.

Archives can be anywhere in the home folder, and large ones are often on a
second drive, so no single XDG folder such as `xdg-download` covers them.
`--filesystem=home` plus the three mount folders is narrower than
`--filesystem=host`, which `org.kde.ark` uses on Flathub, and the same home
access as `org.gnome.FileRoller`. None of the four trips a
`flatpak-builder-lint` error: the linter refuses subfolders of `/run/media`
and absolute `/home` paths, not these. Explain this in the submission pull
request, since Flathub reviews permissions separately.

With these permissions, files picked in the file chooser and files opened
from the file manager reach the app as their real paths, so the side files
and volumes next to them are reachable.

## Generate cargo-sources.json

[flatpak-cargo-generator](https://github.com/flatpak/flatpak-builder-tools/tree/master/cargo)
reads a `Cargo.lock` and writes the list of crates to download. Run it on
the lockfile of the tag the manifest builds:

```sh
git clone https://github.com/flatpak/flatpak-builder-tools
python3 -m venv /tmp/fcg && /tmp/fcg/bin/pip install aiohttp tomlkit
git -C /path/to/hatchzip show v0.1.8:Cargo.lock > /tmp/Cargo.lock
/tmp/fcg/bin/python flatpak-builder-tools/cargo/flatpak-cargo-generator.py \
    /tmp/Cargo.lock -o packaging/flatpak/cargo-sources.json
```

The tool's own instructions use Poetry instead (`poetry install`, then
`poetry env activate` inside its `cargo` folder); either works. The output
is large and changes with every dependency update, so it lives in the
Flathub repository rather than here.

## Build and run locally

Flathub's builder image carries `flatpak-builder` and the linter:

```sh
flatpak remote-add --if-not-exists --user flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak install --user -y flathub org.flatpak.Builder
cd packaging/flatpak
flatpak run --command=flathub-build org.flatpak.Builder --install io.github.xevrion.hatchzip.yml
flatpak run io.github.xevrion.hatchzip
flatpak run --command=hatchzip io.github.xevrion.hatchzip --version
```

Then lint the manifest and the built repository:

```sh
flatpak run --command=flatpak-builder-lint org.flatpak.Builder manifest io.github.xevrion.hatchzip.yml
flatpak run --command=flatpak-builder-lint org.flatpak.Builder repo repo
```

Before submitting, try at least: extracting a ZIP in the home folder and
one on a USB drive, resuming an extraction stopped halfway, a split 7z
(`.7z.001`), the software renderer
(`flatpak run --nodevice=dri io.github.xevrion.hatchzip`), and that a RAR
archive is refused with nothing changed.

The metainfo checks without a build:

```sh
appstreamcli validate packaging/flatpak/io.github.xevrion.hatchzip.metainfo.xml
```

## Submit to Flathub

1. Fork [flathub/flathub](https://github.com/flathub/flathub/fork) with
   "Copy the master branch only" cleared, and check out `new-pr`:

   ```sh
   gh repo fork --clone flathub/flathub && cd flathub
   git checkout --track origin/new-pr
   git checkout -b io.github.xevrion.hatchzip new-pr
   ```

2. Copy in the manifest, the metainfo and the generated `cargo-sources.json`,
   at the top of the repository:

   ```sh
   cp /path/to/hatchzip/packaging/flatpak/io.github.xevrion.hatchzip.yml \
      /path/to/hatchzip/packaging/flatpak/io.github.xevrion.hatchzip.metainfo.xml \
      /path/to/hatchzip/packaging/flatpak/cargo-sources.json .
   git add io.github.xevrion.hatchzip.yml io.github.xevrion.hatchzip.metainfo.xml cargo-sources.json
   git commit -m "Add io.github.xevrion.hatchzip"
   git push -u origin io.github.xevrion.hatchzip
   ```

3. Open the pull request against the `new-pr` branch, not `master`, titled
   "Add io.github.xevrion.hatchzip". In the description, explain the
   filesystem permissions (the section above) and that RAR is left out on
   purpose.
4. Answer the review. When it is done, comment `bot, build` to start a
   test build.
5. Once approved, Flathub invites the account to the new
   `flathub/io.github.xevrion.hatchzip` repository. Two-factor
   authentication must be on, and the invitation expires after a week.
   Later updates are pull requests to that repository.
6. After the first publish, log in to flathub.org with the GitHub account
   that owns `xevrion/hatchzip` to verify the app; an `io.github.` ID is
   verified through that account.

## New version

In the `flathub/io.github.xevrion.hatchzip` repository (and here, to keep
the two the same):

1. In the manifest, set `tag` to the new tag and `commit` to its commit
   (`git rev-list -n 1 v0.1.9`). flatpak-builder refuses a tag and commit
   that don't match.
2. Regenerate `cargo-sources.json` from the new tag's `Cargo.lock`, as above.
3. Add a `<release version="0.1.9" date="YYYY-MM-DD">` at the top of
   `<releases>` in the metainfo, with a short description. Leave out
   anything about RAR.
4. Build, run and lint locally, then open a pull request. Flathub builds it
   and posts a test build to try before merging.

The `x-checker-data` entry lets Flathub's external data checker open a pull
request for a new tag on its own, but it only updates the `tag` and
`commit`: `cargo-sources.json` and the metainfo still need the steps above
before that pull request is merged.

`update-manifests.py` does not touch these files: the commit and the crate
list can't be derived from the release's `.sha256` files.
