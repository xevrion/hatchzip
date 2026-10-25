# How it works

This document describes how HatchZip extracts an archive while giving its space back, and why each step happens in the order it does. Read it before changing anything in `engine.rs`, `journal.rs` or `archive/storage.rs` under `crates/core/src`.

## The space problem

A normal extractor reads the archive and writes the output, then the user deletes the archive. At the moment just before that deletion, the disk holds both: the full archive and the full output. For a 180 GB archive that unpacks to 200 GB, that moment needs 380 GB.

HatchZip deletes the archive gradually, in step with writing the output, so the disk never has to hold much more than the output alone.

## Why from the end

Files can be shortened from the end on every operating system and filesystem (`ftruncate`, `SetEndOfFile`). Removing bytes from the start or the middle of a file needs hole punching (`fallocate` on Linux, `FSCTL_SET_ZERO_DATA` on Windows, `F_PUNCHHOLE` on macOS), which FAT32 and exFAT do not support and which behaves differently on each system.

So HatchZip works from the end. Each format describes its archive as a list of **units**: byte ranges that decode on their own, and that can be truncated away once every file in them is safely on disk.

| Format | Unit | Layout |
|---|---|---|
| ZIP | One entry (local header plus data) | Entries in order, then the central directory, the index, at the very end. A self-extracting stub may come first. |
| 7z | One solid block, or one file in a non-solid archive | A 32-byte signature header, the packed blocks in order, then the header (the index) at the end. |
| tar | One entry, including any long-name or pax records before it | Header and data records in order, then zero blocks. There is no index. |
| `.tar.gz` | The whole archive, extracted from the front instead; see [Compressed tar](#compressed-tar) | One compressed stream holding a tar |
| RAR | One file, from its first header; a solid archive is one unit, extracted only while it is kept; see [RAR](#rar) | Headers and data in order, in one file or over volumes that are complete RAR files, a file split over several of them. There is no index. |

Units are extracted in reverse order of their position in the file. After each batch the archive is truncated to where the lowest unit of the batch begins.

A solid 7z block can only be decoded from its beginning, so it is freed only once all of it is extracted. That makes the largest unit the main cost of in-place extraction: the run needs free space for the output plus roughly the largest unit. 7-Zip's default settings can put a whole archive into one block, which leaves nothing to gain; the space check reports this before anything is touched.

For one large file the same cost is avoided where the filesystem can punch holes: see [Freeing space inside a large file](#freeing-space-inside-a-large-file).

Directories and empty files in 7z have no data at all, only header entries. They form one extra unit that starts where the header does, so it is done first and truncating it frees only the header, which is already saved.

## Split archives

A split archive (`game.7z.001`, `game.7z.002`, ...) as 7-Zip writes it is one archive cut into byte ranges. `Storage` (`crates/core/src/archive/storage.rs`) joins the volumes into one logical file. Truncating it to a length deletes every volume that lies wholly past that length and shortens the one it falls in. Volume boundaries need no special handling: a unit may span two volumes, and the volume past it is deleted once the unit is committed.

Any volume's name finds the whole set, and the journal records every volume with its original length so a resume can check that what is left is an untouched prefix.

## The saved tail

ZIP and 7z keep their index at the end, which is the first thing truncation removes. Before anything is truncated, HatchZip copies everything after the last unit's data (the "tail") into `<archive>.hatchzip-tail` and records its CRC-32 in the journal. For tar the tail is just the closing zero blocks.

From then on the archive is parsed through `storage::Reader`, which serves the live volumes below the tail offset and the saved tail above it, so the format library sees the archive exactly as it was. Regions already truncated away read as zeros. The zip crate scans backwards over them while looking for its end record, and zeros never match a signature; for tar, the zeros read like the end of the archive, so parsing what is left yields exactly the units still to do.

## The journal

`<archive>.hatchzip` is a small JSON file holding:

| Field | Meaning |
|---|---|
| `format` | `zip`, `7z`, `tar`, `tar.gz` or `rar`, so a resume never has to guess from a half-eaten file |
| `parts` | Every volume with its original length |
| `tail_start`, `tail_crc` | Where the saved tail belongs, and its checksum |
| `output` | The output folder, so a resume can't be pointed elsewhere |
| `total_size` | Total output size, for progress |
| `committed_len` | The archive length after the latest commit. Every unit starting at or past it is done. |
| `links` | Symlinks and hard links to create at the very end |
| `resume` | While a large file is streamed: its unit, how many bytes of it are flushed, the bit of the archive decoding restarts at, and the CRC-32 so far. For a compressed tar, also `stream`, the decoder's and the tar parser's state. See below. |
| `keep` | Present and true when the extraction keeps the archive; see [Keeping the archive](#keeping-the-archive) |

The journal is at version 3 since `resume` was added. Version 2 journals load as they are; older builds refuse version 3, which matters, because they would delete the half-written file whose archive bytes are already gone. `resume.stream` came later without a new version: only `tar.gz` journals have it, and builds that don't know that format refuse the journal before reading it. A journal with `keep` is version 4, and only such a journal is: builds from before `keep` refuse it, since they would resume it in place and truncate an archive the user asked to keep. Every other journal is still written as version 3, so an older build can finish it.

There is no separate "done" counter: units start at distinct offsets, so `committed_len` alone says which are finished. The journal is always replaced atomically: written to a temporary file, flushed, renamed over the old one, and the directory flushed.

## One batch

Units are processed in batches. Each batch goes through these steps, in this order:

1. **Choose the batch.** Starting from the next uncommitted unit, take units while their extracted size fits in the free space that exists right now, minus `--keep-free`. A batch holds at most 512 units and 2 GB.
2. **Extract in parallel.** Each unit is decoded by a worker thread, and every file in it is written to `<name>.hatchzip-part`. The decoders verify CRCs as each stream ends, and HatchZip checks the written size against the size the archive declares. Each finished part file is renamed to its real name. Links are not created yet, only recorded.
3. **Flush.** The batch is forced to the physical disk: one `syncfs` on Linux, an fsync of every file and directory elsewhere (`F_FULLFSYNC` on macOS).
4. **Commit.** The journal is updated with the new `committed_len` and the batch's links.
5. **Truncate.** The archive is shortened to `committed_len`, the start of the batch's lowest unit. The freed space becomes available for the next batch.

At the end, links are created, then the remaining archive (at most a self-extractor stub or the 7z signature header) is deleted, then the side files.

## Freeing space inside a large file

A unit only frees its archive bytes when it is committed, so one 30 GB file needs 30 GB free before any of its bytes can go. Where the archive's filesystem can punch holes (free bytes in the middle of a file without moving the rest), a unit holding one file of at least 512 MiB, stored or deflated in a ZIP or any regular file in a tar, is **streamed** instead, alone, when its output doesn't fit whole in the space free at that moment:

1. Its file is written to `<name>.hatchzip-part`. Every 256 MiB of output, at the next point where decoding can restart:
2. **Flush** the part file and the folders above it.
3. **Journal the resume point**: output length, the bit of the archive decoding restarts at, and the CRC-32 so far.
4. **Punch** the archive from the start of the file's data up to the byte holding that bit. The archive's length doesn't change, and the freed bytes read as zeros.

When the file is done it is renamed and committed through the ordinary steps 3 to 5, in the same journal write that clears `resume`. The local header or tar header in front of the data is never punched, so the archive still parses on resume.

Stored data can restart at any byte. A deflate stream can restart at any block boundary, the technique of zlib's `examples/zran.c`, using the C API of zlib-rs (`inflatePrime`, `inflateSetDictionary`, `Z_BLOCK`). A boundary may fall inside a byte, so the point is recorded in bits, and the decoder also needs the 32 KiB of output before the point, which back-references reach into. That window is not stored in the journal: it is the last 32 KiB of the part file, flushed before the point was journaled.

Hole punching is detected by punching a 1 MiB scratch file next to the archive and measuring that its allocation dropped, since a call can succeed without freeing anything. Without it (FAT32, exFAT, HFS+) every unit is extracted whole, as before. 7z solid blocks are not streamed yet.

| Interrupted | State on disk | What a resume does |
|---|---|---|
| Before the first point is journaled | A part file, nothing punched | Deletes it and starts the file over |
| After the flush, before the journal write | The part file holds output past the previous point, which is still the one in force | Cuts the part file back to the previous point and decodes from there; everything it needs is still in the archive |
| After the journal write, before or after the punch | The point is durable, the bytes before it may be freed | Cuts the part file to the point and decodes from it, never reading before it |
| After the rename, before the commit | The file has its real name, the journal still holds the last point | Renames it back and continues from that point |

The rule: **the archive is never punched past the last durable resume point**, and a point is only durable once the output it vouches for is flushed. A half-streamed file is never deleted as uncommitted output. `restore` refuses an archive whose journal holds a resume point, since an entry with holes in it can't be put back. The test suite kills the process at each of the three steps of a point at several points, for a stored ZIP entry, a deflated ZIP entry and a tar entry, and around the final commit (`crates/cli/tests/streaming.rs`); a unit test resumes a deflate stream from every block boundary at levels 6 and 9 (`crates/core/src/archive/resumable.rs`).

A deflate block can be as long as its encoder likes. zlib-ng and zlib-rs write level 1 as one block, which has no point to resume from but its start; such a file frees nothing until it ends, and if the space runs out first the run stops and says how much to free. zlib-rs at levels 6 and 9 wrote a block boundary about every 300 KB of output on the unit test's data, and the ZIP entries written by the zip crate and by Python's `zipfile` in the tests above had boundaries throughout; other encoders are not measured.

## Compressed tar

A `.tar.gz` is one deflate stream, or several gzip members back to back, holding the whole tar, and it can only be decoded from the front. So it is not split into units extracted from the end: the whole archive is one unit, extracted front to back, and the compressed bytes behind the decoder are freed by punching holes, at resume points as in the previous section. The archive keeps its length until the end, when the unit commits through the ordinary steps 3 to 5, truncating it to nothing, and the empty file is deleted.

Before anything is touched, HatchZip decodes the whole stream once (`targz::open`). That lists the entries for the checks in [Rejected before starting](#rejected-before-starting), verifies every gzip member's CRC-32 and length against its trailer, and measures what the space check needs. A damaged archive is refused here, unchanged. The extraction then decodes it a second time. This pass reads the whole archive, so once it has run for half a second the command line shows a progress bar and the GUI the share read ("Reading archive… 37%"). Measured on an Intel Core i7-14700HX with the archive in the page cache: 4.8 s for 488.6 MB holding 1 GiB of word-like text, about 225 MB of output a second; 0.8 s for 1.07 GB holding 1 GiB of random data; 2.1 s for 23.4 MB holding 20 GiB of zeros. The GUI opens an archive once to list it and again to extract it, so there the pass runs twice before the extraction starts. A resumed run makes the same pass from its resume point, since the bytes before it may be gone, so damage found on resume also stops the run before it changes anything.

A resume point can be any deflate block boundary or the start of any gzip member, except inside an entry's header records. Every 256 MiB of output, at the next such point:

1. **Flush** the file being written, every file finished since the last point, and their folders: one `syncfs` on Linux; elsewhere each file is flushed as it is finished, and the folders at the point.
2. **Journal the point** in `resume`: the bit decoding restarts at, and in `resume.stream` whether that is a member start, the CRC-32 and length of the member's output so far, the member's last 32 KiB of output (the deflate window), the offset in the tar, and where in the tar the point falls: between entries, inside a file's data (`resume.written` says how much of it is written), inside padding or data that extracts to nothing, or past the end marker. Links found since the last point go into the same write. The window spans tar headers and several files, so unlike a single ZIP entry's it can't be read back from one output file; it is stored in the journal, as 64 KiB of hex.
3. **Punch** the archive from where this run last punched up to the byte holding that bit. A run's first punch starts at 0, in case the last run stopped between steps 2 and 3.

| Interrupted | State on disk | What a resume does |
|---|---|---|
| Before the first point is journaled | Some output, nothing punched | Deletes that output, all of it still in the archive, and starts over |
| After the flush, before the journal write | Output past the point in force, which is the previous one | Deletes the files that come after that point, cuts the file the point is inside back to its recorded length, and decodes from the point |
| After the journal write, before or after the punch | The new point is durable | The same, from the new point, never reading before it |
| Around the final commit | As for every batch | As for every batch |

Without hole punching (FAT32, exFAT) the stream is extracted the same way and resume points are still journaled, so a crash still resumes from the last one, but nothing is freed until the end: the space check asks for room for every file next to the whole archive, and `check` says why. When the output is on another disk, nothing is punched either, since it would not help there.

`restore` refuses a `.tar.gz` whose journal holds a resume point: some of the stream may be released, and a compressed tar can't be rebuilt from what is left. Before the first point nothing was released, so restore removes the partial output and the journal and leaves the original archive.

The space check counts, for the stream, the highest the files written so far ever run ahead of the compressed bytes consumed, over every point the scan met; plus one step; plus the most file output the scan saw between two consecutive points, since a step can only end at a point; plus a read buffer and a granule at each end of the punched range; never more than all the files. Links are made at the end, once the archive is gone. The scan meets exactly the points the extraction will, since both decode from the same place with the same buffer sizes.

Measured on an 820 MiB tmpfs holding a 385.9 MB `.tar.gz` made by GNU tar 1.35 and gzip 1.13 (`-6`) from 501.1 MB in 2,016 entries (seven 50 MB random files, six 25 MB text files, 2,000 small JSON files), with 473.9 MB free: GNU `tar -xzf` ran out of space. `hatchzip check` asked for 400.2 MB plus the 5 MB kept free; during the extraction the lowest free space, sampled every 10 ms, was 111.8 MB, a peak of 362.1 MB. It finished in 1.0 s with 351.6 MB free, as the check said, and every file's SHA-256 matched. With punching disabled (`HATCHZIP_NO_PUNCH`, debug build) the check asks for 508.2 MB and refuses on that disk; on a 1000 MiB tmpfs that run peaked at 508.2 MB.

pax global headers, which `git archive` writes at the start of every tarball, are skipped, in plain tar as well.

`crates/cli/tests/targz.rs` kills the process at each of the three steps of a point at several points, repeatedly within one run, around the final commit, in multi-member and split archives and in archives from GNU tar, gzip and pigz, and checks after each crash that nothing past the journaled point was touched and, where `HATCHZIP_EXPECT_PUNCH` is set, that the space before it was given back. Unit tests in `resumable.rs` resume a multi-member gzip stream from every point it reports and decode it with input buffers down to one byte.

## RAR

RAR support is the `rar` cargo feature, on in the release builds and off by default; see [the format notes](formats.md#dependency-licenses). A build without it refuses a RAR archive before anything is touched.

A RAR archive that is not solid stores its files one after another, each header followed by the file's data, like tar. A volume set (`game.part1.rar`, `game.part2.rar`, ... or `game.rar`, `game.r00`, ...) is not one archive cut into byte ranges, as a `.7z.001` set is: each volume is a complete RAR file with its own headers, and a file that doesn't fit is split over consecutive volumes, its header repeated in each piece. `Storage` joins the volumes as it does any split set, and every file is a unit that starts at its first header. So the archive is truncated behind each committed batch exactly as tar is, and a volume is deleted once the truncation point falls before it. A split file belongs to the volume it starts in, so the volumes holding its later pieces stay until all of it is committed. What is left is always a prefix of the set whose headers are untouched, and the last volume left simply ends after a file, without its end of archive header, which UnRAR reads like any other.

`archive/rar.rs` reads the headers of every volume itself (RAR 5 and RAR 4, with their checksums) before anything is touched, as tar does. Nothing needs to be saved first: there is no central index and every volume carries its own headers, so the saved tail is empty. A resumed run reads the headers of what is left, up to where the last commit cut it, and finds exactly the units still to do. Before the first run starts, UnRAR lists the set too and every file's name is compared with the one read here, since extraction finds each file by the name UnRAR reports; a name UnRAR reads differently is refused then, not halfway through. Names are read in full, past the 1023 characters of UnRAR's fixed buffer, and a `\` in a RAR 5 name, where it is part of the name rather than a separator, is written as `_` on every system, as WinRAR does on Windows.

UnRAR is driven through its C interface and does the decoding only. It finds a file only by reading every header before it, so a batch of RAR units is decoded on one thread, lowest file first, by one archive handle that walks forward from the volume the lowest file starts in (the format says so with `Format::serial`). A helper thread runs UnRAR and pushes each file's data through a bounded channel to the reader the engine's sinks take. Only one archive handle is open at a time, since UnRAR keeps state in globals and is not documented as safe to use from several threads at once, and the handle is closed before the batch commits, so nothing holds a volume open while it is truncated or deleted.

A stored file that lies whole in one volume is plain bytes at a known offset, so it is streamed like a tar entry ([above](#freeing-space-inside-a-large-file)), with its CRC-32 checked. Inside a compressed file there is no resume point: UnRAR can't save its decoder's state.

A solid archive compresses every file together with the ones before it, so it decodes only front to back in one pass. It could only be freed once all of it is out, and since UnRAR can't save its state, a crash would have to start over with part of the archive possibly gone. So it is not extracted in place: it is one unit holding every file, extracted only while the archive is kept (`--keep-archive`), and refused in place before anything is touched, with a message that says so.

Passwords are proven before anything is touched, as for ZIP. A RAR 5 file carries a 64-bit check value derived from its password with PBKDF2-HMAC-SHA256; HatchZip derives it from the given password and compares, for every distinct salt in the archive, without decoding anything. RAR 4 has no check value: a wrong password only shows in the CRC-32 at the end of a file, so the smallest encrypted file with data is decoded whole into nothing, and the command line says so when that file is 64 MiB or more.

An archive made with "Encrypt file names" (`rar -hp`) encrypts its headers as well, so nothing of it can be read without the password, not even its list: opening it without one asks for it, as for a 7z with an encrypted header. `archive/rar/crypt.rs` decrypts the headers as UnRAR does (`crypt3.cpp`, `crypt5.cpp`, `ReadHeader15` and `ReadHeader50` in `arcread.cpp`), and the same parser reads them:

- RAR 5 puts an archive encryption header right after the signature, in the clear: a salt, the log2 of an iteration count, and usually a password check value. Every header after it is preceded by a 16-byte IV and encrypted with AES-256-CBC, under the key PBKDF2-HMAC-SHA256 derives from the password and the salt. A wrong password fails the check value at once; without one, it fails the checksum of the first header.
- RAR 4 says so with a flag in its main header, which stays in the clear. Every header after it is preceded by an 8-byte salt and encrypted with AES-128-CBC; the key and IV come from RAR 3's derivation, 2^18 rounds of SHA-1 over the password (in UTF-16) and that salt. That SHA-1 keeps a quirk of RAR 2.9, which rewrites the input buffer of a whole 64-byte block it hashes, so a password of 29 characters or more only works if it is reproduced. Keys are kept by salt, and RAR 6.24 writes the same salt in front of every header of a set, so the derivation runs once. A wrong password fails the checksum of the first encrypted header; so does damage to it, which is then reported as a wrong password, as UnRAR does. Once one header has passed, a header that fails is damage.

Either way, a header is padded to a whole number of 16-byte blocks. Every offset is the real one on disk: a file starts at the IV or salt in front of its header, which is where a truncation cuts, and its data starts after the padding. The volumes of a set must agree on whether their headers are encrypted, as UnRAR requires. UnRAR gets the password for its own listing too, so every name is still compared with what it reads. The file data of such an archive is encrypted as well, and proven as above.

`restore` refuses an archive with encrypted file names: it takes no password, and without one it can neither list what is left nor encrypt the end of archive header it would append. Such an extraction can only be completed, with the password.

Refused before anything is touched: symbolic links, hard links and file copies, dictionaries over 4 GB (UnRAR refuses them by default, and decoding one needs that much memory), compression versions UnRAR doesn't know, a set with a volume missing or out of order, a stray volume after the last one (the last says no more follow), and any header that fails its checksum, as damage (exit 4).

`restore` appends an end of archive header, saying no volume follows, to the last volume left, in that volume's format; everything before it is untouched files with their headers. Archives with encrypted file names are refused, as above.

Measured on a 192 MiB tmpfs holding a 146.5 MB RAR 5 set made by RAR 6.24 (`-m3 -v40m`, four volumes) from 190.2 MB in 512 files (seven 20 MB random files, five 10 MB text files, 500 small JSON files), with 54.8 MB free: UnRAR 6.24 stopped with a write error (exit 5). `hatchzip check` asked for 45.6 MB plus the 5 MB kept free, where freeing a whole volume at a time asked for 129.4 MB; during the extraction the lowest free space, sampled every 10 ms, was 5.3 MB, a peak of 49.5 MB. It finished in 0.61 s with 9.3 MB free, as the check said, and every file's SHA-256 matched. The same files as one 146.4 MB `.rar` on the same disk: the check asked for 45.7 MB, where a single volume freed as a whole asked for 192.1 MB; the peak was 43.8 MB, in 0.45 s, every file matching.

`crates/cli/tests/rar.rs` extracts sets made by RAR 6.24 (RAR 5 and RAR 4, with passwords, with encrypted file names in one volume and in several, solid, and with quick open data, a recovery record and a comment) and archives written by the test itself (stored RAR 5 and RAR 4, single files and sets, hostile names, files over many volumes, an 8 MB stored file). It kills the process at both crash points after every number of commits a run makes, for RAR 5 and RAR 4, single files and sets; checks after each commit that what is on disk is exactly what the journal says is left and that it shrank; kills a streamed stored file at each step of a resume point and checks its space was given back; and checks that solid archives, wrong and missing passwords, hostile names, missing and stray volumes and damaged headers are refused with every volume intact. `crates/cli/tests/no_rar.rs` checks the refusal of a build without the feature.

## Crash safety

Each step can be interrupted. The order above makes every interruption recoverable:

| Interrupted during | State on disk | What a resume does |
|---|---|---|
| Choosing or extracting | Some part files or finished files exist, the journal doesn't list them, the archive is intact | Deletes those files (the output folder started empty, so they are HatchZip's) and extracts the batch again |
| Flushing | Same as above | Same as above |
| Committing | The atomic rename either happened or not | Either case above or below |
| Truncating | The journal lists the batch, the archive is still longer than `committed_len` | Truncates the archive, continues with the next batch |
| Creating links at the end | All units committed | Recreates every link, replacing any that exist. A link that can't be made (a folder in its place, which only an extraction started by a release without the checks in [Rejected before starting](#rejected-before-starting) can lead to, or a link on its way) stops the run with every file extracted; the error lists each such link and why, and the journal keeps them. Once what is in the way is moved, a resume makes them and finishes. Nothing is ever created through a link. |
| Deleting the archive and side files | The archive is gone, the journal is still there | Recreates the links and removes the side files |

The rule behind the table: **the journal never claims a file before it is durable, and the archive is never truncated past a unit before the journal claims it.**

The test suites kill the process at steps 3 and 5 at many positions for every format, including split archives with volume boundaries inside entries, and check that the resumed run produces identical output (`crates/cli/tests/extract.rs`, `crates/cli/tests/formats.rs`).

## Keeping the archive

`--keep-archive` (in the app, clearing "Remove the archive during extraction"; in the right-click menu, "Extract here (keep archive)") is a normal extraction through the same engine: the same units, batches, part files, checks, flushes and journal commits, so path safety, collisions, CRCs, passwords and resuming work exactly as above. What changes:

- **Nothing in the archive changes.** The journal records `keep`, and from the moment it is written the archive's `Storage` is protected: truncating, punching, writing and deleting all refuse, so no code path can change a kept archive even by mistake. A commit (step 4) still advances `committed_len`, but only as a record of progress; step 5 is skipped. Nothing is streamed or punched, as for an output on another disk. At the end, links are made and only the journal and the saved tail are deleted.
- **The space check counts every file.** The replay runs as if the output were on another disk: each unit adds its files and frees nothing, plus the saved tail. That is the whole output, rounded up to 4 KiB per file, plus the archive's index, and it is what `check --keep-archive` reports. When it does not fit but extracting in place would, `check` and the app say what extracting in place requires. When planning in place, they mention when keeping the archive would fit too.
- **Resuming keeps keeping.** A resumed run follows the journal, whatever it is asked: an extraction that keeps the archive goes on keeping it, and one that started in place cannot switch to keeping it, since part of the archive is already gone. That refusal happens before the archive is opened for writing, so it doesn't even finish a truncation the last run left pending. On resume the archive must still have its original length, or the run stops without reading it.
- **Restore discards.** `restore` on a kept extraction has no index to rebuild: it deletes what a crashed batch left half written, as a resume would, then the journal and the saved tail. Files already extracted stay. The app shows no "Restore archive" for such a run.

The interruption table above holds with one row fewer: a crash after the journal write leaves nothing pending. `crates/cli/tests/keep.rs` extracts ZIP, 7z (solid and not), tar, `.tar.gz` and split ZIP, 7z and `.tar.gz` sets while keeping them, checks that every volume is byte for byte unchanged, then extracts the same archive in place and compares both outputs with the source. It kills kept runs at both commit points and, for a `.tar.gz`, at each step of a resume point, resumes them without `--keep-archive`, and checks the archive is unchanged after the crash and after the resume. `HATCHZIP_FREE` (debug builds only) sets the free space so the suite can put a disk between the two plans.

## Restore

`hatchzip restore` turns a half-extracted archive back into a normal one, for when an extraction should stop for good. What is left of the archive is the first `committed_len` bytes of the original, and those hold exactly the units not yet extracted, untouched. Only the index is missing, so restore appends a new one after them. Nothing below `committed_len` moves, so it needs only the free space the new index takes. Files already extracted stay in the output folder.

| Format | What restore writes |
|---|---|
| ZIP | A central directory holding the saved records of the entries left, copied as they are (their local header offsets still point at the same bytes), then the end records, with ZIP64 records when the original had them or the counts or offsets need them. A self-extractor stub stays in front, and the offsets keep whatever convention the original used. |
| 7z | A new header for the blocks left, which are the first ones, right after the signature header, and their files. It is encoded by sevenz-rust2's writer, which is handed each block raw and writes nothing for it, since the block already sits where the writer would put it. The signature header is rewritten to point at the new header. |
| tar | Entries for hard links already cut off whose targets are still in the archive, then the two zero blocks that end a tar archive. |
| RAR | An end of archive header in the last volume left, in its format (RAR 5 or RAR 4), saying no volume follows. |

The steps, in order:

1. Undo any earlier restore that was interrupted (below), and finish a truncation a crashed run left pending, so the archive is exactly `committed_len` bytes.
2. Build the new index in memory. Anything restore refuses, it refuses here, before writing.
3. Delete output a crashed batch wrote but never committed, as a resume does, since those entries are still in the archive. Create the links the journal holds for entries already cut off.
4. Save the journal with a `restoring` field holding the bytes step 6 will overwrite.
5. Append the new index at `committed_len` and flush it.
6. Overwrite the bytes below `committed_len` that change (only the 7z signature header) and flush them.
7. Open the result as a fresh archive with the same parsers extraction uses and check that it lists exactly the entries left. If it doesn't, undo and stop.
8. Delete the journal, then the saved tail.

Any run that opens an archive whose journal has `restoring` set, `restore` and `extract` alike, first cuts the archive back to `committed_len`, including a volume the new index made longer than it was, and puts back the bytes the field holds. So an interrupted restore can be run again, or the extraction continued instead. The journal goes before the tail because a journal without its tail can't be loaded; a tail left behind by a crash between the two is never read again. The test suite kills restore after steps 5 and 6 for every format and checks both ways on (`crates/cli/tests/restore.rs`).

## The space check

Before anything is written, HatchZip replays the whole extraction one unit at a time, counting what each step adds (the unit's files, each rounded up to a 4 KiB block) and what it frees (the unit's span in the archive), plus the saved tail written at the start. The highest point of that running total is the free space the run needs. HatchZip refuses to start unless the disk has that much free plus `--keep-free`.

A unit that can be streamed counts at most one step of output (256 MiB) plus a 1 MiB read buffer, plus what the filesystem may keep allocated at each end of a punched range (64 KiB, 1 MiB on NTFS, 16 MiB on ZFS, 128 MiB on btrfs, whose extents are freed whole), plus how far its output outgrows its compressed data, never more than the whole unit. For deflate that last part is bounded by its value at the end, since encoders fall back to stored blocks rather than expand data, with a 1/1024 margin for block headers. Measured on a 950 MiB tmpfs holding a ZIP with one 600 MiB entry and three small ones: stored, the check asked for 269.6 MB where the previous release asked for 629.1 MB and refused, and the lowest free space seen while it ran (sampled every 10 ms) was 98.5 MB of 367.0 MB, a peak of 268.5 MB; deflated (mostly incompressible data, 498.1 MB archive), it asked for 401.3 MB against 629.1 MB, and the peak was 324.4 MB. Both finished with the file byte-identical.

The real run never needs more than the check promised: it uses larger batches only when there is room for them, since each batch is sized against the free space measured at that moment. If something else on the machine fills the disk during the run, HatchZip stops before the next unit, saves its progress, and says how much space to free.

When the output folder is on a different disk from the archive, freeing the archive does not help the output disk, and the check says so.

## Rejected before starting

These are refused while the archive is still untouched:

- Encrypted entries or blocks (not supported yet).
- Paths that resolve outside the output folder (`..`, absolute ZIP paths, drive letters). Leading slashes in tar names are dropped, as tar itself does.
- Two entries with the same path, ignoring case. On a case-insensitive filesystem they would race inside a parallel batch. For ZIP the names are read from the central directory itself, since the zip library keeps only the last of several entries with one name.
- Entries that can't all be created where they say, ignoring case: a file or link at a path where another entry needs a folder (`d` as a symlink next to `d/inner.txt`), which includes any entry inside a link, and a tar hard link to anything but a file in the archive. Links are made at the very end, once the archive is gone, so such an archive would otherwise fail only after it was consumed.
- ZIP entries whose bytes overlap other entries. Overlapping archives are a known zip bomb technique, and truncating one entry's span would destroy another's data.
- tar entry types HatchZip can't create safely: device files, FIFOs, sparse files.
- 7z anti-items (deletions recorded by archive updates).
- In a `.tar.gz`: damage anywhere in the stream, a gzip member whose CRC-32 or length doesn't match, data after the end of the tar or after the gzip stream other than zeros (it would be lost with the archive), and header records over 1 MiB.
- Gzip files that don't hold a tar and compressed tar other than gzip (`.tar.zst`, `.tar.xz`, `.tar.bz2`).
- In RAR: solid archives (unless the archive is kept), links, dictionaries over 4 GB, a volume missing or a stray one after the last, names UnRAR reads differently, a wrong password, and headers that fail their checksum (see [RAR](#rar)). Without the `rar` feature, every RAR archive.
- A non-empty output folder, so that a resume can safely delete anything it finds for uncommitted units.

## Operating systems

Everything that differs between systems lives in `crates/core/src/platform/`, one file per system:

| | Linux | macOS | Windows |
|---|---|---|---|
| Flushing a batch | One `syncfs` | `F_FULLFSYNC` per file | `FlushFileBuffers` per file |
| Directory flush | `fsync` on the directory | `fsync` on the directory | Not possible; NTFS journals renames |
| Same disk check | Device ID | Device ID | Volume prefix of the canonical path |
| Unix permissions | Restored, without setuid, setgid and sticky | Same | Ignored |
| Symlinks | Created | Created | Written as a small text file holding the target, as Git does without developer mode |
| Hard links | Created, or copied where the filesystem has none (FAT32, exFAT) | Same | Same |
