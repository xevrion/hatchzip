# Format notes

Research for the formats after ZIP, checked against crate sources and against real archives made with zstd 1.5.7, xz 5.8.2, 7-Zip 25.01, gzip and pigz. Anything marked unverified has not been tested.

## The general problem

ZIP is easy because every entry decompresses on its own. Solid formats compress everything as one stream, so the archive must be read from the front, and the consumed front must be freed with hole punching. If HatchZip crashes after punching, the decoder state that was needed to continue is gone with it.

The answer is a **resume point**: a saved state from which decoding can restart at a known compressed offset. HatchZip may only punch up to the last durable resume point. A resume point records:

- the compressed offset
- the decoder state
- the uncompressed offset
- the tar parser state (current entry, bytes left in it)
- the length of each open output file
- the running checksum state

The protocol: flush the outputs, write and flush the journal, then punch up to the point just made durable. **Done** for single files stored or deflated in ZIP and for tar entries ([How it works](how-it-works.md#freeing-space-inside-a-large-file)); the deflate decoder state is the bit position plus the last 32 KiB of output, read back from the flushed output file instead of stored in the journal. **Done** for `.tar.gz` as well ([How it works](how-it-works.md#compressed-tar)), where the 32 KiB window spans tar headers and several files, so it is stored in the journal.

## Per format

| Format | Restart granularity available | Crate | Work needed |
|---|---|---|---|
| 7z | **Done** (`crates/core/src/archive/sevenz.rs`). Per folder (solid block), truncating from the end like ZIP. Inside a folder, only at LZMA2 full-reset chunks. | `sevenz-rust2` (Apache-2.0): `BlockDecoder` decodes one folder from our own `Read + Seek`; pack positions and sizes are public. | Freeing space inside one solid block is the remaining work; 7-Zip's defaults can put a whole archive in one block. |
| Split archives | **Done** (`crates/core/src/archive/storage.rs`) for byte splits. | n/a | `.7z.001` and `.zip.001` are byte splits of one archive. Native `.z01` ZIP, which keeps the directory in the last part and numbers disks in its offsets, is not done. |
| tar | **Done** (`crates/core/src/archive/tar.rs`) for uncompressed tar, per entry. | `tar` (MIT or Apache) | |
| `.tar.gz`, `.tgz` | **Done** (`crates/core/src/archive/targz.rs`, gzip framing in `resumable.rs`). Any deflate block boundary, plus the start of every gzip member. Multi-member files (`bgzip`, `pigz -i`, concatenated `.gz`) work; every member's CRC-32 and ISIZE are checked. | `libz-rs-sys` (Zlib license) exports `inflatePrime`, `inflateSetDictionary`, `inflateGetDictionary` and `Z_BLOCK`. The safe `zlib-rs` API does not expose them. | Done. Tested with archives from GNU tar 1.35 with gzip 1.13 (levels 1 and 6), pigz with and without `-i`, and flate2. |
| `.tar.xz` | Per xz block. xz 5.8.2 defaults to multithreaded mode, which writes many blocks; `-T1` writes one. See [below](#tarzst-and-tarxz). | `lzma-rust2` (Apache-2.0, pure Rust, already a dependency through `zip`): start a fresh `Lzma2Reader` per block. | Medium for multi-block files. Large for single-block files. |
| `.tar.zst` | Per frame only. The `zstd` CLI writes one frame even with `-T0`, so typical files have no restart point. See [below](#tarzst-and-tarxz). | `ruzstd` (MIT) keeps exactly the needed state in `DecoderScratch` and can stop at block boundaries, but it is crate-private. | Medium for multi-frame files (`pzstd`). Large for the usual single frame. |
| RAR | **Done** (`crates/core/src/archive/rar.rs`) per file, for archives that are not solid, behind the `rar` cargo feature. Files follow one another with their headers inline, as in tar, so the archive is truncated behind each committed file and a volume goes once every file reaching into it is out. The headers are read in Rust, and decrypted there for archives with encrypted file names (`-hp`); UnRAR only decodes. A stored file that lies whole in one volume is streamed like a tar entry, with resume points anywhere. Inside a compressed file there is no resume point: UnRAR can't save its decoder's state. | `unrar-ng-sys` 0.7.7 (MIT or Apache) builds UnRAR 7.21 beta 1 from source (its `version.hpp`), under its own freeware license (see below). It replaced `unrar_sys` 0.5.8 (UnRAR 7.01), whose structs are declared `repr(C)` while UnRAR's are packed, so every field after the first pointer sat at the wrong offset. The safe `unrar` crates read a file into memory or onto disk and have no data callback, so the engine would need a second copy of every file. `rars` (Apache-2.0, pure Rust, takes `Read + Seek`) is new and its own README asks for more testing. | Solid RAR is extracted only while the archive is kept. Links are not done. `restore` refuses archives with encrypted file names: it takes no password, which they need to be listed and to get a new, encrypted end of archive header. |

## `.tar.zst` and `.tar.xz`

Research only; nothing is implemented. Both fit the engine `.tar.gz` uses: one solid unit, extracted front to back by `targz::Walker`, which already keeps the tar parser state separate from the decoder. What each needs is a decoder that can stop at a point, describe that point in a few fields, and start again from it in a new process. Where the format has independent pieces, the point is just an offset.

Measured on a 503,070,720-byte tar (the dataset from [How it works](how-it-works.md#compressed-tar): seven 50 MB random files, six 25 MB text files, 2,000 small JSON files):

| Command | Independent pieces | Window a mid-piece restart needs |
|---|---|---|
| `zstd -3` (the default level), `zstd -3 -T0`, `zstd -19 -T0`, `zstd --long=27 -T0` (zstd 1.5.7) | 1 frame each | 2 MiB at `-3`, 8 MiB at `-19`, 128 MiB with `--long=27` |
| `pzstd -p 4` | 60 frames (plus 60 skippable frames) | none at frame starts |
| `xz -6` (xz 5.8.2, multithreaded by default) | 20 blocks of 24 MiB (3 times the 8 MiB dictionary) | none at block starts |
| `xz -6 -T1` | 1 block | 8 MiB dictionary plus the LZMA state |

**`.tar.xz`: feasible, and the multi-block case is medium work.** An xz block decodes on its own: it starts with a fresh LZMA2 state and dictionary, and the index at the end of the stream lists every block's compressed and uncompressed size. So:

1. Before anything is touched, read the stream footer and the index (they are at the end, so this costs no decoding), and check the index against the blocks as the scan walks them.
2. A resume point is the start of a block: its compressed offset, its offset in the tar, and the tar parser state, exactly as for gzip member starts. No window is stored.
3. Decode each block with a fresh `Lzma2Reader` (and a BCJ filter if the block header lists one), check the block's CRC-32, CRC-64 or SHA-256 at its end, and the index and stream footer at the end of the stream.

The default 24 MiB blocks give a candidate point every 24 MiB of output, far more often than the 256 MiB step needs. Since blocks are independent, the next few could also be decoded on other cores while one is written, as `lzma-rust2`'s own `XzReaderMt` does.

A single-block file (`xz -T1`) has no restart point except its start. Restarting inside a block means saving the LZMA2 decoder at a chunk boundary: the dictionary (8 MiB at `-6`, 64 MiB at `-9`), the probability tables, the range coder and the state machine. The dictionary would go to a side file written before each journal update, 8 or 64 MiB per 256 MiB step. `lzma-rust2` keeps that state private, so this needs a fork. Without it, such a file can still be extracted in place, but frees nothing until it ends, like a deflate stream written as one block; the space check would say so, as it does for `.tar.gz`.

**`.tar.zst`: feasible only with a decoder that saves its state; large work for the usual single frame.** A zstd frame decodes only from its start, and the CLI writes one frame however many threads it uses. Inside a frame, blocks are at most 128 KiB, and a block boundary is a valid restart point if the decoder can be given back:

- the window: the last 2 MiB to 128 MiB of output, depending on the frame header, which is far too large for the journal and spans tar headers and several files, so it would go to a side file at each point;
- the entropy state carried between blocks: the previous Huffman table for treeless literals, the three FSE tables for repeat mode, and the three repeat offsets, a few KiB;
- the XXH64 state of the frame checksum, under 100 bytes.

libzstd keeps all of this in an opaque `ZSTD_DCtx` with no way to save or restore it. `ruzstd` holds exactly these pieces in `DecoderScratch`, but it is crate-private, so this needs a ruzstd fork with save and load; its decoding speed against libzstd is not measured. Files with several frames, from `pzstd` or the seekable format, are the easy case: every frame start is a free restart point needing no window, the same as a gzip member, and the `zstd` crate already in the tree can decode frame by frame. A first version could support those and extract single-frame files without freeing anything until the end.

For both formats the `Walker`'s tar half can be reused unchanged; the decoder half (`resumable::Gzip` today) would become a trait with `read`, `restart_bit` and `restart_point`, and the journal's `StreamPoint` would gain a field for the decoder kind.

## Dependency licenses

All candidate crates are permissive: zip (MIT), flate2 (MIT or Apache), zlib-rs and libz-rs-sys (Zlib), zstd and zstd-safe (BSD-3), ruzstd (MIT), lzma-rust2 (Apache-2.0), liblzma (MIT or Apache, building only the 0BSD liblzma sources), sevenz-rust2 (Apache-2.0), zesven (MIT or Apache), tar (MIT or Apache). No GPL-only, LGPL-only or non-commercial terms appear in any dependency tree.

The exception is UnRAR. The `unrar-ng-sys` crate is MIT or Apache, but the bundled C++ code is under the UnRAR freeware license: it may be used to extract RAR archives in any software free of charge, but not to build a RAR-compatible archiver. Distributing it requires the full text of that license, which is in `LICENSE-UNRAR`. This reading is not legal advice.

So UnRAR is optional: the `rar` cargo feature of `hatchzip-core`, `hatchzip` and `hatchzip-gui` compiles it in, and it is off by default. `cargo install hatchzip`, crates.io and distribution builds stay open source throughout, need no C++ compiler, and refuse a RAR archive before touching it, naming the release builds. The release workflow turns the feature on for every target and puts `LICENSE-UNRAR` in every download: the `.tar.gz` and `.zip` archives, the Windows setup, the `.dmg` (inside the app's `Resources`), the `.deb` and `.rpm`, and the AppImage. CI tests both builds.

`xz2` and `liblzma` can't be in the same build, since both declare `links = "lzma"`.

## Hole punching per platform

| System | Call | Notes |
|---|---|---|
| Linux | `fallocate(FALLOC_FL_PUNCH_HOLE \| FALLOC_FL_KEEP_SIZE)` | ext4, XFS, btrfs, tmpfs. Verified on ext4: punching 32 MiB of a 64 MiB file dropped its usage from 65M to 33M; punching an unaligned 4,000,008 bytes of an 8 MiB file through `platform::punch_hole` dropped its allocation from 8,392,704 to 4,399,104 bytes (tmpfs: 8,388,608 to 4,395,008). |
| macOS | `fcntl(F_PUNCHHOLE)` with `fpunchhole_t` | Offset and length must be multiples of the filesystem block size, so HatchZip shrinks the range inwards. Can return ENOSPC on cloned files, which HatchZip treats as unsupported. Compiles; unverified whether APFS frees the space. |
| Windows | `FSCTL_SET_SPARSE`, then `FSCTL_SET_ZERO_DATA` | Without marking the file sparse first, NTFS writes real zeros and allocates the range. HatchZip aligns ranges inwards to 64 KiB and counts 1 MiB per end in the space check. Compiles; unverified on real NTFS. The probe measures with `GetCompressedFileSizeW`. |
| FAT32, exFAT | None | Only truncation from the end is possible. |
