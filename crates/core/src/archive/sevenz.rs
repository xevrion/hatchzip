//! 7z keeps its header at the end, like ZIP, and stores its packed data as
//! blocks in order. A solid block can only be decoded from its start, so the
//! unit is the whole block, and it is freed once all its files are out.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use sevenz_rust2::{
    Archive, ArchiveEntry, ArchiveWriter, Block, BlockDecoder, Password, SIGNATURE_HEADER_SIZE,
};

use super::storage::{Reader, Storage, Tail};
use super::{
    Format, Item, Kind, Opened, PasswordNeeded, Sink, Unit, Unsupported, Worker, WrongPassword,
    is_symlink_mode, safe_path,
};

/// Files with no data (directories, empty files) live only in the header.
/// They get a unit of their own that starts where the header does, so it's
/// done first and truncating it frees nothing that's still needed.
const NO_DATA: usize = usize::MAX;

const AES: &[u8] = &[0x06, 0xF1, 0x07, 0x01];

struct SevenZ {
    storage: Arc<Storage>,
    archive: Archive,
    password: Password,
}

pub fn open(
    storage: &Arc<Storage>,
    tail: Option<Arc<Tail>>,
    password: Option<&str>,
) -> Result<Opened> {
    use sevenz_rust2::Error;

    let resuming = tail.as_ref().map(|t| t.start);
    let mut reader = storage.reader(tail);
    let password = password.map_or_else(Password::empty, Password::new);
    let archive = Archive::read(&mut reader, &password).map_err(|e| match e {
        Error::PasswordRequired => anyhow::Error::new(PasswordNeeded),
        Error::BadSignature(_) | Error::UnsupportedVersion { .. } => {
            anyhow!("not a readable 7z archive: {e}")
        }
        // An encrypted header that won't parse is almost always the wrong
        // password; damage can't be told apart from it.
        _ if !password.is_empty() => anyhow::Error::new(WrongPassword),
        e => anyhow!("not a readable 7z archive: {e}"),
    })?;

    if archive.files.iter().any(|f| f.is_anti_item()) {
        bail!("this 7z archive contains deletion records (anti-items), which are not supported");
    }

    // Decoding finds an unknown method only when its block is reached, after
    // the blocks behind it were committed and their bytes freed. Refuse it
    // here instead, while the archive is still whole.
    refuse_unsupported_methods(&archive, resuming.is_none())?;

    let (starts, data_end) = block_starts(&archive)?;
    if resuming.is_none() {
        check_layout(&archive, &mut reader, data_end)?;
    }
    let tail_start = resuming.unwrap_or(data_end);

    let mut covered = vec![false; archive.files.len()];
    let mut units = Vec::with_capacity(archive.blocks.len() + 1);
    for (block, &start) in starts.iter().enumerate() {
        // Exactly the slice the decoder will walk, so items line up with
        // what it hands back.
        let first = archive.stream_map.block_first_file_index[block];
        let count = BlockDecoder::new(1, block, &archive, &password, &mut reader).entry_count();
        let files = archive
            .files
            .get(first..first + count)
            .ok_or_else(inconsistent)?;
        let end = starts.get(block + 1).copied().unwrap_or(data_end);
        if start == end {
            // No packed bytes, so the block starts where the next one does,
            // and committing it would truncate that block's data. All it can
            // hold is empty files, which are made like those of no block.
            if files.iter().any(|f| f.size() > 0) {
                return Err(inconsistent());
            }
            continue;
        }
        covered[first..first + count].fill(true);
        let items = files.iter().map(item).collect::<Result<_>>()?;
        let encrypted = archive.blocks[block]
            .coders
            .iter()
            .any(|c| c.encoder_method_id() == AES);
        units.push(Unit {
            start,
            key: block,
            items,
            encrypted,
        });
    }
    let empty: Vec<Item> = archive
        .files
        .iter()
        .zip(&covered)
        .filter(|(_, covered)| !**covered)
        .map(|(f, _)| item(f))
        .collect::<Result<_>>()?;
    if !empty.is_empty() {
        units.push(Unit {
            start: tail_start,
            key: NO_DATA,
            items: empty,
            encrypted: false,
        });
    }

    Ok(Opened {
        name: "7z",
        format: Box::new(SevenZ {
            storage: storage.clone(),
            archive,
            password,
        }),
        units,
        tail_start,
    })
}

fn inconsistent() -> anyhow::Error {
    anyhow!("this 7z archive's header is inconsistent")
}

/// Refuses a coder this build cannot decode, naming it. `fresh` says the
/// archive has not been touched yet, which a resumed run cannot claim.
fn refuse_unsupported_methods(archive: &Archive, fresh: bool) -> Result<()> {
    for coder in archive.blocks.iter().flat_map(|block| &block.coders) {
        let id = coder.encoder_method_id();
        if coder_supported(id) {
            continue;
        }
        return Err(Unsupported(unsupported_method(id, fresh)).into());
    }
    Ok(())
}

fn unsupported_method(id: &[u8], fresh: bool) -> anyhow::Error {
    let what = match sevenz_rust2::EncoderMethod::by_id(id) {
        Some(method) => format!("the {} compression method", method.name()),
        None => format!(
            "a compression method this build does not know ({})",
            id.iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    };
    // A resumed run already freed what it extracted, so only a fresh one can
    // say nothing was modified.
    let untouched = if fresh { " Nothing was modified" } else { "" };
    anyhow!("this 7z archive uses {what}, which this build does not decode.{untouched}")
}

/// The methods sevenz-rust2's decoder builds with the features this crate
/// turns on, kept in step with its `add_decoder` and `BlockDecoder`. Each
/// gated entry follows a `sevenz-*` feature in `Cargo.toml`.
fn coder_supported(id: &[u8]) -> bool {
    use sevenz_rust2::EncoderMethod as Method;
    if id == Method::ID_COPY
        || id == Method::ID_LZMA
        || id == Method::ID_LZMA2
        || id == Method::ID_DELTA
        || id == Method::ID_BCJ_X86
        || id == Method::ID_BCJ_PPC
        || id == Method::ID_BCJ_IA64
        || id == Method::ID_BCJ_ARM
        || id == Method::ID_BCJ_ARM64
        || id == Method::ID_BCJ_ARM_THUMB
        || id == Method::ID_BCJ_SPARC
        || id == Method::ID_BCJ_RISCV
        || id == Method::ID_BCJ2
    {
        return true;
    }
    #[cfg(feature = "sevenz-aes256")]
    if id == Method::ID_AES256_SHA256 {
        return true;
    }
    #[cfg(feature = "sevenz-bzip2")]
    if id == Method::ID_BZIP2 {
        return true;
    }
    #[cfg(feature = "sevenz-brotli")]
    if id == Method::ID_BROTLI {
        return true;
    }
    #[cfg(feature = "sevenz-deflate")]
    if id == Method::ID_DEFLATE {
        return true;
    }
    #[cfg(feature = "sevenz-lz4")]
    if id == Method::ID_LZ4 {
        return true;
    }
    #[cfg(feature = "sevenz-ppmd")]
    if id == Method::ID_PPMD {
        return true;
    }
    #[cfg(feature = "sevenz-zstd")]
    if id == Method::ID_ZSTD {
        return true;
    }
    false
}

/// Where each block's packed data begins, and where the packed data of the
/// last one ends. A block's data runs up to the start of the next.
fn block_starts(archive: &Archive) -> Result<(Vec<u64>, u64)> {
    let packed = SIGNATURE_HEADER_SIZE
        .checked_add(archive.pack_pos())
        .ok_or_else(inconsistent)?;
    let data_end = archive
        .pack_sizes()
        .iter()
        .try_fold(packed, |end, &size| end.checked_add(size))
        .ok_or_else(inconsistent)?;
    let offsets = archive.stream_map.pack_stream_offsets();
    let starts = archive
        .stream_map
        .block_first_pack_stream_index()
        .iter()
        .map(|&first| offsets.get(first).map(|o| packed + o))
        .collect::<Option<Vec<u64>>>()
        .filter(|s| s.len() == archive.blocks.len())
        .ok_or_else(inconsistent)?;
    Ok((starts, data_end))
}

/// Refuses layouts that extracting in place would break. A resumed run reads
/// the header again after blocks are truncated, so the header, and the
/// packed data of an encoded one, must lie in the tail saved before anything
/// is truncated. Packed streams that no block uses are refused as well: they
/// would go with the last block, and nothing checks what they hold.
fn check_layout(archive: &Archive, reader: &mut Reader, data_end: u64) -> Result<()> {
    let first_pack = archive.stream_map.block_first_pack_stream_index();
    let mut used = 0;
    for (block, &first) in archive.blocks.iter().zip(first_pack) {
        if first != used {
            return Err(inconsistent());
        }
        used += packed_streams(block);
    }
    if used > archive.pack_sizes().len() {
        return Err(inconsistent());
    }
    if used < archive.pack_sizes().len() {
        bail!(
            "this 7z archive holds packed data that no block uses, which cannot be extracted in \
             place safely. Nothing was modified"
        );
    }
    if header_starts(reader)?.into_iter().any(|at| at < data_end) {
        bail!(
            "the header of this 7z archive lies among its packed data instead of after it, so \
             extracting in place would destroy it before the extraction completes. Nothing was \
             modified"
        );
    }
    Ok(())
}

/// How many packed streams a block reads. sevenz-rust2 keeps the count to
/// itself, but every coder it decodes has one input and one output except
/// BCJ2, which has four inputs. All outputs but the last feed another coder.
fn packed_streams(block: &Block) -> usize {
    const BCJ2: &[u8] = &[0x03, 0x03, 0x01, 0x1B];
    let inputs: usize = block
        .coders
        .iter()
        .map(|c| if c.encoder_method_id() == BCJ2 { 4 } else { 1 })
        .sum();
    inputs + 1 - block.coders.len()
}

/// Where the header begins, and for an encoded header, where its packed
/// data begins, read from the start header the way sevenz-rust2 reads it.
fn header_starts(reader: &mut Reader) -> Result<Vec<u64>> {
    const ENCODED_HEADER: u8 = 0x17;
    const PACK_INFO: u8 = 0x06;
    let mut start = [0u8; SIGNATURE_HEADER_SIZE as usize];
    reader.seek(SeekFrom::Start(0))?;
    reader.read_exact(&mut start)?;
    if start[8..].iter().all(|&b| b == 0) {
        // sevenz-rust2 then searches the end of the file for anything that
        // parses as a header, which may well be inside a block.
        bail!(
            "the start header of this 7z archive is blank, as an interrupted write leaves it, so \
             it cannot be extracted in place safely. Nothing was modified"
        );
    }
    let offset = u64::from_le_bytes(start[12..20].try_into().expect("8 bytes"));
    let header = SIGNATURE_HEADER_SIZE
        .checked_add(offset)
        .ok_or_else(inconsistent)?;
    let mut found = vec![header];
    let mut kind = [0u8; 2];
    reader.seek(SeekFrom::Start(header))?;
    reader.read_exact(&mut kind)?;
    if kind == [ENCODED_HEADER, PACK_INFO] {
        let pos = number(reader)?;
        found.push(
            SIGNATURE_HEADER_SIZE
                .checked_add(pos)
                .ok_or_else(inconsistent)?,
        );
    }
    Ok(found)
}

/// A 7z variable-length number: the leading one bits of the first byte say
/// how many bytes follow, and its remaining bits are the highest.
fn number(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    let first = b[0];
    let mut value = 0;
    for i in 0..8 {
        let mask = 0x80u8 >> i;
        if first & mask == 0 {
            return Ok(value | (u64::from(first & (mask - 1)) << (8 * i)));
        }
        r.read_exact(&mut b)?;
        value |= u64::from(b[0]) << (8 * i);
    }
    Ok(value)
}

fn item(f: &ArchiveEntry) -> Result<Item> {
    let path = safe_path(f.name()).ok_or_else(|| {
        anyhow!(
            "'{}' would be written outside the destination folder; extraction refused",
            f.name()
        )
    })?;
    let mode = if f.has_windows_attributes {
        super::unix_mode_from_attributes(f.windows_attributes())
    } else {
        None
    };
    let kind = if f.is_directory() {
        Kind::Dir
    } else if is_symlink_mode(mode) {
        Kind::Symlink
    } else {
        Kind::File
    };
    let mtime = f
        .has_last_modified_date
        .then(|| SystemTime::from(f.last_modified_date()));
    Ok(Item {
        path,
        size: f.size(),
        kind,
        mode,
        mtime,
    })
}

impl Format for SevenZ {
    fn worker(&self, threads: u32) -> Result<Box<dyn Worker + '_>> {
        Ok(Box::new(SevenZWorker {
            fmt: self,
            reader: self.storage.reader(None),
            threads,
        }))
    }
}

struct SevenZWorker<'a> {
    fmt: &'a SevenZ,
    reader: Reader,
    threads: u32,
}

impl Worker for SevenZWorker<'_> {
    fn extract(&mut self, unit: &Unit, sink: &mut dyn Sink) -> Result<()> {
        if unit.key == NO_DATA {
            for item in &unit.items {
                sink.item(item, &mut io::empty())?;
            }
            return Ok(());
        }
        if unit.encrypted && self.fmt.password.is_empty() {
            return Err(PasswordNeeded.into());
        }
        let decoder = BlockDecoder::new(
            self.threads,
            unit.key,
            &self.fmt.archive,
            &self.fmt.password,
            &mut self.reader,
        );
        // The decoder's callback can only return its own error type, so the
        // sink's real error waits here and replaces the generic one.
        let mut failed = None;
        let mut items = unit.items.iter();
        decoder
            .for_each_entries(&mut |_entry, data: &mut dyn Read| {
                let item = items.next().expect("one item per block entry");
                match sink.item(item, data) {
                    Ok(()) => Ok(true),
                    Err(e) => {
                        failed = Some(e);
                        Ok(false)
                    }
                }
            })
            .map_err(|e| anyhow!("{e}"))
            .context("decoding a 7z block")?;
        match failed {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// A new header describing the blocks that start below `keep` and their
/// files, to be written at `keep`, and the start header that points at it.
///
/// sevenz-rust2's writer does the encoding. It is handed each kept block raw,
/// the way it copies blocks from one archive into another, but the copy goes
/// nowhere: the blocks already sit exactly where it would put them, right
/// after the start header. Only what it writes at `keep` and the start header
/// are kept, so nothing is read from the blocks and nothing is moved.
pub fn rebuild_header(storage: &Storage, tail: Arc<Tail>, keep: u64) -> Result<(Vec<u8>, Vec<u8>)> {
    let unusual = || {
        anyhow!(
            "the layout of this 7z archive cannot be restored. \
             Run extract to complete the extraction"
        )
    };
    let mut reader = storage.reader(Some(tail));
    let archive = Archive::read(&mut reader, &Password::empty())
        .map_err(|e| anyhow!("reading the saved 7z header: {e}"))?;
    // 7-Zip always starts the packed data right after the start header.
    // Anything else would leave a gap the writer doesn't know about.
    if archive.pack_pos() != 0 {
        return Err(unusual());
    }
    let (starts, data_end) = block_starts(&archive)?;
    let kept = starts.iter().take_while(|&&s| s < keep).count();
    if starts[kept..].iter().any(|&s| s < keep) {
        return Err(unusual());
    }
    // A block without packed bytes was never a unit of its own: `open` puts
    // its empty files with those of no block, which were done first.
    let kept: Vec<usize> = (0..kept)
        .filter(|&b| starts[b] < starts.get(b + 1).copied().unwrap_or(data_end))
        .collect();

    let encode = |e: sevenz_rust2::Error| anyhow!("writing the new 7z header: {e}");
    let mut w = ArchiveWriter::new(Sieve::new(keep)).map_err(encode)?;
    for &block in &kept {
        w.push_raw_block(&archive, block, &mut Nothing, |e| e.name().to_string())
            .map_err(encode)?;
    }
    // Files with no data that the engine counts as part of a kept block, as
    // in `open`. Ones outside every block were done first and are gone.
    for &block in &kept {
        let first = archive.stream_map.block_first_file_index[block];
        let count =
            BlockDecoder::new(1, block, &archive, &Password::empty(), &mut reader).entry_count();
        let files = archive
            .files
            .get(first..first + count)
            .ok_or_else(inconsistent)?;
        for f in files.iter().filter(|f| !f.has_stream) {
            w.push_archive_entry::<&[u8]>(f.clone(), None)
                .map_err(encode)?;
        }
    }
    let sieve = w.finish().context("writing the new 7z header")?;
    if Some(sieve.skipped) != keep.checked_sub(SIGNATURE_HEADER_SIZE) {
        return Err(unusual());
    }
    let start = sieve.start.ok_or_else(unusual)?;
    Ok((sieve.header, start))
}

/// Stands in for the archive as the source of raw blocks. The writer copies
/// from it into [`Sieve`], which drops those bytes, so what it reads doesn't
/// matter and it never touches the disk.
struct Nothing;

impl Read for Nothing {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Ok(buf.len())
    }
}

impl io::Seek for Nothing {
    fn seek(&mut self, to: io::SeekFrom) -> io::Result<u64> {
        match to {
            io::SeekFrom::Start(p) => Ok(p),
            _ => Err(io::Error::other("only absolute seeks are expected")),
        }
    }
}

/// Where the writer's output goes: block copies below `keep` are counted and
/// dropped, the start header and everything from `keep` on are kept. Any
/// other write means the writer's layout differs from the archive's.
struct Sieve {
    keep: u64,
    pos: u64,
    skipped: u64,
    start: Option<Vec<u8>>,
    header: Vec<u8>,
}

impl Sieve {
    fn new(keep: u64) -> Self {
        Self {
            keep,
            pos: 0,
            skipped: 0,
            start: None,
            header: Vec::new(),
        }
    }
}

impl io::Write for Sieve {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let end = self.pos + buf.len() as u64;
        if self.pos >= self.keep {
            let at = (self.pos - self.keep) as usize;
            if self.header.len() < at + buf.len() {
                self.header.resize(at + buf.len(), 0);
            }
            self.header[at..at + buf.len()].copy_from_slice(buf);
        } else if self.pos == 0 && end == SIGNATURE_HEADER_SIZE {
            self.start = Some(buf.to_vec());
        } else if self.pos >= SIGNATURE_HEADER_SIZE && end <= self.keep {
            self.skipped += buf.len() as u64;
        } else {
            return Err(io::Error::other(format!(
                "unexpected write of {} bytes at {}",
                buf.len(),
                self.pos
            )));
        }
        self.pos = end;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Seek for Sieve {
    fn seek(&mut self, to: io::SeekFrom) -> io::Result<u64> {
        self.pos = match to {
            io::SeekFrom::Start(p) => p,
            io::SeekFrom::Current(d) => self
                .pos
                .checked_add_signed(d)
                .ok_or_else(|| io::Error::other("bad seek"))?,
            io::SeekFrom::End(_) => return Err(io::Error::other("no end to seek from")),
        };
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn numbers_read_as_7z_writes_them() {
        let read = |bytes: &[u8]| number(&mut &bytes[..]).unwrap();
        assert_eq!(read(&[0x7F]), 0x7F);
        assert_eq!(read(&[0x81, 0x23]), 0x123);
        assert_eq!(read(&[0xC1, 0x23, 0x45]), 0x14523);
        assert_eq!(read(&[0xFF, 1, 2, 3, 4, 5, 6, 7, 8]), 0x0807060504030201);
    }

    /// The method is named when the decoder has a name for it, and its bytes
    /// when it doesn't, and only a fresh run claims nothing was modified.
    #[test]
    fn an_unsupported_method_says_what_and_whether_anything_was_touched() {
        let lzs = &[0x04, 0xF7, 0x11, 0x05];
        let fresh = format!("{:#}", unsupported_method(lzs, true));
        assert!(fresh.contains("the LZS compression method"), "{fresh}");
        assert!(fresh.contains("Nothing was modified"), "{fresh}");

        let resumed = format!("{:#}", unsupported_method(lzs, false));
        assert!(!resumed.contains("Nothing was modified"), "{resumed}");

        let unknown = format!("{:#}", unsupported_method(&[0x1F, 0x2E], true));
        assert!(unknown.contains("1F 2E"), "{unknown}");
    }

    /// sevenz-rust2 finds the header of an archive with a blank start header
    /// by searching the end of the file, where a block's data can pass for
    /// one, so where the header lies is unknown.
    #[test]
    fn a_blank_start_header_is_refused() {
        let dir = std::env::temp_dir().join(format!("hatchzip-blank-7z-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blank.7z");
        let mut bytes = b"7z\xBC\xAF\x27\x1C\x00\x04".to_vec();
        bytes.resize(64, 0);
        fs::write(&path, &bytes).unwrap();
        let storage = Storage::open(&path, false).unwrap();
        let err = header_starts(&mut storage.reader(None)).unwrap_err();
        assert!(err.to_string().contains("start header"), "{err:#}");
        drop(storage);
        fs::remove_dir_all(&dir).unwrap();
    }
}
