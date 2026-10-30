//! A `.xz`-compressed tar holds its whole tar in one stream, and like gzip
//! a stream is decodable only from the front. Unlike gzip, that stream is a
//! sequence of blocks that share nothing: every block decodes entirely from
//! its own bytes. So the walk reads one block at a time, and a point
//! between two blocks is a resume point that needs nothing from the output
//! before it. The engine frees the blocks behind the decoder by punching
//! them away at those points (see `Run::stream_solid` in the engine).
//!
//! [`crate::archive::xz`] parses the container: every block's header, where
//! its data and check lie, and the filters it declares. The walk builds the
//! filter chain each header names over a source bounded to the block's own
//! compressed bytes, verifies every block's check as it completes, and
//! refuses a block that does not decode before a file claims more of it. A
//! resumed run starts at its resume point's block: the blocks before it
//! were freed, and the container is parsed again without revalidating the
//! headers those freed blocks write on, which no longer exist.
//!
//! A stream has to hold more than one block for any of that to matter: a
//! single-threaded run of xz (`xz -T1`, or writing on one core) puts the
//! whole archive in one block. Nothing can be freed while one block is
//! being decoded, so such an archive is refused for in-place extraction,
//! with `--keep-archive` still working: it decodes as usual, and the
//! archive is left whole anyway. The tar parser itself is the one both
//! compressed tars share, see `targz`.

use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use lzma_rust2::Lzma2Reader;
use lzma_rust2::filter::{bcj::BcjReader, delta::DeltaReader};

use super::storage::Storage;
use super::targz::{self, CodecKind, Event, Feed, Profile, StreamPoint, TarAt, TarStream};
use super::xz::{Block, Check, Filter, Xz};
use super::{
    Format, Item, Kind, Opened, Sink, SolidStream, SolidWalker, Unit, Unsupported, Worker,
};
use crate::journal::Resume;

pub const NAME: &str = "tar.xz";

/// Decoded output is handed out in pieces of at most this much, as targz
/// does.
const BUF: usize = targz::BUF;

/// The largest LZMA2 dictionary a block may name. It is allocated whole,
/// and xz's presets top out at 256 MiB, so this only refuses archives
/// built to exhaust the extractor's memory.
const MAX_DICT: u32 = 1 << 30;

const CRC64_POLY: u64 = 0xC96C_5795_D787_0F42;

const fn make_crc64_table() -> [u64; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u64;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ CRC64_POLY
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

const CRC64_TABLE: [u64; 256] = make_crc64_table();

/// CRC-64/XZ, what most xz archives check their blocks with. lzma-rust2
/// keeps its own private.
#[derive(Clone, Copy)]
struct Crc64 {
    state: u64,
}

impl Crc64 {
    fn new() -> Self {
        Self { state: u64::MAX }
    }

    fn update(&mut self, data: &[u8]) {
        for &byte in data {
            let index = ((self.state ^ u64::from(byte)) & 0xFF) as usize;
            self.state = (self.state >> 8) ^ CRC64_TABLE[index];
        }
    }

    fn finalize(self) -> u64 {
        self.state ^ u64::MAX
    }
}

/// The check a block runs against, as bytes little-endian, which is how
/// the container stores them (spec 2.1.1.2).
enum Running {
    Crc32(crc32fast::Hasher),
    Crc64(Crc64),
    None,
}

impl Running {
    fn new(check: Check) -> Self {
        match check {
            Check::Crc32 => Running::Crc32(crc32fast::Hasher::new()),
            Check::Crc64 => Running::Crc64(Crc64::new()),
            Check::None | Check::Sha256 | Check::Other { .. } => Running::None,
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Running::Crc32(h) => h.update(data),
            Running::Crc64(h) => h.update(data),
            Running::None => {}
        }
    }

    fn finish(self) -> Vec<u8> {
        match self {
            Running::Crc32(h) => h.finalize().to_le_bytes().to_vec(),
            Running::Crc64(h) => h.finalize().to_le_bytes().to_vec(),
            Running::None => Vec::new(),
        }
    }
}

/// What refuses a multi-stream, unchecked or otherwise unsupported xz
/// archive before anything is touched, except a single-block stream and a
/// stream whose blocks carry no check: both decode fine, and are refused
/// only for in-place extraction (see [`TarXz`]'s `Format` impl).
fn policy(xz: &Xz) -> Result<()> {
    let [stream] = xz.streams.as_slice() else {
        bail!(
            "the archive holds {} xz streams one after another, which is not supported. \
             Extract them as separate archives",
            xz.streams.len()
        );
    };
    match stream.check {
        Check::None | Check::Crc32 | Check::Crc64 => {}
        Check::Sha256 => bail!(
            "the archive checks every xz block with SHA-256, which is not supported. \
             xz's default --check=crc64, or --check=crc32, works"
        ),
        Check::Other { .. } => bail!(
            "the archive checks its xz blocks with a check of an unknown kind, which \
             is not supported"
        ),
    }
    for b in &stream.blocks {
        for f in &b.filters {
            match *f {
                Filter::Unknown { id } => {
                    bail!(
                        "a block of the archive uses the xz filter {id:#x}, which is not \
                         supported"
                    );
                }
                Filter::Lzma2 { dict_size } if dict_size > MAX_DICT => bail!(
                    "a block of the archive names an LZMA2 dictionary beyond 1 GiB, \
                     which is not supported"
                ),
                _ => {}
            }
        }
    }
    Ok(())
}

/// The container with what the format refuses about it, and a resumed
/// run's walk.
pub struct Solid {
    storage: Arc<Storage>,
    xz: Xz,
    /// The point the scan began at: a resumed run's, whose earlier blocks
    /// were freed, or `None`.
    from: Option<Resume>,
    profile: Profile,
}

impl Solid {
    fn new(storage: Arc<Storage>, from: Option<&Resume>) -> Result<Self> {
        let from_byte = from.map_or(0, |r| r.bit / 8);
        let xz = {
            let mut reader = storage.reader(None);
            Xz::parse_resumed(&mut reader, storage.original_len(), from_byte)
                .context("reading the xz stream's structure")?
        };
        // A resume reads the same container, so it meets the same refusals;
        // a journal that names a check or filter this build cannot verify
        // fails here rather than halfway into a block.
        policy(&xz).map_err(|e| anyhow::Error::new(super::Unsupported(e)))?;
        Ok(Self {
            storage,
            xz,
            from: from.cloned(),
            profile: Profile::default(),
        })
    }

    /// Walks the whole of what `from` leaves, or the whole archive.
    pub fn walk(&self, from: Option<&Resume>) -> Result<Walker> {
        Walker::new(self, from)
    }
}

/// Walks the archive from `from`, or from its start, listing what is left
/// and measuring it. Damage anywhere is found here, before anything is
/// touched.
pub fn open(storage: &Arc<Storage>, from: Option<&Resume>) -> Result<Opened> {
    let name = storage
        .primary()
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    storage.expect_reads(storage.original_len() - from.map_or(0, |r| r.bit / 8));
    let read = (|| -> Result<(Solid, Vec<Item>, Profile)> {
        let solid = Solid::new(storage.clone(), from)?;
        let (items, profile) = super::scan(&solid, from)?;
        Ok((solid, items, profile))
    })();
    // Unsupported is not damage: the archive is valid, this build just does
    // not extract it. A parse error that says so (an oversized index) keeps
    // the same class; everything else that stops the read is damage.
    let (mut solid, items, profile) = read
        .map_err(|e| {
            if e.is::<Unsupported>()
                || e.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == io::ErrorKind::Unsupported)
            {
                e
            } else {
                anyhow::Error::new(super::Damaged(e))
            }
        })
        .with_context(|| format!("checking {name} (nothing was changed)"))?;
    solid.profile = profile;
    Ok(Opened {
        name: NAME,
        format: Box::new(TarXz { solid }),
        // Even with no entries left there is one unit, so that a resume point
        // past the last entry still belongs to something.
        units: vec![Unit {
            start: 0,
            key: 0,
            items,
            encrypted: false,
        }],
        // Nothing is saved: the stream is never truncated from the end.
        tail_start: storage.original_len(),
    })
}

struct TarXz {
    solid: Solid,
}

impl Format for TarXz {
    fn worker(&self, _threads: u32) -> Result<Box<dyn Worker + '_>> {
        Ok(Box::new(TarXzWorker { solid: &self.solid }))
    }

    fn solid(&self) -> Option<&dyn SolidStream> {
        Some(&self.solid)
    }

    fn refuses_in_place(&self) -> Option<String> {
        let stream = self.solid.xz.streams.first()?;
        let whole = stream.blocks.len() < 2;
        let unchecked = stream.check == Check::None;
        Some(match (whole, unchecked) {
            (true, true) => String::from(
                "the archive's whole tar is inside one xz block and its blocks carry no \
                 check, so nothing of it can be freed as it is extracted",
            ),
            (true, false) => String::from(
                "the archive's whole tar is inside one xz block, so nothing of the archive \
                 can be freed as it is extracted",
            ),
            (false, true) => String::from(
                "the archive's xz blocks carry no check, so damage inside them could not be \
                 detected before the bytes behind are freed",
            ),
            (false, false) => return None,
        })
    }
}

impl SolidStream for Solid {
    fn profile(&self) -> &Profile {
        &self.profile
    }

    fn walk(&self, from: Option<&Resume>) -> Result<Box<dyn SolidWalker>> {
        Ok(Box::new(self.walk(from)?))
    }
}

/// The compressed bytes of one block and nothing else: a decoder that lies
/// about how much of the block it has decoded cannot read past them into
/// the blocks that follow.
struct Bytes {
    reader: super::storage::Reader,
    left: u64,
}

impl Read for Bytes {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let cap = self.left.min(buf.len() as u64) as usize;
        if cap == 0 {
            return Ok(0);
        }
        let n = self.reader.read(&mut buf[..cap])?;
        if n == 0 && self.left > 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the archive is shorter than its blocks say",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// The filter chain of one block, over its bounded source. The chain ends
/// in LZMA2 (the container guarantees it), so the build goes backwards from
/// the header's list: the LZMA2 decoder sits on the source and every size
/// preserving filter the header names wraps it, until the last one,
/// which is the first the writer applied.
struct Stage(Box<dyn Read>);

impl Stage {
    fn build(b: &Block, storage: &Arc<Storage>) -> Result<Stage> {
        let mut r = storage.reader(None);
        // The reader starts at the file; the block owns its own slice of it.
        r.seek(SeekFrom::Start(b.data_off))
            .with_context(|| "reading the block's compressed data")?;
        Stage::over(
            Bytes {
                reader: r,
                left: b.comp_len,
            },
            b,
        )
    }

    /// The chain over any source holding exactly the block's compressed
    /// bytes. Tests decode a fixture's block from memory through it.
    fn over(source: impl Read + 'static, b: &Block) -> Result<Stage> {
        let mut chain: Box<dyn Read> = Box::new(source);
        for f in b.filters.iter().rev() {
            chain = match *f {
                Filter::Lzma2 { dict_size } => {
                    if dict_size > MAX_DICT {
                        bail!(
                            "a block of the archive names an LZMA2 dictionary beyond 1 GiB, \
                             which is not supported"
                        );
                    }
                    // No block borrows from the blocks before it, so no back
                    // reference reaches past its own output: the declared
                    // dictionary is an upper bound, and a block of a few KiB
                    // does not need (or deserve) a gigabyte of it.
                    let dict =
                        dict_size.min(b.uncomp_len.clamp(4 * 1024, u64::from(u32::MAX)) as u32);
                    Box::new(Lzma2Reader::new(chain, dict, None))
                }
                Filter::Delta { dist } => Box::new(DeltaReader::new(chain, dist as usize)),
                Filter::Bcj { arch, start } => Box::new(bcjer(arch, chain, start as usize)?),
                Filter::Unknown { .. } => {
                    bail!("a block of the archive uses an xz filter that is not supported")
                }
            };
        }
        Ok(Stage(chain))
    }

    /// Reads decoded bytes; 0 means the block's stream ended.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Read::read(self.0.as_mut(), buf)
    }
}

fn bcjer(arch: u64, inner: Box<dyn Read>, start: usize) -> Result<BcjReader<Box<dyn Read>>> {
    Ok(match arch {
        0x04 => BcjReader::new_x86(inner, start),
        0x05 => BcjReader::new_ppc(inner, start),
        0x06 => BcjReader::new_ia64(inner, start),
        0x07 => BcjReader::new_arm(inner, start),
        0x08 => BcjReader::new_arm_thumb(inner, start),
        0x09 => BcjReader::new_sparc(inner, start),
        0x0A => BcjReader::new_arm64(inner, start),
        0x0B => BcjReader::new_riscv(inner, start),
        _ => bail!("{arch:#x} is not a BCJ filter architecture, and it parses as one"),
    })
}

/// The xz side of a walk: it decodes the stream one block at a time into
/// the tar parser, and verifies each block's check where the container
/// says it lies.
struct XzFeed {
    blocks: Vec<Block>,
    check: Check,
    storage: Arc<Storage>,
    /// The chain of the block being decoded, `None` between blocks.
    stage: Option<Stage>,
    /// Which block `stage` decodes, or the next one to build. Past the last
    /// block once the stream has ended.
    block: usize,
    /// Uncompressed bytes of the block `stage` is decoding, still to hand out.
    left: u64,
    /// The block's check, as its bytes come out of the chain.
    running: Running,
    /// Every block of the stream has been decoded.
    ended: bool,
}

impl XzFeed {
    fn new(storage: Arc<Storage>, check: Check, blocks: Vec<Block>, block: usize) -> Self {
        Self {
            blocks,
            check,
            storage,
            stage: None,
            block,
            left: 0,
            running: Running::None,
            ended: false,
        }
    }

    /// The block's uncompressed size is what its header says, so reaching
    /// it means the whole block decoded. Its check is how a corrupt decode
    /// gets caught, before anything can be worth the bytes it claims.
    fn finish_block(&mut self) -> Result<()> {
        let computed = std::mem::replace(&mut self.running, Running::None).finish();
        match self.check {
            // Nothing to compare. The format refuses in-place extraction of
            // an unchecked stream, so only a kept run gets here.
            Check::None => {}
            Check::Crc32 | Check::Crc64 => {
                let b = &self.blocks[self.block];
                let stored = {
                    let mut r = self.storage.reader(None);
                    r.seek(SeekFrom::Start(b.check_off)).with_context(|| {
                        format!(
                            "reading the check of the archive's block {}",
                            self.block + 1
                        )
                    })?;
                    let mut out = vec![0u8; self.check.size() as usize];
                    r.read_exact(&mut out).with_context(|| {
                        format!(
                            "reading the check of the archive's block {}",
                            self.block + 1
                        )
                    })?;
                    out
                };
                if stored != computed {
                    let name = if self.check == Check::Crc32 {
                        "CRC-32"
                    } else {
                        "CRC-64"
                    };
                    bail!(
                        "the archive's block {} fails its {name} check; it is damaged",
                        self.block + 1
                    );
                }
            }
            // The policy refuses these before a byte is decoded; reaching
            // one means the container changed under the run.
            Check::Sha256 | Check::Other { .. } => bail!(
                "the archive's blocks are checked with a kind HatchZip does not verify, \
                 which is not supported"
            ),
        }
        self.stage = None;
        self.block += 1;
        self.ended = self.block == self.blocks.len();
        Ok(())
    }
}

impl Feed for XzFeed {
    fn fill(&mut self, buf: &mut [u8]) -> Result<usize> {
        if self.stage.is_none() {
            let Some(b) = self.blocks.get(self.block) else {
                self.ended = true;
                return Ok(0);
            };
            anyhow::ensure!(
                b.validated,
                "the journal's resume point names a block this archive no longer holds \
                 whole, and will not be modified"
            );
            self.stage = Some(Stage::build(b, &self.storage)?);
            self.left = b.uncomp_len;
            self.running = Running::new(self.check);
        }
        let want = self.left.min(buf.len() as u64) as usize;
        let n = self
            .stage
            .as_mut()
            .expect("the block's chain was just built")
            .read(&mut buf[..want])
            .with_context(|| format!("decoding the archive's block {}", self.block + 1))?;
        if n == 0 {
            bail!(
                "the archive's block {} decodes {} bytes short of what its container says; \
                 it is damaged",
                self.block + 1,
                self.left
            );
        }
        self.left -= n as u64;
        self.running.update(&buf[..n]);
        if self.left == 0 {
            self.finish_block()?;
        }
        Ok(n)
    }

    fn restartable(&self) -> bool {
        // A block's end is the only place the stream restarts from, and it
        // was just verified: the next block's header is intact.
        self.stage.is_none() && !self.ended
    }

    fn point(&self, tar_pos: u64, at: TarAt, written: u64) -> Result<Resume> {
        let b = self
            .blocks
            .get(self.block)
            .ok_or_else(|| anyhow!("the stream has no block a resume point could follow"))?;
        Ok(Resume {
            unit: 0,
            written,
            // Points are at block headers, always whole bytes.
            bit: b.header_off * 8,
            crc: 0,
            stream: Some(Box::new(StreamPoint {
                kind: CodecKind::Xz,
                member_start: false,
                member_crc: 0,
                member_len: 0,
                window: Vec::new(),
                block: self.block as u64,
                tar_pos,
                at,
            })),
        })
    }

    fn point_byte(&self) -> Option<u64> {
        if self.ended {
            None
        } else {
            self.blocks.get(self.block).map(|b| b.header_off)
        }
    }
}

/// Walks the archive from `from`, or from its start, listing what is left
/// and measuring it. Damage anywhere is found here, before anything is
/// touched.
pub struct Walker {
    inner: TarStream<XzFeed>,
}

impl Walker {
    fn new(solid: &Solid, from: Option<&Resume>) -> Result<Self> {
        let stream = solid
            .xz
            .streams
            .first()
            .ok_or_else(|| anyhow!("the archive holds no xz stream"))?;
        let feed = match from {
            None => XzFeed::new(
                solid.storage.clone(),
                stream.check,
                stream.blocks.clone(),
                0,
            ),
            Some(r) => {
                let s = r.stream.as_ref().ok_or_else(|| {
                    anyhow!("the journal's resume point does not belong to a compressed tar")
                })?;
                if s.kind != CodecKind::Xz {
                    bail!(
                        "the journal's resume point belongs to {} tar, not this archive's",
                        match s.kind {
                            CodecKind::Gzip => "a gzip member of a",
                            CodecKind::Xz => "an xz block of a",
                        }
                    );
                }
                let Ok(block) = usize::try_from(s.block) else {
                    bail!("the journal's resume point names a block the format has none of");
                };
                let Some(b) = stream.blocks.get(block) else {
                    bail!(
                        "the journal's resume point names block {}, which the archive's \
                         xz stream does not hold",
                        block + 1
                    );
                };
                if !b.validated || b.header_off != r.bit / 8 {
                    bail!(
                        "the journal's resume point is inconsistent; the archive will \
                         not be modified"
                    );
                }
                if !matches!(s.at, TarAt::File { .. }) && r.written != 0 {
                    bail!(
                        "the journal's resume point claims a file is open where none is; \
                         the archive will not be modified"
                    );
                }
                XzFeed::new(
                    solid.storage.clone(),
                    stream.check,
                    stream.blocks.clone(),
                    block,
                )
            }
        };
        Ok(Self {
            inner: TarStream::new(feed, from)?,
        })
    }

    fn next(&mut self, max: usize) -> Result<Event<'_>> {
        self.inner.next(max)
    }

    fn point(&self) -> Result<Resume> {
        self.inner.point()
    }

    fn point_byte(&self) -> Option<u64> {
        self.inner.point_byte()
    }
}

impl SolidWalker for Walker {
    fn next(&mut self, max: usize) -> Result<Event<'_>> {
        self.next(max)
    }

    fn point(&self) -> Result<Resume> {
        self.point()
    }

    fn point_byte(&self) -> Option<u64> {
        self.point_byte()
    }
}

/// Decodes the whole stream in one go, for `test` and `--verify-first`. The
/// extraction itself goes through the engine's streaming path instead.
struct TarXzWorker<'a> {
    solid: &'a Solid,
}

impl Worker for TarXzWorker<'_> {
    fn extract(&mut self, _unit: &Unit, sink: &mut dyn Sink) -> Result<()> {
        let mut w = self.solid.walk(self.solid.from.as_ref())?;
        loop {
            match w.next(BUF)? {
                Event::Entry { item, done, .. } if item.kind == Kind::File => {
                    // A resumed run's open file: only its rest is here.
                    let left = Item {
                        size: item.size - done,
                        ..item
                    };
                    let mut data = FileData {
                        walker: &mut w,
                        ended: false,
                    };
                    sink.item(&left, &mut data)?;
                    let mut rest = [0u8; 4096];
                    while data.read(&mut rest)? > 0 {}
                }
                Event::Entry { item, link, .. } => {
                    sink.item(&item, &mut Cursor::new(link.unwrap_or_default()))?;
                }
                Event::Data(_) | Event::FileEnd | Event::Point => {}
                Event::End => return Ok(()),
            }
        }
    }
}

/// One file's data, read through the walker.
struct FileData<'a> {
    walker: &'a mut Walker,
    ended: bool,
}

impl Read for FileData<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while !self.ended && !buf.is_empty() {
            match self.walker.next(buf.len()) {
                Ok(Event::Data(d)) => {
                    buf[..d.len()].copy_from_slice(d);
                    return Ok(d.len());
                }
                Ok(Event::Point) => {}
                Ok(Event::FileEnd) => self.ended = true,
                Ok(_) => return Err(io::Error::other("the tar entry ended unexpectedly")),
                Err(e) => return Err(io::Error::other(format!("{e:#}"))),
            }
        }
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// The fixtures come from the real xz CLI; see fixtures/xz/.
    fn fixture(name: &str) -> &'static [u8] {
        match name {
            "t1" => include_bytes!("fixtures/xz/xz-t1.xz"),
            "multi" => include_bytes!("fixtures/xz/xz-multi.xz"),
            "uneven" => include_bytes!("fixtures/xz/xz-uneven.xz"),
            "crc32" => include_bytes!("fixtures/xz/xz-crc32.xz"),
            "nocheck" => include_bytes!("fixtures/xz/xz-nocheck.xz"),
            "sha256" => include_bytes!("fixtures/xz/xz-sha256.xz"),
            "bcj" => include_bytes!("fixtures/xz/xz-bcj.xz"),
            "delta" => include_bytes!("fixtures/xz/xz-delta.xz"),
            "multistream" => include_bytes!("fixtures/xz/xz-multistream.xz"),
            _ => unreachable!(),
        }
    }

    fn parse(name: &str) -> Xz {
        let bytes = fixture(name);
        Xz::parse(&mut Cursor::new(bytes), bytes.len() as u64).unwrap()
    }

    /// Decodes every block of a fixture through the same chain the walk
    /// builds, and checks each block against the bytes the real xz CLI
    /// stored at the offset the index records. Returns the output.
    fn decode(name: &str) -> Vec<u8> {
        let bytes = fixture(name);
        let xz = parse(name);
        let stream = &xz.streams[0];
        let mut out = Vec::new();
        for block in &stream.blocks {
            let from = block.data_off as usize;
            let to = from + block.comp_len as usize;
            let mut stage = Stage::over(Cursor::new(bytes[from..to].to_vec()), block).unwrap();
            let mut running = Running::new(stream.check);
            let mut left = block.uncomp_len;
            let mut buf = vec![0u8; 64 * 1024];
            while left > 0 {
                let want = (left as usize).min(buf.len());
                let n = stage.read(&mut buf[..want]).unwrap();
                assert!(n > 0, "block ended early");
                running.update(&buf[..n]);
                out.extend_from_slice(&buf[..n]);
                left -= n as u64;
            }
            if stream.check != Check::None {
                let at = block.check_off as usize;
                let stored = &bytes[at..at + stream.check.size() as usize];
                assert_eq!(running.finish(), stored, "block check");
            }
        }
        out
    }

    #[test]
    fn crc64_matches_the_known_vector() {
        let mut crc = Crc64::new();
        crc.update(b"123456789");
        assert_eq!(crc.finalize(), 0x995D_C9BB_DF19_39FA);
    }

    #[test]
    fn one_block_passes_policy_but_still_cannot_free_anything() {
        let xz = parse("t1");
        assert!(xz.streams[0].blocks.len() < 2);
        policy(&xz).unwrap();
    }

    #[test]
    fn several_blocks_pass_policy() {
        policy(&parse("multi")).unwrap();
    }

    #[test]
    fn concatenated_streams_are_refused() {
        let err = policy(&parse("multistream")).unwrap_err();
        assert!(
            err.to_string().contains("one after another"),
            "got: {err:#}"
        );
    }

    #[test]
    fn sha256_checks_are_refused() {
        let err = policy(&parse("sha256")).unwrap_err();
        assert!(err.to_string().contains("SHA-256"), "got: {err:#}");
    }

    #[test]
    fn an_unchecked_stream_passes_policy_because_it_extracts_while_kept() {
        let xz = parse("nocheck");
        assert_eq!(xz.streams[0].check, Check::None);
        policy(&xz).unwrap();
    }

    /// Same payload through every single-block shape: the LZMA2 reader and
    /// the container's check must reproduce what the real xz CLI wrote.
    #[test]
    fn real_archives_decode_through_their_block_chains() {
        let plain = decode("t1");
        assert_eq!(plain.len(), 8192);
        for name in ["multi", "crc32", "nocheck"] {
            assert_eq!(decode(name), plain, "{name} decoded differently");
        }
        // BCJ and Delta were made from another payload; both filters must
        // reproduce it, and it is the one that starts like an executable.
        let filtered = decode("bcj");
        assert_eq!(filtered.len(), 8192);
        assert_eq!(decode("delta"), filtered, "delta decoded differently");
        assert!(filtered.starts_with(b"MZ"));
    }

    #[test]
    fn uneven_blocks_decode_to_the_whole_payload() {
        let out = decode("uneven");
        assert_eq!(out.len(), 29_491_200);
        assert!(out.iter().enumerate().all(|(i, b)| *b == (i % 256) as u8));
    }
}
