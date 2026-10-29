# Packaging

How each download on the releases page is made. The release workflow
(`.github/workflows/release.yml`) does all of it on a tag; this is what it
runs, so any step can be repeated by hand.

| Download | Contents | Built with | On |
|---|---|---|---|
| `hatchzip-<arch>-<os>.tar.gz`, `hatchzip-x86_64-windows.zip` | The CLI, man page, completions | `cargo build`, `tar`, `7z` | Each OS |
| `HatchZip-x64-setup.exe` | App, CLI (`hatchzip.exe` and `hz.exe`) | [Inno Setup 6](https://jrsoftware.org/isinfo.php) | Windows |
| `HatchZip-x64.msi` | The same as the setup | [WiX Toolset 5](https://wixtoolset.org) | Windows |
| `HatchZip-x64-portable.zip` | The setup's files, nothing to install; the app adds the right-click menu from Settings | `7z` | Windows |
| `HatchZip-aarch64.dmg`, `HatchZip-x86_64.dmg` | `HatchZip.app`, with the CLI and man page inside | `hdiutil`, `codesign` | macOS |
| `hatchzip_amd64.deb`, `hatchzip-x86_64.rpm` | App, CLI, `hz` link, man pages, completions, menu entry, icons | [nfpm](https://nfpm.goreleaser.com) | Ubuntu 22.04 |
| `HatchZip-x86_64.AppImage` | App | [linuxdeploy](https://github.com/linuxdeploy/linuxdeploy), [appimagetool](https://github.com/AppImage/appimagetool) | Ubuntu 22.04 |

Every file gets a `.sha256` next to it. Every package is installed and run
before anything is published: in clean Ubuntu 22.04 and Fedora containers
for the .deb and .rpm, by a silent install and uninstall of the setup and
of the MSI on the Windows runner, and by opening the dmg on the Mac runner.

The AUR, winget, Homebrew and Scoop manifests that install these
downloads are in this folder too: see [Package managers](#package-managers).

The Windows and macOS jobs build the app next to the CLI and package both.
The Linux packages have a job of their own on Ubuntu 22.04: the app links
the C library and fontconfig, so it's built on the oldest system it should
run on, while the CLI inside is the static musl build from the tarball.

## Why these tools

One tool for everything, [cargo-packager](https://github.com/crabnebula-dev/cargo-packager),
was the other candidate. It's maintained, but its releases ship no binaries
(the latest, 0.11.8, is from November 2025), so every CI job would compile
it first, and the Windows requirements below would need NSIS hooks anyway.
These tools are all prebuilt, pinned by version and SHA-256, and each one
does one thing that's easy to check:

- **nfpm** writes .deb and .rpm from one YAML file, without dpkg or rpmbuild.
- **Inno Setup** is already on GitHub's Windows runners, installs per user
  without admin rights, and its Pascal code makes the PATH edit exact.
- **WiX** writes the MSI from one XML file. It's a .NET tool, installed at
  a fixed version by `build-msi.ps1`. It stays on version 5: WiX 6
  introduced the [Open Source Maintenance Fee](https://docs.firegiant.com/wix/osmf/),
  and WiX 7 won't build until its EULA is accepted.
- **hdiutil** and **codesign** come with macOS; an `.app` is a folder and a
  plist.
- **linuxdeploy** copies in any shared library the app links that isn't on
  every desktop, and **appimagetool** packs the folder. The AppImage runtime
  is pinned too, rather than downloaded at whatever version is newest.

## Icon

`assets/icon/hatchzip.svg` is the drawing, on a 16 px grid. 16 and 24 px
have their own pixel-fitted drawings next to it. Everything else is generated
and committed, so packaging never needs librsvg:

```sh
python3 -I assets/icon/build.py   # needs rsvg-convert
```

It writes the PNGs in `assets/icon/png/`, `hatchzip.ico` (16 to 256 px),
`hatchzip.icns` (32 to 1024 px, covering 16 to 512 pt on normal and Retina screens) and the `hicolor` set in
`packaging/linux/icons/`. `site/favicon.svg` is a copy of the drawing.

## Linux: .deb and .rpm

`linux/nfpm.yaml` lists every file. It reads them from `dist/pkg/`:

```sh
cargo build --release --locked --bin hatchzip-gui
mkdir -p dist/pkg/completions
cp path/to/static/hatchzip target/release/hatchzip-gui dist/pkg/
dist/pkg/hatchzip man | gzip -9n > dist/pkg/hatchzip.1.gz
dist/pkg/hatchzip completions bash > dist/pkg/completions/hatchzip.bash
dist/pkg/hatchzip completions zsh > dist/pkg/completions/_hatchzip
dist/pkg/hatchzip completions fish > dist/pkg/completions/hatchzip.fish
HATCHZIP_VERSION=0.1.0 nfpm package -f packaging/linux/nfpm.yaml -p deb -t dist/hatchzip_amd64.deb
HATCHZIP_VERSION=0.1.0 nfpm package -f packaging/linux/nfpm.yaml -p rpm -t dist/hatchzip-x86_64.rpm
sh packaging/linux/test-packages.sh   # docker; DOCKER=podman works too
```

The CLI is the static musl build from the
tarball, so the packages' only real dependency is the app's C library.
`linux/check-installed.sh` is what the container test checks.

Both packages install to `/usr`: `hatchzip`, `hz` (a link to it) and
`hatchzip-gui` in `/usr/bin`, `hatchzip.1.gz` and `hz.1.gz` so
`man hatchzip` and `man hz` work, bash, zsh and fish completions, the desktop
entry, AppStream metadata and icons. They don't add the right-click menu:
that lives in each user's home, and a package runs as root, so the app turns
it on.

Desktop entry and metadata checks:

```sh
desktop-file-validate packaging/linux/io.github.xevrion.hatchzip.desktop
appstreamcli validate --no-net packaging/linux/io.github.xevrion.hatchzip.metainfo.xml
```

## Linux: AppImage

```sh
sh packaging/linux/build-appimage.sh target/release/hatchzip-gui
```

Build it on the oldest system it should run on: it uses that system's C
library. CI uses Ubuntu 22.04.

## Windows: setup

```powershell
cargo build --release --locked --bin hatchzip-gui
# dist\payload: the files of hatchzip-x86_64-windows.zip, plus hatchzip-gui.exe
packaging\windows\build-setup.ps1 dist\payload 0.1.0
packaging\windows\test-setup.ps1 dist\HatchZip-x64-setup.exe
```

The setup installs for the current user to
`%LOCALAPPDATA%\Programs\HatchZip`, the folder `install.ps1` uses, with no
admin prompt. It adds a Start menu entry, an entry in Settings > Apps that
uninstalls it, `hz.exe` as a copy of `hatchzip.exe`, the folder on the user
PATH, and HatchZip under "Open with" for .zip, .7z, .tar, .tgz, .gz, .001,
.rar and .r00 files. It runs `hatchzip-gui --register-shell` after
installing and `hatchzip-gui --unregister-shell` before uninstalling.
Uninstalling removes all of it, PATH entry included.

The setup carries a full version resource (company, description, product
name, copyright, version), which Windows shows under Properties > Details
and antivirus engines read. Its compression (`lzma2/max`, solid) is left as
it is: any compressed installer holds high-entropy data, and there is no
measurement showing that another setting changes what the heuristic
engines report.

## Windows: MSI

```powershell
# dist\payload: the same folder as for the setup
packaging\windows\build-msi.ps1 dist\payload 0.1.0
packaging\windows\test-msi.ps1 dist\HatchZip-x64.msi dist\HatchZip-x64-setup.exe
```

`windows/hatchzip.wxs` installs the same files to the same folder as the
setup, for the current user and without an admin prompt (`Scope="perUser"`):
`hatchzip-gui.exe`, `hatchzip.exe`, `hz.exe`, the PowerShell completion
script, the README and the three licenses. It adds the Start menu shortcut,
the folder to the user PATH, the same "Open with" keys as the setup, and an
entry in Settings > Apps with the icon and links to hatchzip.dev and the
repository. A deferred custom action runs the installed
`hatchzip-gui --register-shell` after the files are in, and
`--unregister-shell` before they are removed, each with a rollback action
that undoes it if the install fails later on. A new version replaces the
installed one (`MajorUpgrade`, keyed by the `UpgradeCode`, which never
changes); Windows Installer versions are three numbers, so a pre-release
like 0.2.0-beta.1 is built as 0.2.0.

The setup and the MSI are separate products to Windows, so each refuses to
install while the other is installed: the MSI with a launch condition on
the setup's uninstall entry, the setup by asking Windows Installer for a
product with the MSI's UpgradeCode. A per-user MSI has no key under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall`: Windows
Installer keeps its Settings > Apps entry itself, so `test-msi.ps1` also
reads the installed product's name, version, publisher and links from
Windows Installer. It tries both refusals too.

`build-msi.ps1` checks that the payload holds exactly the files the `.wxs`
lists, since, unlike the setup, the MSI names each file. A file added to the
zip has to be added there too.

## Windows: signing

The release workflow can sign everything Windows runs with Authenticode:
`hatchzip.exe` and `hatchzip-gui.exe` right after the build, so the zip,
the setup and the MSI all carry signed copies (`hz.exe` is a copy of
`hatchzip.exe`), then the setup and the MSI once they are built. The setup's
uninstaller is signed with the programs: Inno Setup writes it unsigned
first, and the setup is built with the signed copy (see `build-setup.ps1`).
Each signed file is checked for a valid, timestamped signature before it
replaces the unsigned one, and the install tests run on the signed
installers and check that every program they install is signed,
`unins000.exe` included.

A dry run on SignPath's `test-signing` policy is checked the same way,
except that the test certificate is self-signed, so a chain ending in an
untrusted root is accepted there (`TEST_CERTIFICATE` in the workflow). A
tag never accepts it, so a release can't ship with the test certificate.

Signing runs when one of the two services below is set up, and is skipped
otherwise; the workflow log says which one it used. Builds of a tag are
signed. Dry runs (the workflow run by hand without a tag) are signed only
when the repository variable `SIGN_DRY_RUNS` is `true`, since a signing
policy can wait for a person to approve each request. If both services are
set up, SignPath is used.

Secrets and variables are created under the repository's Settings >
Secrets and variables > Actions.

**SignPath** ([signpath.io](https://signpath.io); free for open source
through the [SignPath Foundation](https://signpath.org)):

1. In the SignPath project, add the predefined trusted build system
   "GitHub.com" and link it to the project.
2. Create two artifact configurations with the slugs `programs` and
   `installers`, from `.signpath/artifact-configurations/programs.xml` and
   `installers.xml`. SignPath keeps artifact configurations in the project,
   so these files are the copies to paste in; change both together.
3. Create a signing policy, and an API token for a user that can submit
   signing requests with it.

| Kind | Name | Value |
|---|---|---|
| Secret | `SIGNPATH_API_TOKEN` | The API token |
| Variable | `SIGNPATH_ORGANIZATION_ID` | The organization ID |
| Variable | `SIGNPATH_PROJECT_SLUG` | The project slug |
| Variable | `SIGNPATH_SIGNING_POLICY_SLUG` | The signing policy slug, such as `release-signing` |

The workflow uploads the files to sign as the artifacts `signing-programs`
and `signing-installers`, which SignPath downloads (the Windows job has
`actions: read` for that), and waits up to 10 minutes for each request. The
publish job leaves those two artifacts out, since they hold unsigned
copies.

**Azure Artifact Signing** (formerly Trusted Signing): an Artifact Signing
account with a certificate profile, and an app registration with a client
secret that has the "Artifact Signing Certificate Profile Signer" role on
the account.

| Kind | Name | Value |
|---|---|---|
| Secret | `AZURE_TENANT_ID` | The directory (tenant) ID |
| Secret | `AZURE_CLIENT_ID` | The app registration's client ID |
| Secret | `AZURE_CLIENT_SECRET` | The app registration's client secret |
| Variable | `AZURE_SIGNING_ENDPOINT` | The account's endpoint, such as `https://eus.codesigning.azure.net/` |
| Variable | `AZURE_SIGNING_ACCOUNT` | The signing account name |
| Variable | `AZURE_CERTIFICATE_PROFILE` | The certificate profile name |

Files are signed with SHA-256 and timestamped by
`http://timestamp.acs.microsoft.com`.

Either way, the uninstaller the setup writes on the user's machine
(`unins000.exe`) is not signed: Inno Setup signs it while compiling, through
its `SignTool` setting, which needs a signing command on the build machine
rather than a service the workflow calls afterwards.

## macOS: dmg

```sh
cargo build --release --locked --bin hatchzip-gui --target aarch64-apple-darwin
sh packaging/macos/build-dmg.sh aarch64 \
    target/aarch64-apple-darwin/release/hatchzip-gui dist/hatchzip-aarch64-macos
sh packaging/macos/test-dmg.sh dist/HatchZip-aarch64.dmg
```

The last argument of `build-dmg.sh` is the unpacked CLI tarball for the same
architecture. `macos/Info.plist` declares zip, 7z and tar as documents
HatchZip can open, so it shows up in Finder's "Open With" without replacing
Archive Utility as the default.

The app is signed ad hoc, not with a Developer ID, and not notarized, so
macOS asks before the first launch:

- macOS 15 and later: open HatchZip, close the warning, then go to System
  Settings > Privacy & Security and click "Open Anyway".
- macOS 14 and earlier: Control-click HatchZip in Applications, choose Open,
  then Open again.
- Or, in Terminal: `xattr -dr com.apple.quarantine /Applications/HatchZip.app`

The CLI and its man page are inside the app. To use them from a terminal:

```sh
sudo mkdir -p /usr/local/bin /usr/local/share/man/man1
sudo ln -sf /Applications/HatchZip.app/Contents/MacOS/hatchzip /usr/local/bin/hatchzip
sudo ln -sf /Applications/HatchZip.app/Contents/MacOS/hatchzip /usr/local/bin/hz
sudo ln -sf /Applications/HatchZip.app/Contents/Resources/man/man1/hatchzip.1 /usr/local/share/man/man1/hatchzip.1
```

## Package managers

The manifests for the AUR, winget, Homebrew and Scoop live here, ready to
submit. They install the release downloads above, checked against their
published `.sha256`, except the AUR `hatchzip` package and the
homebrew-core formula, which build from the tagged source.

| Folder | Package | Installs |
|---|---|---|
| `aur/hatchzip-bin/` | AUR `hatchzip-bin`, x86_64 | CLI, `hz`, app, man pages, completions, menu entry, icons, licenses. The CLI is the static tarball build, the app comes from the .deb |
| `aur/hatchzip/` | AUR `hatchzip`, x86_64 and aarch64 | The same files, built from source with `cargo build --release` |
| `winget/manifests/x/Xevrion/HatchZip/<version>/` | winget `Xevrion.HatchZip` | `HatchZip-x64-setup.exe`, per user, silent. Not the MSI yet: see below |
| `homebrew/Formula/hatchzip.rb` | tap formula, macOS and Linux, arm64 and x86_64 | CLI, `hz`, man pages, completions, from the tarballs |
| `homebrew/Casks/hatchzip.rb` | tap cask, macOS | `HatchZip.app` from the dmg, plus the CLI, `hz` and man page inside it |
| `homebrew/core/hatchzip.rb` | for a later homebrew-core pull request | CLI built from source |
| `scoop/hatchzip.json` | Scoop | CLI and `hz` from `hatchzip-x86_64-windows.zip` |

The formula and the cask both put `hatchzip` on PATH, so a Mac user
installs one or the other: the cask when they want the app too.

winget lists only the setup. Its manifests can list an MSI next to it for
the same architecture and scope, since installers differ by type, but
v0.1.3, the release the manifests are at, has no MSI, and
`update-manifests.py` only swaps the checksums of installers a manifest
already lists. Once a release ships `HatchZip-x64.msi`, it can be added as a
second installer with `InstallerType: wix` and an `AppsAndFeaturesEntries`
entry carrying the MSI's `UpgradeCode` (in `windows/hatchzip.wxs`), and
`HatchZip-x64.msi` added to `ASSETS` in the script.

### After each release

The release workflow runs `update-manifests.py` after publishing and
uploads the result as the `package-manifests` artifact of the run. It
pushes nothing. To do the same by hand:

```sh
python3 packaging/update-manifests.py v0.1.3
git diff packaging
```

It reads the version the manifests are at now, downloads that release's
`.sha256` files and the new release's, and swaps each checksum for the new
one of the same download, along with the version, the URLs, `pkgrel`, the
winget `ReleaseDate` and the winget version folder. A checksum it can't
match to a download stops it before it writes anything. Commit the result,
then publish each one as below.

### Publishing, the first time

None of this is automated: each registry is published from the owner's own
account.

**AUR.** Create an account at <https://aur.archlinux.org/register> and add
an SSH public key under My Account. Then, for `hatchzip-bin` and again for
`hatchzip`:

```sh
git clone ssh://aur@aur.archlinux.org/hatchzip-bin.git aur-hatchzip-bin
cp packaging/aur/hatchzip-bin/PKGBUILD aur-hatchzip-bin/
cd aur-hatchzip-bin
makepkg --printsrcinfo > .SRCINFO
makepkg -si                 # on Arch: build, install and try it once
namcap PKGBUILD *.pkg.tar.zst
git add PKGBUILD .SRCINFO
git commit -m "hatchzip-bin 0.1.2"
git push origin HEAD:master
```

Cloning a name nobody has taken yet gives an empty repository, and the
first push creates the package. The AUR only accepts the `master` branch.
For later versions, copy the updated PKGBUILD, regenerate `.SRCINFO` and
push again.

**winget.** Either let `wingetcreate` fork and open the pull request, with a
GitHub token that has the `public_repo` scope:

```powershell
winget install Microsoft.WingetCreate
wingetcreate submit --token <token> packaging\winget\manifests\x\Xevrion\HatchZip\0.1.2
```

or fork [microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs),
copy the folder to the same path in the fork, and open the pull request by
hand. On Windows, check it first:

```powershell
winget validate --manifest packaging\winget\manifests\x\Xevrion\HatchZip\0.1.2
winget settings --enable LocalManifestFiles   # once, as administrator
winget install --manifest packaging\winget\manifests\x\Xevrion\HatchZip\0.1.2
```

The first pull request is reviewed by hand, the installer is scanned, and
a bot asks for the Microsoft CLA to be signed. Later versions go the same
way, or with `wingetcreate update Xevrion.HatchZip --version <version>
--urls <setup URL> --submit`.

**Homebrew.** Create a public repository named `xevrion/homebrew-tap` and
copy `Formula/` and `Casks/` into it:

```sh
gh repo create xevrion/homebrew-tap --public --clone
cp -r packaging/homebrew/Formula packaging/homebrew/Casks homebrew-tap/
cd homebrew-tap
git add Formula Casks
git commit -m "hatchzip 0.1.2"
git push
brew install xevrion/tap/hatchzip           # then check it works
brew test xevrion/tap/hatchzip
```

`homebrew/core/hatchzip.rb` is for later. homebrew-core takes software
that is notable enough (`brew audit --new` checks the stars, forks and
watchers) and builds it from source. When it qualifies, fork
[Homebrew/homebrew-core](https://github.com/Homebrew/homebrew-core), add
the file as `Formula/h/hatchzip.rb`, run `brew audit --new --formula
hatchzip`, `brew install --build-from-source hatchzip` and `brew test
hatchzip`, and open the pull request. Then drop the formula from the tap,
so the two don't shadow each other; the cask stays in the tap.

**Scoop.** Create a public bucket repository, for example from
[ScoopInstaller/BucketTemplate](https://github.com/ScoopInstaller/BucketTemplate),
named `xevrion/scoop-bucket`, and add the manifest as `bucket/hatchzip.json`:

```sh
gh repo create xevrion/scoop-bucket --public --clone \
    --template ScoopInstaller/BucketTemplate
cp packaging/scoop/hatchzip.json scoop-bucket/bucket/
cd scoop-bucket
git add bucket/hatchzip.json
git commit -m "hatchzip: Add version 0.1.2"
git push
```

The manifest's `checkver` and `autoupdate` let Scoop's own tools update it:
the template's scheduled workflow does that inside the bucket repository,
if it is left enabled. Once HatchZip is well known, the same file can go to
[ScoopInstaller/Main](https://github.com/ScoopInstaller/Main) by pull
request.

## What the packages expect from `hatchzip-gui`

- `--help` prints and exits without opening a window or needing a display:
  the Linux and macOS tests run it to prove the app starts.
- `--register-shell` and `--unregister-shell` exit when done, with no window.
- On Windows, the exe carries `assets/icon/hatchzip.ico` as its first icon
  resource (the right-click menu, Start menu and "Open with" all show icon 0
  of the exe) and a version resource. `hatchzip.exe` carries a version
  resource too (`crates/cli/build.rs`), with the same product and company
  names.
- On Linux, the window's Wayland app ID and X11 class are
  `io.github.xevrion.hatchzip`, so the desktop shows the menu entry's icon.
- Inside an AppImage the program runs from a temporary mount, so
  `--register-shell` should point the menu at `$APPIMAGE`, not at its own path.
- On macOS, "Open With" and drops on the Dock icon arrive as an Apple Event,
  not as a command line argument.
