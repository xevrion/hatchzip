# zstd fixtures

Made on macOS with the real `zstd` and `pzstd` 1.5.7 from
[facebook/zstd](https://github.com/facebook/zstd). `generate.py` rebuilds
them from deterministic payloads; run it from this folder with both CLIs on
PATH.

The small payload is a 260 KiB GNU tar holding three files
(`game/data/random.bin` and `game/data/text.txt` at 128 KiB each, a short
readme) and one empty folder. `zst-pzstd.tar.zst` holds a different, 8 MiB
tar so that pzstd writes more than one frame.

| File | Frames | Check | Window | Size | Notes |
|---|---|---|---|---|---|
| `zst-one.tar.zst` | 1 | XXH64 | 266,240 B (content size) | 163,992 B | What `zstd -3` writes for a file: one single-segment frame. Refused in place. |
| `zst-two.tar.zst` | 2 | XXH64 | 133,120 B each | 163,857 B | The tar split at a 512-byte boundary, both halves compressed: points at the frame start. |
| `zst-skip.tar.zst` | 2, plus 1 skip | XXH64 | 133,120 B each | 163,869 B | A 12-byte skippable frame (`pzstd`'s `50 2A 4D 18` header with `pzst`) between the frames. |
| `zst-seekable.tar.zst` | 2, plus 1 trailing skip | XXH64 | 133,120 B each | 163,885 B | A skippable frame after the last frame, shaped like the seekable format's table. |
| `zst-nocheck-one.tar.zst` | 1 | none | content size | 163,988 B | `zstd -3 --no-check`; refused in place. |
| `zst-nocheck-two.tar.zst` | 2 | none | 133,120 B each | 163,849 B | Both halves with `--no-check`; refused in place even with several frames. |
| `zst-long27.tar.zst` | 1 | XXH64 | 128 MiB | 163,989 B | Compressed from stdin with `--zstd=wlog=27`, so the header names a window larger than the content. |
| `zst-long31.tar.zst` | 1 | XXH64 | 2 GiB | 163,989 B | The same with `wlog=31`; the format refuses the window before anything is decoded. |
| `zst-dict.tar.zst` | 1 | XXH64 | content size | 84,185 B | Compressed with a trained dictionary (`-D`); the frame names it, and the format refuses it. |
| `zst-pzstd.tar.zst` | 2, plus 2 skips | XXH64 | 2 MiB | 5,169,704 B | `pzstd -p 4` on the 8 MiB tar: a skippable frame before every frame. |
| `zst-bad-check.tar.zst` | 1 | XXH64 | content size | 163,992 B | One byte flipped in the middle of `zst-one`; the checksum fails while decoding. |
| `zst-truncated.tar.zst` | 1 | XXH64 | content size | 163,984 B | The last 8 bytes of `zst-one` removed; the frame's extent runs past the file. |
| `zst-bad-magic.tar.zst` | 1 | XXH64 | content size | 163,992 B | The first byte of `zst-one` flipped; the format is no longer readable. |
| `zst-notar.tar.zst` | 1 | XXH64 | 41 B | 54 B | A plain text file compressed, not a tar; refused once the first frame decodes. |

The last four exist so each refusal is exercised separately: a checksum
failure, a frame that promises more bytes than the file has, a bad magic,
and a valid stream that does not hold a tar. `fixtures/zst` has no other
consumers; the CLI suite builds its own archives from the same crate.
