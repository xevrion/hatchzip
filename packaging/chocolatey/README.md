# Chocolatey

The `hatchzip` package for the [Chocolatey Community Repository](https://community.chocolatey.org).
It downloads `HatchZip-x64-setup.exe` from the GitHub release, checks it
against the SHA-256 in `tools/chocolateyinstall.ps1`, and runs it silently.

| File | What it is |
|---|---|
| `hatchzip.nuspec` | Package metadata |
| `tools/chocolateyinstall.ps1` | Downloads, checks and runs the setup |
| `tools/chocolateyuninstall.ps1` | Runs the setup's own uninstaller |

Nothing is embedded in the package, so it needs no `LICENSE.txt` or
`VERIFICATION.txt` (rules CPMR0005 and CPMR0006 apply only to packages that
include binaries).

## Why the setup and not the MSI

Both install the same files to the same folder, for the current user only.
Chocolatey's [package validator rules](https://docs.chocolatey.org/en-us/community-repository/moderation/package-validator/rules)
have no rule that prefers one installer type or covers per-user installs;
the only install location rule, CPMR0008, is about portable packages
writing to Program Files. So the choice rests on how each behaves under
Chocolatey:

- **Uninstalling.** The setup writes a normal uninstall entry under
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall`, which
  Chocolatey's `Get-UninstallRegistryKey` and its automatic uninstaller
  find. A per-user MSI has no such key: Windows Installer keeps its Settings
  > Apps entry itself (see "Windows: MSI" in `packaging/README.md`), so the
  package would have to find the product through Windows Installer by the
  MSI's UpgradeCode.
- **Silent switches.** The setup's are Inno Setup's standard ones, the same
  winget uses for this installer.
- **Consistency.** winget lists the setup too, so both package managers
  install the same file.

The uninstall script is there so uninstalling doesn't depend on the
`autoUninstaller` feature being enabled.

### Per-user install under Chocolatey

Chocolatey normally runs elevated. The setup has `PrivilegesRequired=lowest`,
so even when started elevated it installs into the profile of the account
that runs it. From an elevated prompt of the user's own account, that is the
user, which is the normal case. Run as another account (SYSTEM, a
deployment tool, a separate administrator), HatchZip lands in that
account's `%LOCALAPPDATA%` and PATH, where the user can't see it. The
package description says so. A machine-wide install would need the setup to
allow it (`PrivilegesRequiredOverridesAllowed` in `hatchzip.iss`), which is
a change to the setup rather than to this package.

## Test locally

On Windows, from this folder, in an elevated PowerShell:

```powershell
choco pack
choco install hatchzip --source "'.;https://community.chocolatey.org/api/v2/'" -y
hatchzip --version         # in a new terminal, or after refreshenv
choco uninstall hatchzip -y
```

Check after uninstalling that `%LOCALAPPDATA%\Programs\HatchZip` is gone and
that HatchZip is no longer in Settings > Apps or on the user PATH.

For a clean machine, the [Chocolatey Testing Environment](https://github.com/chocolatey-community/chocolatey-test-environment)
is a Vagrant setup similar to the package verifier. Its box moves off
HashiCorp's registry at the end of 2026; its README says how to build it
instead. Windows Sandbox also gives a clean machine for a quick check.

## Publish

1. Create an account at <https://community.chocolatey.org/account/Register>.
   If the user name is not `xevrion`, change `<owners>` in the nuspec to it.
2. Copy the API key from the account page, then:

   ```powershell
   choco apikey --key <api key> --source https://push.chocolatey.org/
   choco pack
   choco push hatchzip.0.1.8.nupkg --source https://push.chocolatey.org/
   ```

3. Follow the package's page on community.chocolatey.org through moderation,
   below.

## Moderation

Every version of a package that is not trusted is reviewed by a person
before it is listed ([Moderation](https://docs.chocolatey.org/en-us/community-repository/moderation/)).
Before that, three automated checks run, starting about 30 minutes after the
push:

- **Validator**: the nuspec and scripts against the rules linked above.
  Requirements must be fixed; guidelines and suggestions are advice, and a
  moderator can't hold a package on those alone.
- **Verifier**: installs and uninstalls the package in a clean Windows VM.
- **Scanner**: sends every file in the package, and every file it downloads
  while installing, to VirusTotal.

When a check or the moderator asks for a change, fix it and push the exact
same version again. A package left waiting for the maintainer gets a
reminder after 20 days and is rejected 15 days after that.

### VirusTotal and unsigned downloads

The scanner sorts each version by its VirusTotal detections
([Chocolatey blog, April 2021](https://blog.chocolatey.org/2021/04/package-moderation-added-to-moderation-process)):

| Detections | Result |
|---|---|
| 0 | Not flagged |
| 1 to 5 | Flagged, note. Approved automatically only for trusted packages |
| 6 to 10 | Flagged, warning. A moderator has to review it |
| More than 10 | Flagged, error. Sent back to the maintainer to investigate |

Until the Windows builds are signed (see "Windows: signing" in
`packaging/README.md`), the setup and the programs it installs are
unsigned. If the scan flags a version:

1. Read the VirusTotal report: which engines flagged which file, and under
   what name. Treat it as a real finding until the file is shown to be the
   one the release workflow built (the next step).
2. Reply on the package page with what the file is and how to check it:
   the build provenance (`gh attestation verify HatchZip-x64-setup.exe --repo xevrion/hatchzip`),
   the published `.sha256`, the VirusTotal links on
   <https://hatchzip.dev/verify.html>, and that the release workflow builds
   it from the tagged source.
3. Report the false positive to each vendor that flagged it, through that
   vendor's own submission form, and say on the package page when it is
   done.

Once the release workflow signs the Windows builds, this package needs no
change for it beyond the new checksum of the signed setup.

## New version

`update-manifests.py` rewrites the version, the download URL, the icon and
release notes URLs and the checksum here, along with the other manifests:

```sh
python3 packaging/update-manifests.py v0.1.9
git diff packaging/chocolatey
```

By hand: set `<version>`, the `@v` in `<iconUrl>` and the tag in
`<releaseNotes>` in the nuspec, and `url64bit` and `checksum64` in
`tools/chocolateyinstall.ps1` (the value in `HatchZip-x64-setup.exe.sha256`
of the release). Then pack, test and push as above. Each version goes
through moderation again.
