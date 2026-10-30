#!/usr/bin/env python3
"""Rewrites the package manager manifests in packaging/ for a new release.

    python3 packaging/update-manifests.py v0.1.3
    python3 packaging/update-manifests.py v0.1.3 --sha256-dir dist

It changes the AUR PKGBUILDs and .SRCINFO files, the winget manifests (and
renames their version folder), the Homebrew formulae and cask, the Scoop
manifest, the Chocolatey package and the snapcraft.yaml. Nothing is pushed
or submitted: see "Package managers" in packaging/README.md for that. The
Flatpak manifest is not touched: it names the tag's commit and needs a
cargo-sources.json regenerated from the new Cargo.lock (see
packaging/flatpak/README.md).

How the checksums are matched: the manifests are at some release now (the
version in scoop/hatchzip.json). The script reads that release's published
.sha256 files and the new release's, so it knows, for each checksum in the
manifests, which download it belongs to, and swaps it for the new one. This
works whatever the file format, and a checksum it can't account for is an
error rather than something left stale. The GitHub source archive has no
.sha256 of its own, so for that one both versions are downloaded and hashed.

The new .sha256 files are read from --sha256-dir when given (the release
workflow passes its dist folder) and downloaded from the release otherwise.
Only the Python standard library is used.
"""

import argparse
import datetime
import hashlib
import json
import re
import shutil
import sys
import urllib.request
from pathlib import Path

REPO = "xevrion/hatchzip"
PACKAGING = Path(__file__).resolve().parent

# Every release download a manifest points at.
ASSETS = [
    "hatchzip-x86_64-linux.tar.gz",
    "hatchzip-aarch64-linux.tar.gz",
    "hatchzip-x86_64-macos.tar.gz",
    "hatchzip-aarch64-macos.tar.gz",
    "hatchzip-x86_64-windows.zip",
    "hatchzip_amd64.deb",
    "HatchZip-x64-setup.exe",
    "HatchZip-aarch64.dmg",
    "HatchZip-x86_64.dmg",
]
SOURCE = "source archive"

SCOOP = PACKAGING / "scoop" / "hatchzip.json"
WINGET = PACKAGING / "winget" / "manifests" / "x" / "Xevrion" / "HatchZip"
FIXED_FILES = [
    PACKAGING / "aur" / "hatchzip-bin" / "PKGBUILD",
    PACKAGING / "aur" / "hatchzip-bin" / ".SRCINFO",
    PACKAGING / "aur" / "hatchzip" / "PKGBUILD",
    PACKAGING / "aur" / "hatchzip" / ".SRCINFO",
    PACKAGING / "homebrew" / "Formula" / "hatchzip.rb",
    PACKAGING / "homebrew" / "Casks" / "hatchzip.rb",
    PACKAGING / "homebrew" / "core" / "hatchzip.rb",
    PACKAGING / "chocolatey" / "hatchzip.nuspec",
    PACKAGING / "chocolatey" / "tools" / "chocolateyinstall.ps1",
    PACKAGING / "snap" / "snapcraft.yaml",
    SCOOP,
]

HASH = re.compile(r"\b[0-9A-Fa-f]{64}\b")
VERSION = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?$")


def fetch(url: str) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": "hatchzip-update-manifests"})
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            return response.read()
    except OSError as e:
        sys.exit(f"update-manifests: could not download {url}: {e}")


def parse_sha256(text: str, where: str) -> str:
    # "<hash>  <name>" from sha256sum, or "<hash> *<name>" from Windows.
    fields = text.split()
    if not fields or not re.fullmatch(r"[0-9A-Fa-f]{64}", fields[0]):
        sys.exit(f"update-manifests: no SHA-256 in {where}")
    return fields[0].lower()


def release_hashes(version: str, sha256_dir: Path | None) -> dict[str, str]:
    """Download name to SHA-256 for one release."""
    hashes = {}
    for asset in ASSETS:
        if sha256_dir is not None:
            path = sha256_dir / f"{asset}.sha256"
            if not path.is_file():
                sys.exit(f"update-manifests: {path} is missing")
            hashes[asset] = parse_sha256(path.read_text(), str(path))
        else:
            url = f"https://github.com/{REPO}/releases/download/v{version}/{asset}.sha256"
            hashes[asset] = parse_sha256(fetch(url).decode(), url)
    url = f"https://github.com/{REPO}/archive/refs/tags/v{version}.tar.gz"
    hashes[SOURCE] = hashlib.sha256(fetch(url)).hexdigest()
    return hashes


def release_date(version: str) -> str:
    url = f"https://api.github.com/repos/{REPO}/releases/tags/v{version}"
    try:
        published = json.loads(fetch(url))["published_at"]
        return published[:10]
    except (KeyError, TypeError, ValueError, SystemExit):
        return datetime.datetime.now(datetime.UTC).date().isoformat()


def rewrite(text: str, path: Path, old: str, new: str, swap: dict[str, str]) -> str:
    def new_hash(match: re.Match) -> str:
        found = match.group(0)
        replacement = swap.get(found.lower())
        if replacement is None:
            sys.exit(
                f"update-manifests: {path.relative_to(PACKAGING)} has the checksum {found}, "
                f"which belongs to no download of {old}. Fix the file by hand first."
            )
        # winget writes checksums in capitals, everything else in lower case.
        return replacement.upper() if found.isupper() else replacement

    text = HASH.sub(new_hash, text)
    if old != new:
        # 0.1.2 as a whole version: not inside 10.1.2, 0.1.23 or 0.1.2.4.
        text = re.sub(rf"(?<![0-9.]){re.escape(old)}(?![0-9]|\.[0-9])", new, text)
        text = re.sub(r"(?m)^pkgrel=\d+$", "pkgrel=1", text)
        text = re.sub(r"(?m)^\tpkgrel = \d+$", "\tpkgrel = 1", text)
    return text


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("tag", help="the release tag, such as v0.1.3")
    parser.add_argument("--sha256-dir", type=Path, help="folder holding the new release's .sha256 files")
    parser.add_argument("--date", help="winget ReleaseDate, YYYY-MM-DD (default: the release's publish date)")
    args = parser.parse_args()

    new = args.tag.removeprefix("v")
    if not VERSION.match(new):
        sys.exit(f"update-manifests: {args.tag} is not a version tag like v0.1.3")
    old = json.loads(SCOOP.read_text())["version"]
    if args.date is not None and not re.fullmatch(r"\d{4}-\d{2}-\d{2}", args.date):
        sys.exit(f"update-manifests: --date {args.date} is not YYYY-MM-DD")

    print(f"manifests are at {old}, updating to {new}")
    old_hashes = release_hashes(old, None)
    new_hashes = release_hashes(new, args.sha256_dir)
    swap = {old_hashes[name]: new_hashes[name] for name in old_hashes}
    if len(swap) != len(old_hashes):
        sys.exit(f"update-manifests: two downloads of {old} have the same checksum")
    date = args.date or release_date(new)

    old_dir, new_dir = WINGET / old, WINGET / new
    if not old_dir.is_dir():
        sys.exit(f"update-manifests: {old_dir} is missing")
    winget_files = sorted(old_dir.glob("*.yaml"))

    changed = {}
    for path in FIXED_FILES + winget_files:
        text = rewrite(path.read_text(), path, old, new, swap)
        if path.parent == old_dir:
            text = re.sub(r"(?m)^ReleaseDate: .*$", f"ReleaseDate: {date}", text)
        changed[path] = text

    # Everything is checked before anything is written.
    for path, text in changed.items():
        if old != new and re.search(rf"(?<![0-9.]){re.escape(old)}(?![0-9]|\.[0-9])", text):
            sys.exit(f"update-manifests: {path} still mentions {old}")
    for path, text in changed.items():
        path.write_text(text)
        print(f"  {path.relative_to(PACKAGING)}")
    if old != new:
        if new_dir.exists():
            shutil.rmtree(new_dir)
        old_dir.rename(new_dir)
        print(f"  winget: {old_dir.relative_to(PACKAGING)} renamed to {new_dir.relative_to(PACKAGING)}")
    print("done; review with `git diff packaging`, then publish by hand (packaging/README.md)")


if __name__ == "__main__":
    main()
