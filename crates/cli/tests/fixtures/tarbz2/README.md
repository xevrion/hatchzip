# Bzip2 tar fixtures

Created with GNU tar 1.35 and bzip2 1.0.8. Rebuild from the repository root:

```powershell
$env:TAR_TOOL = 'C:/Users/Mahd2/Programming/Git/usr/bin/tar.exe'
$env:BZIP2_TOOL = 'C:/Users/Mahd2/Programming/Git/mingw64/bin/bzip2.exe'
python crates/cli/tests/fixtures/tarbz2/build.py
```

`build.py` creates deterministic file contents, then runs `tar --format=gnu --mtime=@1700000000 --owner=0 --group=0 --numeric-owner -cf - <sorted file names> game/saves` and `bzip2 -1 -c` or `bzip2 -9 -c`. `game/saves` is the empty directory expected by the shared extraction assertions. The level fixtures have multiple blocks. `multi.tar.bz2` concatenates an empty stream and three independently compressed tar slices, at tar offsets 0, 777 and 1500777, using levels 9, 1 and 9. Split sets are made by the integration test, as for gzip.

`corrupt-block.tar.bz2` flips bit 0 of byte 100 of the level 1 fixture. `bad-stream-crc.tar.bz2` flips the first combined CRC bit in its trailer. `not-tar.bz2` contains a plain file.

`false-magic.tar.bz2` keeps a real compressed GNU tar containing `game/empty.txt`, inserts an unused Huffman table, and adjusts selector numbers to leave the used tables unchanged. Valid length deltas in the unused table put a non-boundary block signature at bit 465; the real block begins at bit 32.

`early-false-magic.tar.bz2` has three blocks in one stream. Real bzip2 encodes the tar header and the trailing padding. A short middle block encodes three zero bytes for `game/early.bin`, with an in-use bitmap containing the decoy. The real blocks begin at bits 32, 764 and 1045. The decoy is at bit 870, 106 bits after the second block starts, one bit into its bitmap. The builder supplies valid block and combined CRCs. Both false-magic fixtures are checked byte for byte with Python's independent decoder and real `bzip2 -dc`; integration tests assert the exact positions and extracted output.
