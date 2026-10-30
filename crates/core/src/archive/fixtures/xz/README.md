# xz fixtures

Made on macOS with the real `xz` CLI, version 5.8.4, and, for the check
variants (CRC32, none, SHA-256), Python 3's `lzma` module, which is the same
liblzma. Deterministic inputs: an 8 KiB pseudo-random payload for most files, and a repeating
0..255 pattern for `xz-uneven.xz`. The exact command lines were not
recorded. The table lists the properties `xz -lvv` reports today, which is
what the parser tests assert; if a fixture is ever regenerated, keep them.

| File | Streams | Blocks | Check | Filters | Uncompressed | Notes |
|---|---|---|---|---|---|---|
| `xz-t1.xz` | 1 | 1 | CRC64 | LZMA2, 8 MiB dictionary | 8,192 B | The `xz -T1` shape: the block header names no sizes. |
| `xz-multi.xz` | 1 | 2 | CRC64 | LZMA2, 8 MiB dictionary | 8,192 B | Two 4 KiB blocks, both headers naming their sizes. |
| `xz-uneven.xz` | 1 | 4 | CRC64 | LZMA2, 8 MiB dictionary | 29,491,200 B | The last block is smaller than the rest. It once sent every block to the wrong start when the index was walked forward. |
| `xz-crc32.xz` | 1 | 1 | CRC32 | LZMA2, 8 MiB dictionary | 8,192 B | |
| `xz-nocheck.xz` | 1 | 1 | None | LZMA2, 8 MiB dictionary | 8,192 B | |
| `xz-sha256.xz` | 1 | 1 | SHA-256 | LZMA2, 8 MiB dictionary | 8,192 B | The format refuses it before extraction. |
| `xz-bcj.xz` | 1 | 1 | CRC64 | x86 BCJ, then LZMA2, 8 MiB dictionary | 8,192 B | The payload starts with `MZ`, so the filter has something to translate. |
| `xz-delta.xz` | 1 | 1 | CRC64 | Delta (distance 4), then LZMA2, 8 MiB dictionary | 8,192 B | Same output bytes as `xz-bcj.xz`. |
| `xz-multistream.xz` | 2 | 1 each | CRC32 | LZMA2, 8 MiB dictionary | 16,384 B | Two concatenated streams, the `xz-crc32.xz` shape twice. Refused as concatenated streams. |

The damaged fixtures are byte-level variants of `xz-multi.xz`:

| File | Change |
|---|---|
| `xz-bad-magic.xz` | The last byte of the file, part of the stream footer magic, flipped (`0x5A` to `0xA5`). |
| `xz-bad-flags.xz` | The second stream flags byte changed, so the flags and their CRC-32 disagree and the flags are invalid. |
| `xz-bad-blockhdr.xz` | The first byte of the first block header (its size) changed. |
| `xz-truncated.xz` | The last 5 bytes missing. |

They exist so the parser is tested against each refusal separately rather
than against one generic error. `fixtures/xz` has no other consumers.
