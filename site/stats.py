#!/usr/bin/env python3
"""Fills in the live numbers on the site before it is deployed.

The site has no JavaScript, so the numbers are written into the HTML at
deploy time instead: the pages workflow runs this every hour. Each number
sits between markers, `<!--stat:NAME-->...<!--/stat-->`, and only the text
between them changes. If the GitHub API can't be read, the files are left as
they are, numbers and date together, so a page never shows a date its
numbers weren't checked on.

    python3 site/stats.py site/sponsor.html [more files...]

Reads GITHUB_TOKEN when set, for the higher rate limit.
"""

import datetime
import json
import os
import re
import sys
import urllib.request

REPO = "xevrion/hatchzip"
API = "https://api.github.com"
# Files that are not downloads of HatchZip itself.
NOT_DOWNLOADS = (".sha256", ".intoto.jsonl", ".sig", ".asc", ".minisig")


def get(path):
    """One GET against the API: the decoded JSON and the Link header."""
    req = urllib.request.Request(API + path)
    req.add_header("Accept", "application/vnd.github+json")
    req.add_header("User-Agent", "hatchzip-site-stats")
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        req.add_header("Authorization", "Bearer " + token)
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.load(resp), resp.headers.get("Link", "")


def pages(path):
    """Every item of a paginated list."""
    items, page = [], 1
    while True:
        sep = "&" if "?" in path else "?"
        batch, link = get(f"{path}{sep}per_page=100&page={page}")
        items.extend(batch)
        if 'rel="next"' not in link:
            return items
        page += 1


def platform(name):
    low = name.lower()
    if "windows" in low or low.endswith((".msi", "setup.exe")) or "-x64" in low:
        return "windows"
    if "macos" in low or "darwin" in low or low.endswith(".dmg"):
        return "macos"
    if "linux" in low or low.endswith((".deb", ".rpm", ".appimage")):
        return "linux"
    return "other"


def collect():
    repo, _ = get(f"/repos/{REPO}")
    counts = {"windows": 0, "macos": 0, "linux": 0, "other": 0}
    for release in pages(f"/repos/{REPO}/releases"):
        for asset in release["assets"]:
            if not asset["name"].lower().endswith(NOT_DOWNLOADS):
                counts[platform(asset["name"])] += asset["download_count"]
    contributors = pages(f"/repos/{REPO}/contributors")
    today = datetime.datetime.now(datetime.timezone.utc).date()
    return {
        "stars": f"{repo['stargazers_count']:,}",
        "forks": f"{repo['forks_count']:,}",
        "downloads": f"{sum(counts.values()):,}",
        "downloads-windows": f"{counts['windows']:,}",
        "downloads-linux": f"{counts['linux']:,}",
        "downloads-macos": f"{counts['macos']:,}",
        "contributors": f"{len(contributors):,}",
        "date": f"{today.day} {today:%B %Y}",
    }


MARKER = re.compile(r"(<!--stat:([a-z-]+)-->)(.*?)(<!--/stat-->)", re.S)


def fill(text, stats):
    def one(m):
        name = m.group(2)
        if name not in stats:
            raise SystemExit(f"unknown stat {name!r}")
        return m.group(1) + stats[name] + m.group(4)

    return MARKER.sub(one, text)


def main(files):
    try:
        stats = collect()
    except Exception as e:  # noqa: BLE001 - any failure leaves the files alone
        print(f"stats: GitHub API not read ({e}); leaving the numbers as they are")
        return
    for path in files:
        with open(path, encoding="utf-8") as f:
            text = f.read()
        new = fill(text, stats)
        if new != text:
            with open(path, "w", encoding="utf-8") as f:
                f.write(new)
    print("stats:", json.dumps(stats))


if __name__ == "__main__":
    main(sys.argv[1:])
