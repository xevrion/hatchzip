"""Regenerate the tar.xz fixtures with Python 3 and XZ Utils 5.8.1."""

import io
import os
from pathlib import Path
import subprocess
import tarfile

HERE = Path(__file__).resolve().parent
XZ = os.environ.get("XZ", "xz")


def payload(length):
    x = 12345
    pattern = bytearray()
    for _ in range(262144):
        x = (x * 1103515245 + 12345) & 0x7FFFFFFF
        pattern.append((x >> 16) & 0xFF)
    for i in range(0, len(pattern) - 5, 64):
        pattern[i : i + 5] = b"\xe8\x01\x00\x00\x00"
    return (pattern * ((length + len(pattern) - 1) // len(pattern)))[:length]


def tar_bytes(length):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        directory = tarfile.TarInfo("game/saves/")
        directory.type = tarfile.DIRTYPE
        directory.mode = 0o755
        tar.addfile(directory)
        for name, data in [("game/tool.exe", payload(length)), ("game/readme.txt", b"tar.xz fixture\n")]:
            entry = tarfile.TarInfo(name)
            entry.mode = 0o644
            entry.size = len(data)
            tar.addfile(entry, io.BytesIO(data))
    return output.getvalue()


def make(name, args, data):
    result = subprocess.run([XZ, "--stdout", *args], input=data, check=True, stdout=subprocess.PIPE)
    (HERE / name).write_bytes(result.stdout)
    return result.stdout


data = tar_bytes(4 * 1024 * 1024 + 4096)
make("default.tar.xz", ["-T2", "-6"], tar_bytes(25 * 1024 * 1024))
multi = make("multi.tar.xz", ["-T2", "-6", "--block-size=1MiB"], data)
single = make("single.tar.xz", ["-T1", "-6"], data)
make("bcj.tar.xz", ["-T2", "--block-size=1MiB", "--x86", "--lzma2=preset=6"], data)
make("delta.tar.xz", ["-T2", "--block-size=1MiB", "--delta=dist=4", "--lzma2=preset=6"], data)
(HERE / "multistream.tar.xz").write_bytes(single + single)
size = (len(multi) + 2) // 3
for i in range(3):
    (HERE / f"split.tar.xz.{i + 1:03}").write_bytes(multi[i * size : (i + 1) * size])
