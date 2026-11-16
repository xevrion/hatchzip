#!/usr/bin/env python3
"""Build hatchzip.<version>.nupkg without choco or nuget, for pushing from
macOS or Linux. A .nupkg is an OPC zip: the nuspec (without <files>), the
tools folder, and the relationship and content-type parts NuGet expects.

    python3 packaging/chocolatey/pack.py [out-dir]
"""

import re
import sys
import uuid
import zipfile
from pathlib import Path

HERE = Path(__file__).resolve().parent


def main() -> None:
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else Path.cwd()
    nuspec = (HERE / "hatchzip.nuspec").read_text(encoding="utf-8")
    meta = lambda tag: re.search(rf"<{tag}>(.*?)</{tag}>", nuspec, re.S).group(1)
    version = meta("version")
    # nuget pack leaves <files> out of the packed nuspec.
    packed = re.sub(r"\s*<files>.*?</files>", "", nuspec, flags=re.S)
    core = f"package/services/metadata/core-properties/{uuid.uuid4().hex}.psmdcp"
    rels = (
        '<?xml version="1.0" encoding="utf-8"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        '<Relationship Type="http://schemas.microsoft.com/packaging/2010/07/manifest" '
        f'Target="/hatchzip.nuspec" Id="R{uuid.uuid4().hex[:16]}" />'
        '<Relationship Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" '
        f'Target="/{core}" Id="R{uuid.uuid4().hex[:16]}" />'
        "</Relationships>"
    )
    props = (
        '<?xml version="1.0" encoding="utf-8"?>'
        '<coreProperties xmlns:dc="http://purl.org/dc/elements/1.1/" '
        'xmlns="http://schemas.openxmlformats.org/package/2006/metadata/core-properties">'
        f"<dc:creator>{meta('authors')}</dc:creator><dc:identifier>{meta('id')}</dc:identifier>"
        f"<version>{version}</version><dc:title>{meta('title')}</dc:title>"
        "</coreProperties>"
    )
    types = (
        '<?xml version="1.0" encoding="utf-8"?>'
        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml" />'
        '<Default Extension="nuspec" ContentType="application/octet" />'
        '<Default Extension="ps1" ContentType="application/octet" />'
        '<Default Extension="psmdcp" ContentType="application/vnd.openxmlformats-package.core-properties+xml" />'
        "</Types>"
    )
    path = out / f"hatchzip.{version}.nupkg"
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("_rels/.rels", rels)
        z.writestr("hatchzip.nuspec", packed)
        for f in sorted((HERE / "tools").iterdir()):
            z.write(f, f"tools/{f.name}")
        z.writestr(core, props)
        z.writestr("[Content_Types].xml", types)
    print(path)


if __name__ == "__main__":
    main()
