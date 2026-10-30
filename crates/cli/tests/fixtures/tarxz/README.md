# tar.xz fixtures

These archives were generated with XZ Utils 5.8.1 (liblzma 5.8.1) and
Python 3.13.5. `generate.py` constructs a deterministic USTAR archive,
including an empty directory, a short text file and a file containing
repeated pseudo-random bytes and x86 call instructions. The Rust tests
independently reconstruct the expected files and compare every byte.

Run `python generate.py` in this directory with `xz` on PATH, or set `XZ`
to the path of the xz executable. The generator passes the tar bytes on
stdin to these commands (`--stdout` is added to each):

| Fixture | xz arguments | Purpose |
|---|---|---|
| `default.tar.xz` | `-T2 -6` | 25 MiB input, two blocks with xz's default block size. |
| `multi.tar.xz` | `-T2 -6 --block-size=1MiB` | Five blocks with frequent resume points. |
| `single.tar.xz` | `-T1 -6` | One block, refused in place and extractable when kept. |
| `bcj.tar.xz` | `-T2 --block-size=1MiB --x86 --lzma2=preset=6` | Five x86 BCJ and LZMA2 blocks. |
| `delta.tar.xz` | `-T2 --block-size=1MiB --delta=dist=4 --lzma2=preset=6` | Five Delta and LZMA2 blocks. |

All use CRC64. `multistream.tar.xz` concatenates two copies of
`single.tar.xz` and is refused unchanged. `split.tar.xz.001` through `.003`
are consecutive equal-sized pieces of `multi.tar.xz`, except for the last
piece, which can be shorter. A volume boundary falls inside a compressed
block. The compressed and uncompressed sizes, block counts, checks, and
filter chains can be inspected with `xz -lvv`.

The 1 MiB block size is intentional for the crash tests. The compressed
fixtures are small, so on filesystems with large punch granules the tests
check resume correctness and preserved bytes but do not require every
individual crash to release disk space.
