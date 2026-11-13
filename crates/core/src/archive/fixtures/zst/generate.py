#!/usr/bin/env python3
"""Rebuilds the zstd fixtures with the real zstd 1.5.7 and pzstd 1.5.7.

Run from this folder with both CLIs on PATH. The payloads are deterministic,
so the properties in README.md stay true; byte-identical output is not
promised across zstd builds.
"""

import io
import random
import struct
import subprocess
import tarfile

SMALL = 128 * 1024
BIG = 4 * 1024 * 1024


def noise(length: int, seed: int) -> bytes:
    rnd = random.Random(seed)
    return bytes(rnd.getrandbits(8) for _ in range(length))


def text(length: int) -> bytes:
    words = (
        "hatch every byte of the archive back into files while the disk stays nearly "
        "full and a crash at any moment loses nothing 42 1337 zran"
    ).split()
    out = []
    n = 0
    x = 0x2545F4914F6CDD1D
    while n < length:
        x ^= (x << 13) & 0xFFFFFFFFFFFFFFFF
        x ^= x >> 7
        x ^= (x << 17) & 0xFFFFFFFFFFFFFFFF
        w = words[x % len(words)]
        out.append(w.encode())
        out.append(b"\n" if x & 0x700 == 0 else b" ")
        n += len(w) + 1
    return b"".join(out)[:length]


def tar(name: str, files: dict[str, bytes], empty_dir: bool) -> None:
    with tarfile.open(name, "w", format=tarfile.GNU_FORMAT) as t:
        for path, data in files.items():
            ti = tarfile.TarInfo(path)
            ti.size = len(data)
            ti.mode = 0o644
            ti.mtime = 1700000000
            ti.uid = ti.gid = 0
            ti.uname = ti.gname = ""
            t.addfile(ti, io.BytesIO(data))
        if empty_dir:
            ti = tarfile.TarInfo("game/empty")
            ti.type = tarfile.DIRTYPE
            ti.mode = 0o755
            ti.mtime = 1700000000
            ti.uid = ti.gid = 0
            ti.uname = ti.gname = ""
            t.addfile(ti)


def zstd(args: list[str]) -> None:
    subprocess.run(["zstd", "-q", *args], check=True)


def read(name: str) -> bytes:
    with open(name, "rb") as f:
        return f.read()


def write(name: str, data: bytes) -> None:
    with open(name, "wb") as f:
        f.write(data)


def main() -> None:
    small = {
        "game/README.txt": b"HatchZip zstd fixture\n" * 20,
        "game/data/random.bin": noise(SMALL, 1234),
        "game/data/text.txt": text(SMALL),
    }
    big = {
        "game/README.txt": b"zstd fixture\n" * 100,
        "game/random.bin": noise(BIG, 99),
        "game/text.txt": text(BIG),
    }
    tar("payload.tar", small, True)
    tar("big.tar", big, False)

    zstd(["-3", "-f", "-o", "zst-one.tar.zst", "payload.tar"])
    zstd(["-3", "--no-check", "-f", "-o", "zst-nocheck-one.tar.zst", "payload.tar"])

    # Streaming input keeps zstd from shrinking the window to the content
    # size, so these two declare a window the format must refuse.
    for log, out in [(27, "zst-long27.tar.zst"), (31, "zst-long31.tar.zst")]:
        with open("payload.tar", "rb") as src:
            with open(out, "wb") as dst:
                subprocess.run(
                    ["zstd", "-q", "-3", f"--zstd=wlog={log}", "-c"],
                    stdin=src,
                    stdout=dst,
                    check=True,
                )

    # Two frames, the split inside a file's data.
    tar_bytes = read("payload.tar")
    half = len(tar_bytes) // 2 - (len(tar_bytes) // 2) % 512
    write("half1", tar_bytes[:half])
    write("half2", tar_bytes[half:])
    zstd(["-3", "-f", "-o", "half1.zst", "half1"])
    zstd(["-3", "-f", "-o", "half2.zst", "half2"])
    write("zst-two.tar.zst", read("half1.zst") + read("half2.zst"))

    zstd(["-3", "--no-check", "-f", "-o", "half1n.zst", "half1"])
    zstd(["-3", "--no-check", "-f", "-o", "half2n.zst", "half2"])
    write(
        "zst-nocheck-two.tar.zst", read("half1n.zst") + read("half2n.zst")
    )

    # pzstd's 12-byte skippable frame: magic, size 4, "pzst".
    skip = struct.pack("<II", 0x184D2A50, 4) + b"pzst"
    write("zst-skip.tar.zst", read("half1.zst") + skip + read("half2.zst"))

    # A trailing skippable frame shaped like the seekable format's table.
    table = struct.pack("<I", 2) + struct.pack("<II", 0, 0) * 2
    write(
        "zst-seekable.tar.zst",
        read("zst-two.tar.zst") + struct.pack("<II", 0x184D2A5E, len(table)) + table,
    )

    # A trained dictionary, which the format refuses.
    subprocess.run(["mkdir", "-p", "train"], check=True)
    subprocess.run(
        f"split -b 4096 -d -a 3 payload.tar train/s", shell=True, check=True
    )
    zstd(["--train", *sorted(__import__("glob").glob("train/s*")), "-o", "dict"])
    zstd(["-3", "-D", "dict", "-f", "-o", "zst-dict.tar.zst", "payload.tar"])

    good = read("zst-one.tar.zst")
    bad = bytearray(good)
    bad[len(bad) // 2] ^= 0xFF
    write("zst-bad-check.tar.zst", bad)
    write("zst-truncated.tar.zst", good[:-8])
    bad_magic = bytearray(good)
    bad_magic[0] ^= 0xFF
    write("zst-bad-magic.tar.zst", bad_magic)

    write("notar.txt", b"this is not a tar archive, just one file\n")
    zstd(["-3", "-f", "-o", "zst-notar.tar.zst", "notar.txt"])

    # Several real frames with a skippable frame before every one, as pzstd
    # writes them; the second frame holds only the tar's end blocks.
    subprocess.run(
        ["pzstd", "-q", "-p", "4", "-f", "-o", "zst-pzstd.tar.zst", "big.tar"],
        check=True,
    )

    for name in ["payload.tar", "big.tar", "half1", "half2", "notar.txt"]:
        __import__("os").remove(name)
    for name in ["half1.zst", "half2.zst", "half1n.zst", "half2n.zst"]:
        __import__("os").remove(name)
    subprocess.run(["rm", "-rf", "train", "dict"], check=True)


if __name__ == "__main__":
    main()
