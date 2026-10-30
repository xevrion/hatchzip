//! Bzip2 blocks feed the shared tar parser. Boundaries are accepted only
//! after a synthetic one-block stream decodes and verifies its CRC.

use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};

use super::storage::{Reader, Storage};
use super::targz::{BUF, CodecKind, Event, Feed, Profile, StreamPoint, TarAt, TarStream};
use super::{Format, Item, Kind, Opened, Sink, SolidStream, SolidWalker, Unit, Worker};
use crate::journal::Resume;

pub const NAME: &str = "tar.bz2";
const BLOCK: u64 = 0x3141_5926_5359;
const END: u64 = 0x1772_4538_5090;
const MASK: u64 = (1 << 48) - 1;

static MAGIC_PAIRS: [u64; 1024] = magic_pairs();

const fn magic_pairs() -> [u64; 1024] {
    let mut table = [0; 1024];
    let magics = [BLOCK, END];
    let mut i = 0;
    while i < magics.len() {
        // Across all eight alignments, a whole-byte pair can start at
        // any of the first 33 bits of a magic.
        let mut shift = 0;
        while shift <= 32 {
            let pair = ((magics[i] >> shift) & 0xffff) as usize;
            table[pair / 64] |= 1 << (pair % 64);
            shift += 1;
        }
        i += 1;
    }
    table
}

/// Trial decodes allowed per block before it is refused as damaged.
const MAX_CANDIDATES: u32 = 64;

fn find_magic(bytes: &[u8], base: u64, from_bit: u64) -> Option<(u64, u64)> {
    let mut next = from_bit.max(base * 8);
    if bytes.len() < 6 {
        return None;
    }
    let last = (base + bytes.len() as u64) * 8 - 48;
    if next > last {
        return None;
    }
    let mut pair = (next.div_ceil(16) * 2 - base) as usize;
    while pair + 1 < bytes.len() {
        let value = usize::from(u16::from_be_bytes([bytes[pair], bytes[pair + 1]]));
        if MAGIC_PAIRS[value / 64] & (1 << (value % 64)) != 0 {
            let bit = (base + pair as u64) * 8;
            // Five whole bytes guarantee an even-offset pair in every magic.
            // Its first such pair tests its start before any later match;
            // the cursor prevents overlapping pairs from testing starts twice.
            next = next.max(bit.saturating_sub(32));
            let stop = bit.min(last);
            while next <= stop {
                let candidate = next;
                next += 1;
                let byte = (candidate / 8 - base) as usize;
                let mut word = [0; 8];
                word[1..7].copy_from_slice(&bytes[byte..byte + 6]);
                word[7] = bytes.get(byte + 6).copied().unwrap_or(0);
                let magic = (u64::from_be_bytes(word) >> (8 - candidate % 8)) & MASK;
                if magic == BLOCK || magic == END {
                    return Some((candidate, magic));
                }
            }
        }
        pair += 2;
    }
    None
}

pub struct Solid {
    storage: Arc<Storage>,
    /// The point the scan began at: a resumed run's, or the start.
    from: Option<Resume>,
    profile: Profile,
}

impl Solid {
    /// Decodes from `from`, or from the start of the archive.
    pub fn walk(&self, from: Option<&Resume>) -> Result<Walker> {
        Walker::new(&self.storage, from)
    }
}

/// Walks the archive from `from`, or from its start, listing what is left
/// and measuring it. Damage anywhere is found here, before anything is
/// touched.
pub struct Walker {
    inner: TarStream<Bzip2>,
}

impl Walker {
    fn new(storage: &Storage, from: Option<&Resume>) -> Result<Self> {
        let reader = storage.reader(None);
        let end = storage.original_len();
        let bz = match from {
            None => Bzip2::new(reader, end, &BzPoint::start())?,
            Some(r) => {
                let s = r.stream.as_ref().ok_or_else(|| {
                    anyhow!("the journal's resume point does not belong to a compressed tar")
                })?;
                if s.kind != CodecKind::Bzip2 {
                    bail!(
                        "the journal's resume point belongs to {}, not this archive's \
                         compressed stream",
                        match s.kind {
                            CodecKind::Gzip => "a gzip member of",
                            CodecKind::Xz => "an xz block of",
                            CodecKind::Bzip2 => "a bzip2 block of",
                        }
                    );
                }
                if !matches!(s.at, TarAt::File { .. }) && r.written != 0 {
                    bail!(
                        "the journal's resume point claims a file is open where none is; the archive will not be modified"
                    );
                }
                Bzip2::new(
                    reader,
                    end,
                    &BzPoint {
                        bit: r.bit,
                        stream_start: s.member_start,
                        crc: s.combined_crc.ok_or_else(|| anyhow!("the journal lacks the bzip2 combined CRC; restore the original journal before resuming"))?,
                        level: s.level.ok_or_else(|| anyhow!("the journal lacks the bzip2 level; restore the original journal before resuming"))?,
                    },
                )?
            }
        };
        Ok(Self {
            inner: TarStream::new(bz, from)?,
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

/// Walks the archive from `from`, or from its start, listing what is left
/// and measuring it. Damage anywhere is found here, before anything is
/// touched.
pub fn open(storage: &Arc<Storage>, from: Option<&Resume>) -> Result<Opened> {
    let mut solid = Solid {
        storage: storage.clone(),
        from: from.cloned(),
        profile: Profile::default(),
    };
    let name = storage
        .primary()
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    storage.expect_reads(storage.original_len() - from.map_or(0, |r| r.bit / 8));
    let (items, profile) = super::scan(&solid, from)
        .map_err(|e| anyhow::Error::new(super::Damaged(e)))
        .with_context(|| format!("checking {name} (nothing was changed)"))?;
    solid.profile = profile;
    Ok(Opened {
        name: NAME,
        format: Box::new(TarBz2 { solid }),
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

struct TarBz2 {
    solid: Solid,
}

impl Format for TarBz2 {
    fn worker(&self, _threads: u32) -> Result<Box<dyn Worker + '_>> {
        Ok(Box::new(TarBz2Worker { solid: &self.solid }))
    }

    fn solid(&self) -> Option<&dyn SolidStream> {
        Some(&self.solid)
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

/// Decodes the whole stream in one go, for `test` and `--verify-first`. The
/// extraction itself goes through the engine's streaming path instead.
struct TarBz2Worker<'a> {
    solid: &'a Solid,
}

impl Worker for TarBz2Worker<'_> {
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

impl Feed for Bzip2 {
    fn fill(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.read(buf)
    }
    fn restartable(&self) -> bool {
        self.restart_point().is_some()
    }
    fn point(&self, tar_pos: u64, at: TarAt, written: u64) -> Result<Resume> {
        let p = self
            .restart_point()
            .ok_or_else(|| anyhow!("the bzip2 stream cannot be resumed here"))?;
        Ok(Resume {
            unit: 0,
            written,
            bit: p.bit,
            crc: 0,
            stream: Some(Box::new(StreamPoint {
                kind: CodecKind::Bzip2,
                member_start: p.stream_start,
                member_crc: 0,
                member_len: 0,
                level: Some(p.level),
                combined_crc: Some(p.crc),
                window: Vec::new(),
                block: 0,
                tar_pos,
                at,
            })),
        })
    }
    fn point_byte(&self) -> Option<u64> {
        self.restart_point().map(|p| p.bit / 8)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BzPoint {
    pub(crate) bit: u64,
    pub(crate) stream_start: bool,
    pub(crate) level: u8,
    pub(crate) crc: u32,
}

impl BzPoint {
    pub(crate) fn start() -> Self {
        Self {
            bit: 0,
            stream_start: true,
            level: 9,
            crc: 0,
        }
    }
}

/// Each candidate boundary is tried as the end of a synthetic one-block
/// stream. Only a successful decode, including its CRC, accepts it.
pub(crate) struct Bzip2<R = Reader> {
    input: Bits<R>,
    point: BzPoint,
    output: Vec<u8>,
    pos: usize,
    done: bool,
}

impl<R: Read + Seek> Bzip2<R> {
    pub(crate) fn new(reader: R, end: u64, at: &BzPoint) -> Result<Self> {
        if at.bit / 8 > end
            || !(1..=9).contains(&at.level)
            || (at.stream_start && !at.bit.is_multiple_of(8))
            || (at.stream_start && at.crc != 0)
        {
            bail!(
                "the journal's bzip2 resume point is inconsistent; the archive was not changed. Restore the original journal before resuming"
            );
        }
        Ok(Self {
            input: Bits::new(reader, end, at.bit)?,
            point: at.clone(),
            output: Vec::new(),
            pos: 0,
            done: false,
        })
    }

    pub(crate) fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.pos == self.output.len() {
            if self.done {
                return Ok(0);
            }
            self.block()?;
        }
        let n = buf.len().min(self.output.len() - self.pos);
        buf[..n].copy_from_slice(&self.output[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }

    pub(crate) fn restart_point(&self) -> Option<BzPoint> {
        (!self.done && self.pos == self.output.len()).then(|| self.point.clone())
    }

    fn header(&mut self) -> Result<bool> {
        if self.input.bit == self.input.end * 8 {
            return Ok(false);
        }
        let at = self.input.bit / 8;
        // Nothing before a stream header is needed again, and concatenated
        // empty streams would otherwise pile up in the buffer.
        self.input.discard_before(at);
        let first = self.input.take(8)?;
        if first == 0 {
            // Check zero padding a chunk at a time and drop each one, so a
            // long padded tail costs no more memory than one chunk.
            while self.input.bit < self.input.end * 8 {
                let byte = self.input.bit / 8;
                self.input.ensure(byte)?;
                self.input.discard_before(byte);
                self.input.bit = (self.input.base + self.input.bytes.len() as u64) * 8;
                if self.input.bytes.iter().any(|&b| b != 0) {
                    bail!(
                        "unexpected data follows the compressed data at byte {at}; the archive is damaged and was not changed. Download it again"
                    );
                }
            }
            return Ok(false);
        }
        if first != u64::from(b'B') || self.input.take(16)? != 0x5a68 {
            bail!(
                "unexpected data follows the compressed data at byte {at}; the archive is damaged and was not changed. Download it again"
            );
        }
        let level = self.input.take(8)? as u8;
        if !(b'1'..=b'9').contains(&level) {
            bail!(
                "the bzip2 stream at byte {at} has an invalid level; the archive is damaged and was not changed. Download it again"
            );
        }
        self.point.level = level - b'0';
        self.point.crc = 0;
        self.point.stream_start = false;
        self.point.bit = self.input.bit;
        Ok(true)
    }

    fn trailer(&mut self) -> Result<()> {
        if self.input.take(32)? as u32 != self.point.crc {
            bail!(
                "the bzip2 stream is damaged (combined CRC mismatch); the archive is damaged and was not changed. Download it again"
            );
        }
        let pad = (8 - self.input.bit % 8) % 8;
        if self.input.take(pad as usize)? != 0 {
            bail!(
                "the bzip2 stream has nonzero padding; the archive is damaged and was not changed. Download it again"
            );
        }
        self.point.bit = self.input.bit;
        self.point.stream_start = true;
        self.point.crc = 0;
        Ok(())
    }

    fn block(&mut self) -> Result<()> {
        self.output.clear();
        self.pos = 0;
        if self.point.stream_start && !self.header()? {
            self.done = true;
            return Ok(());
        }
        match self.input.take(48)? {
            END => return self.trailer(),
            BLOCK => {}
            _ => bail!(
                "the bzip2 block at bit {} has a damaged header; the archive is damaged and was not changed. Download it again",
                self.point.bit
            ),
        }
        let crc = self.input.take(32)? as u32;
        let start = self.point.bit;
        // Keep the source bytes intact. A candidate is aligned only once,
        // when the decoder can prove that the preceding block ends there.
        self.input.discard_before(start / 8);
        let mut byte = self.input.bit / 8;
        let mut next = start + 80;
        // At most 20 bits per symbol, plus room for the tables and selectors,
        // covers every block a real compressor writes. The format itself has
        // no such bound (table code lengths can be padded with deltas that
        // cancel out), but without one a damaged archive would be buffered
        // whole while looking for a boundary.
        let limit = start / 8 + u64::from(self.point.level) * 400_000 + 65_536;
        // A false signature turns up about once in 2^47 bits, so a block with
        // many was planted to make every one of them a trial decode.
        let mut tries = 0;
        loop {
            self.input.ensure(byte)?;
            let stop = self.input.base + self.input.bytes.len() as u64;
            // A candidate must finish before the byte that trips the limit.
            let scan_stop = stop.min(limit);
            while let Some((candidate, magic)) = find_magic(
                &self.input.bytes[..(scan_stop - self.input.base) as usize],
                self.input.base,
                next,
            ) {
                next = candidate + 1;
                tries += 1;
                if tries > MAX_CANDIDATES {
                    bail!(
                        "the bzip2 block at bit {start} holds more than {MAX_CANDIDATES} false block signatures; the archive is damaged and was not changed. Download it again"
                    );
                }
                let offset = (start / 8 - self.input.base) as usize;
                if let Some(out) = decode_block(
                    &self.input.bytes[offset..],
                    (start % 8) as usize,
                    (candidate - start) as usize,
                    self.point.level,
                    crc,
                ) {
                    self.output = out;
                    self.point.crc = self.point.crc.rotate_left(1) ^ crc;
                    self.input.bit = candidate;
                    self.point.bit = candidate;
                    if magic == END {
                        self.input.take(48)?;
                        self.trailer()?;
                    }
                    return Ok(());
                }
            }
            byte = stop;
            if byte > limit {
                bail!(
                    "the bzip2 block is damaged, or larger than any bzip2 compressor writes (no CRC-checked boundary); the archive was not changed. Download it again, or recompress it with bzip2"
                );
            }
            // A signature in the final 47 bits may finish in the next chunk.
            next = next.max(stop * 8 - 47);
        }
    }
}

/// Shift whole words rather than rebuilding a block bit by bit. The final
/// byte is masked because its unused bits belong to the following block.
fn decode_block(bytes: &[u8], shift: usize, bits: usize, level: u8, crc: u32) -> Option<Vec<u8>> {
    let len = bits.div_ceil(8);
    let mut data = vec![0; 4 + (bits + 80).div_ceil(8)];
    data[..4].copy_from_slice(&[b'B', b'Z', b'h', b'0' + level]);
    let dest = &mut data[4..4 + len];
    if shift == 0 {
        dest.copy_from_slice(&bytes[..len]);
    } else {
        let mut pos = 0;
        while pos + 8 <= len {
            let word = u64::from_be_bytes(bytes[pos..pos + 8].try_into().ok()?);
            let next = u64::from(bytes.get(pos + 8).copied().unwrap_or(0));
            dest[pos..pos + 8]
                .copy_from_slice(&((word << shift) | (next >> (8 - shift))).to_be_bytes());
            pos += 8;
        }
        while pos < len {
            dest[pos] =
                (bytes[pos] << shift) | (bytes.get(pos + 1).copied().unwrap_or(0) >> (8 - shift));
            pos += 1;
        }
    }
    if !bits.is_multiple_of(8) {
        *dest.last_mut()? &= 0xff << (8 - bits % 8);
    }
    let trailer = [0x17, 0x72, 0x45, 0x38, 0x50, 0x90];
    let offset = bits / 8 + 4;
    let shift = bits % 8;
    for (i, byte) in trailer.into_iter().chain(crc.to_be_bytes()).enumerate() {
        data[offset + i] |= byte >> shift;
        if shift != 0 {
            data[offset + i + 1] |= byte << (8 - shift);
        }
    }
    let mut decoder = bzip2::read::BzDecoder::new(data.as_slice());
    let mut out = Vec::new();
    // The level caps a block's size before its first RLE stage is undone,
    // not its output: that stage expands five encoded bytes to 259 bytes.
    let limit = u64::from(level) * 100_000 * 52;
    (&mut decoder).take(limit + 1).read_to_end(&mut out).ok()?;
    (out.len() as u64 <= limit && decoder.total_in() == data.len() as u64).then_some(out)
}

/// Large reads serve both header fields and the byte scanner. `base` never
/// precedes the durable point's byte, even when that point is unaligned.
struct Bits<R> {
    reader: R,
    bytes: Vec<u8>,
    base: u64,
    bit: u64,
    end: u64,
}

impl<R: Read + Seek> Bits<R> {
    fn new(mut reader: R, end: u64, bit: u64) -> Result<Self> {
        reader
            .seek(SeekFrom::Start(bit / 8))
            .context("seeking to the bzip2 resume point")?;
        Ok(Self {
            reader,
            bytes: Vec::new(),
            base: bit / 8,
            bit,
            end,
        })
    }

    fn discard_before(&mut self, byte: u64) {
        let n = (byte - self.base) as usize;
        self.bytes.drain(..n);
        self.base = byte;
    }

    fn ensure(&mut self, byte: u64) -> Result<()> {
        while byte >= self.base + self.bytes.len() as u64 {
            let offset = self.base + self.bytes.len() as u64;
            let n = (self.end.saturating_sub(offset)).min(1 << 20) as usize;
            if n == 0 {
                bail!(
                    "the bzip2 data ends early or is damaged (no CRC-checked boundary); the archive was not changed. Download it again"
                );
            }
            let start = self.bytes.len();
            self.bytes.resize(start + n, 0);
            self.reader
                .read_exact(&mut self.bytes[start..])
                .context("reading the bzip2 archive; check that all volumes are available")?;
        }
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<u64> {
        if n as u64 > (self.end * 8).saturating_sub(self.bit) {
            bail!(
                "the bzip2 data ends early or is damaged (no CRC-checked boundary); the archive was not changed. Download it again"
            );
        }
        let mut value = 0;
        let mut left = n;
        while left > 0 {
            let byte = self.bit / 8;
            self.ensure(byte)?;
            let skip = (self.bit % 8) as usize;
            let count = left.min(8 - skip);
            value = (value << count)
                | u64::from(
                    (self.bytes[(byte - self.base) as usize] >> (8 - skip - count))
                        & (0xff >> (8 - count)),
                );
            self.bit += count as u64;
            left -= count;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

    fn naive_magic(bytes: &[u8], base: u64, from_bit: u64) -> Option<(u64, u64)> {
        let from = from_bit.max(base * 8);
        let mut rolling = 0;
        for bit in from..(base + bytes.len() as u64) * 8 {
            let byte = bytes[(bit / 8 - base) as usize];
            rolling = ((rolling << 1) | u64::from((byte >> (7 - bit % 8)) & 1)) & MASK;
            if bit - from >= 47 && (rolling == BLOCK || rolling == END) {
                return Some((bit + 1 - 48, rolling));
            }
        }
        None
    }

    fn plant(bytes: &mut [u8], at: usize, magic: u64) {
        for i in 0..48 {
            let bit = at + i;
            let mask = 1 << (7 - bit % 8);
            bytes[bit / 8] =
                (bytes[bit / 8] & !mask) | (((magic >> (47 - i)) as u8 & 1) << (7 - bit % 8));
        }
    }

    fn compare_searches(bytes: &[u8]) {
        for base in [0, 1, 4096, 4097] {
            for from in base * 8..=(base + bytes.len() as u64) * 8 + 8 {
                assert_eq!(
                    find_magic(bytes, base, from),
                    naive_magic(bytes, base, from),
                    "base {base}, from {from}, bytes {bytes:02x?}"
                );
            }
            let mut next = base * 8;
            let mut expected_from = next;
            for stop in 0..=bytes.len() {
                while let Some(found) = find_magic(&bytes[..stop], base, next) {
                    assert_eq!(Some(found), naive_magic(bytes, base, expected_from));
                    next = found.0 + 1;
                    expected_from = next;
                }
                next = next.max(((base + stop as u64) * 8).saturating_sub(47));
            }
            assert_eq!(naive_magic(bytes, base, expected_from), None);
        }
    }

    #[test]
    fn pair_search_matches_bit_by_bit_search() {
        let mut seed = 0x7138_b549_2ef0_6acdu64;
        let mut random = |len| -> Vec<u8> {
            (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed as u8
                })
                .collect()
        };
        for len in 0..=64 {
            compare_searches(&random(len));
        }
        for alignment in 0..8 {
            for first in [BLOCK, END] {
                for len in [6, 7, 16, 33] {
                    if alignment > len * 8 - 48 {
                        continue;
                    }
                    for at in [alignment, len * 8 - 48 - alignment] {
                        let mut bytes = random(len);
                        plant(&mut bytes, at, first);
                        compare_searches(&bytes);
                    }
                }
                for second in [BLOCK, END] {
                    for distance in [1, 7, 15, 16, 31, 32, 47, 48, 49, 55] {
                        let mut bytes = random(24);
                        plant(&mut bytes, alignment, first);
                        plant(&mut bytes, alignment + distance, second);
                        compare_searches(&bytes);
                    }
                }
            }
        }
    }

    struct Guard {
        data: Cursor<Vec<u8>>,
        first: u64,
    }

    impl Read for Guard {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            assert!(
                self.data.position() >= self.first,
                "read before the durable point"
            );
            self.data.read(buf)
        }
    }

    impl Seek for Guard {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.data.seek(pos)
        }
    }

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../cli/tests/fixtures/tarbz2")
                .join(name),
        )
        .unwrap()
    }

    fn decoder(bytes: &[u8], point: &BzPoint) -> Bzip2<Guard> {
        Bzip2::new(
            Guard {
                data: Cursor::new(bytes.to_vec()),
                first: point.bit / 8,
            },
            bytes.len() as u64,
            point,
        )
        .unwrap()
    }

    fn decode(bz: &mut Bzip2<Guard>, size: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = vec![0; size];
        loop {
            let n = bz.read(&mut buf)?;
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    #[test]
    fn every_block_and_stream_start_resumes_without_reading_its_prefix() {
        for name in ["level1.tar.bz2", "level9.tar.bz2", "multi.tar.bz2"] {
            let bytes = fixture(name);
            let mut bz = decoder(&bytes, &BzPoint::start());
            let mut out = Vec::new();
            let mut points = Vec::new();
            let mut buf = [0; 7777];
            loop {
                let n = bz.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
                if let Some(p) = bz.restart_point() {
                    points.push((p, out.len()));
                }
            }
            assert!(points.iter().any(|(p, _)| !p.bit.is_multiple_of(8)));
            if name == "multi.tar.bz2" {
                assert!(points.iter().filter(|(p, _)| p.stream_start).count() > 1);
            }
            for (point, at) in points {
                for size in [1, 65536] {
                    let mut bz = decoder(&bytes, &point);
                    assert_eq!(
                        decode(&mut bz, size).unwrap(),
                        out[at..],
                        "{name}: {point:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn zero_padding_longer_than_a_chunk_is_accepted_and_anything_else_refused() {
        let bytes = fixture("level1.tar.bz2");
        let out = decode(&mut decoder(&bytes, &BzPoint::start()), 65536).unwrap();
        let mut padded = bytes.clone();
        padded.resize(bytes.len() + 3 * (1 << 20) + 5, 0);
        let mut bz = decoder(&padded, &BzPoint::start());
        assert_eq!(decode(&mut bz, 65536).unwrap(), out);
        assert!(bz.input.bytes.len() <= 1 << 20);
        *padded.last_mut().unwrap() = 1;
        let err = decode(&mut decoder(&padded, &BzPoint::start()), 65536).unwrap_err();
        assert!(format!("{err:#}").contains("unexpected data follows"));
    }

    #[test]
    fn a_block_decodes_to_far_more_than_its_level_size() {
        let zeros = vec![0; 4_000_000];
        let mut enc = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(1));
        std::io::Write::write_all(&mut enc, &zeros).unwrap();
        let bytes = enc.finish().unwrap();
        let mut bz = decoder(&bytes, &BzPoint::start());
        let mut buf = vec![0; 1 << 20];
        bz.read(&mut buf).unwrap();
        assert!(bz.output.len() > 100_000 * 30, "{}", bz.output.len());
        assert_eq!(
            decode(&mut decoder(&bytes, &BzPoint::start()), 65536).unwrap(),
            zeros
        );
    }

    #[test]
    fn a_block_full_of_planted_signatures_is_refused() {
        let mut bytes = b"BZh1".to_vec();
        bytes.extend_from_slice(&BLOCK.to_be_bytes()[2..]);
        bytes.extend_from_slice(&[0; 4]);
        for _ in 0..=MAX_CANDIDATES {
            bytes.extend_from_slice(&[0xff; 4]);
            bytes.extend_from_slice(&BLOCK.to_be_bytes()[2..]);
        }
        bytes.resize(bytes.len() + 64, 0xff);
        let err = decode(&mut decoder(&bytes, &BzPoint::start()), 65536).unwrap_err();
        assert!(
            format!("{err:#}").contains("false block signatures"),
            "{err:#}"
        );
    }

    #[test]
    fn a_resumed_stream_still_checks_its_combined_crc() {
        let bytes = fixture("level1.tar.bz2");
        let mut bz = decoder(&bytes, &BzPoint::start());
        let mut buf = vec![0; 50_000_000];
        bz.read(&mut buf).unwrap();
        let mut point = bz.restart_point().unwrap();
        assert!(!point.stream_start);
        point.crc ^= 1;
        let err = decode(&mut decoder(&bytes, &point), 65536).unwrap_err();
        assert!(format!("{err:#}").contains("combined CRC mismatch"));
    }
}
