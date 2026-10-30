//! The `.xz` container: stream headers, blocks with their filter chains,
//! the index and stream footers, per "The .xz File Format" v1.2.1. This
//! parses structure only. Nothing here decodes compressed data; verifying
//! a block's check belongs to the full scan the tar.xz format runs before
//! touching anything, which builds on these block tables.
//!
//! A single-threaded `xz` writes block headers without any sizes, so the
//! walk cannot jump from header to header the way a ZIP central directory
//! lets it. The parser therefore works the way liblzma does: from the end.
//! The footer's backward size locates the index, the index's records give
//! every block's extent, and each block header is then validated where it
//! must be. Sizes the header does store are checked against the index, so
//! a lying header fails the parse.
//!
//! The index sits past every block, so it is also what keeps a half-freed
//! archive parseable: [`Xz::parse_resumed`] trusts the index's arithmetic
//! for the blocks an earlier run freed (whose headers are zeros now) and
//! validates the headers from its resume point's block on. Whether a block
//! was proveable is on it, as [`Block::validated`].

use std::io::{self, Read, Seek, SeekFrom};

const HEADER_MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0x00];
const FOOTER_MAGIC: [u8; 2] = *b"YZ";

/// Filter IDs, spec sections 5.3.1 to 5.3.3.
const ID_DELTA: u64 = 0x03;
const ID_BCJ_FIRST: u64 = 0x04;
const ID_BCJ_LAST: u64 = 0x0B;
const ID_LZMA2: u64 = 0x21;

/// The largest index this parser reads. A record takes at least two bytes
/// and a real archive has a few bytes per block, so 64 MiB leaves room for
/// millions of blocks and still bounds what a lying footer can make the
/// parser allocate.
const MAX_INDEX: u64 = 64 * 1024 * 1024;

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// A valid container this build refuses to read, as opposed to damage.
fn unsupported(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, msg)
}

/// The check a stream stores after every block, from the low nibble of
/// Stream Flags (spec 2.1.1.2). The reserved IDs have known sizes and no
/// known algorithm, so they stay `Other` for the policy to refuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Check {
    None,
    Crc32,
    Crc64,
    Sha256,
    Other { id: u8, size: u8 },
}

impl Check {
    fn from_id(id: u8) -> Self {
        match id {
            0x00 => Check::None,
            0x01 => Check::Crc32,
            0x04 => Check::Crc64,
            0x0A => Check::Sha256,
            other => Check::Other {
                id: other,
                size: match other {
                    0x02 | 0x03 => 4,
                    0x05 | 0x06 => 8,
                    0x07..=0x09 => 16,
                    0x0B | 0x0C => 32,
                    _ => 64,
                },
            },
        }
    }

    pub(crate) fn size(self) -> u64 {
        match self {
            Check::None => 0,
            Check::Crc32 => 4,
            Check::Crc64 => 8,
            Check::Sha256 => 32,
            Check::Other { size, .. } => u64::from(size),
        }
    }
}

/// One filter of a block's chain, properties decoded (spec 5.3). Chains end
/// in LZMA2 and the size-preserving kinds only ever precede it (spec 5.2).
/// An ID this module doesn't know parses as `Unknown`; the caller's support
/// policy is what refuses it, so the parser stays format agnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Filter {
    Lzma2 {
        /// The dictionary size the one properties byte describes.
        dict_size: u32,
    },
    Delta {
        /// 1 to 256.
        dist: u32,
    },
    Bcj {
        /// The filter ID, so callers can tell the architectures apart.
        arch: u64,
        start: u32,
    },
    Unknown {
        id: u64,
    },
}

impl Filter {
    /// Instruction alignment per filter, spec 5.3.2.
    fn bcj_alignment(arch: u64) -> u64 {
        match arch {
            0x04 => 1,                      // x86
            0x05 | 0x07 | 0x09 | 0x0A => 4, // PowerPC, ARM, SPARC, ARM64
            0x08 | 0x0B => 2,               // ARM Thumb, RISC-V
            _ => 16,                        // IA64
        }
    }

    pub(crate) fn is_lzma2(self) -> bool {
        matches!(self, Filter::Lzma2 { .. })
    }
}

/// One block: where its parts lie in the file and how to decode them.
/// `header_len`, `data_off` and `unpadded` beyond the extents below exist
/// for the tiles the tests measure, and the format module reads the rest.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct Block {
    pub header_off: u64,
    pub header_len: u64,
    pub data_off: u64,
    pub comp_len: u64,
    pub uncomp_len: u64,
    /// After the block padding; the check itself is `Check::size` bytes.
    pub check_off: u64,
    /// Header plus data plus check, what the index records (spec 4.3.1).
    pub unpadded: u64,
    pub filters: Vec<Filter>,
    /// Whether the header at `header_off` was read whole and proven by its
    /// CRC32. Only false for the blocks a resumed run freed earlier: their
    /// extents still come from the index, but their header bytes, written
    /// over by the punches, are zeros without a check behind them.
    pub validated: bool,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct Stream {
    /// Where this stream's 12-byte header begins in the file.
    pub start: u64,
    pub check: Check,
    pub blocks: Vec<Block>,
    pub index_off: u64,
    /// One past the last footer byte.
    pub end: u64,
    pub total_uncomp: u64,
}

/// A parsed `.xz` file: one or more streams, as `xz -l` would report.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct Xz {
    pub streams: Vec<Stream>,
    pub uncompressed: u64,
}

impl Xz {
    /// Parses a file of `len` bytes, from its last stream backwards. Every
    /// size is bounded by `len` and by checked arithmetic, so a corrupt
    /// varint can neither send the reader past the end nor make the index
    /// parse allocate for blocks that cannot exist. Tests call this; the
    /// format reads [`parse_resumed`](Self::parse_resumed).
    #[cfg(test)]
    pub(crate) fn parse<R: Read + Seek>(reader: &mut R, len: u64) -> io::Result<Self> {
        Self::parse_from(reader, len, 0)
    }

    /// Parses what a resumed run reads: the file's last stream and only
    /// that one, keeping the index's sizes for the `skip` first bytes that
    /// an earlier run freed without holding a whole archive over it. The
    /// blocks whose extents lie below `skip` come out unvalidated; the
    /// first block at or after `skip` is where a resume point means, and
    /// everything from there on is validated as usual. Every stream the
    /// file still holds comes with it; what a resumed run may do with them
    /// is the format's policy, not this parser's.
    pub(crate) fn parse_resumed<R: Read + Seek>(
        reader: &mut R,
        len: u64,
        skip: u64,
    ) -> io::Result<Self> {
        Self::parse_from(reader, len, skip)
    }

    fn parse_from<R: Read + Seek>(reader: &mut R, len: u64, skip: u64) -> io::Result<Self> {
        let mut streams = Vec::new();
        let mut end = skip_padding_tail(reader, len)?;
        loop {
            let (stream, before) = parse_stream_end(reader, len, end, skip)?;
            streams.push(stream);
            let start = streams.last().unwrap().start;
            if start == 0 {
                break;
            }
            if skip > 0 {
                return Err(invalid(
                    "a resume point names a stream that is not the file's only one",
                ));
            }
            end = skip_padding_tail(reader, before)?;
            if end == 0 {
                return Err(invalid("the file starts in the middle of a stream"));
            }
        }
        streams.reverse();
        let uncompressed = streams
            .iter()
            .try_fold(0u64, |sum, s| sum.checked_add(s.total_uncomp))
            .ok_or_else(|| invalid("the total uncompressed size overflows"))?;
        Ok(Self {
            streams,
            uncompressed,
        })
    }
}

/// Walks back over a multiple-of-four run of nulls (trailing Stream
/// Padding, spec 2.2) and returns the offset it ends at.
fn skip_padding_tail<R: Read + Seek>(reader: &mut R, end: u64) -> io::Result<u64> {
    let mut pos = end;
    while pos > 0 {
        reader.seek(SeekFrom::Start(pos - 1))?;
        let mut b = [0u8; 1];
        if reader.read_exact(&mut b).is_err() || b[0] != 0 {
            break;
        }
        pos -= 1;
    }
    if !(end - pos).is_multiple_of(4) {
        return Err(invalid("the file ends in a partial run of stream padding"));
    }
    Ok(pos)
}

/// Parses the stream whose last byte sits at `end`, and returns it together
/// with the first byte of whatever precedes it (its header, minus the
/// 12-byte stream header this function verifies). With `skip` set, a
/// resumed run's read of a half-freed file: the stream header and the
/// blocks whose extents lie below `skip` were punched over, so only their
/// index-record sizes stand, unvalidated, while everything past `skip` is
/// validated as usual.
fn parse_stream_end<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    end: u64,
    skip: u64,
) -> io::Result<(Stream, u64)> {
    if end < 32 || end > len {
        return Err(invalid("the file ends before a whole stream"));
    }
    // The footer (spec 2.1.2): CRC32 of the six bytes that follow it,
    // backward size, a copy of the stream flags, and the 'YZ' magic.
    let foot = read_at(reader, end - 12, 12)?;
    if foot[10..12] != FOOTER_MAGIC {
        return Err(invalid("missing the footer magic bytes"));
    }
    if crc32(&foot[4..10]) != le_u32(&foot[0..4]) {
        return Err(invalid("the stream footer CRC32 does not match"));
    }
    let index_len = u64::from(le_u32(&foot[4..8])) * 4 + 4;
    if index_len > MAX_INDEX {
        return Err(unsupported(
            "the index is larger than 64 MiB, which this build does not read",
        ));
    }
    let index_off = end
        .checked_sub(12)
        .and_then(|p| p.checked_sub(index_len))
        .ok_or_else(|| invalid("the footer's backward size reaches past the file"))?;
    let check = Check::from_id(foot[9] & 0x0F);
    let cs = check.size();

    // The index (spec 4): indicator, record count, records, null padding to
    // four and a CRC32 over everything before it.
    let body_len = (index_len - 4) as usize;
    let body = read_at(reader, index_off, body_len)?;
    let mut cur = Cur::new(&body);
    if cur.byte()? != 0 {
        return Err(invalid("the index does not start with its indicator"));
    }
    let count = cur.varint()?;
    if count > body.len() as u64 / 2 {
        return Err(invalid(
            "the index claims more records than the file can hold",
        ));
    }
    let mut records = Vec::with_capacity(count.min(1024) as usize);
    for _ in 0..count {
        let unpadded = cur.varint()?;
        let uncomp = cur.varint()?;
        records.push((unpadded, uncomp));
    }
    if cur.rest().iter().any(|b| *b != 0) {
        return Err(invalid("the index padding is not null"));
    }
    if crc32(&body) != le_u32(&read_at(reader, index_off + index_len - 4, 4)?) {
        return Err(invalid("the index CRC32 does not match"));
    }

    // Every block's total length follows from its record: the unpadded size
    // without the check, rounded out to four bytes, plus the check. Walking
    // back from the index puts the last block first, so the records are
    // visited in reverse and the starts flipped to file order after.
    let mut starts = Vec::with_capacity(records.len());
    let mut pos = index_off;
    for (unpadded, _) in records.iter().rev() {
        // The block's body is its unpadded size without the check, and the
        // block padding rounds it up to four bytes for the next block.
        if *unpadded < cs {
            return Err(invalid("an index record is smaller than the check"));
        }
        let total = (unpadded - cs).div_ceil(4) * 4 + cs;
        if total > pos {
            return Err(invalid(
                "the index records do not fit the space of the stream",
            ));
        }
        pos -= total;
        starts.push(pos);
    }
    // Walked backwards, so flip to file order.
    starts.reverse();
    let start = pos
        .checked_sub(12)
        .ok_or_else(|| invalid("the index records reach past the start of the stream"))?;

    // The stream header, including the flags copy the footer must match. A
    // resumed run cannot have it: the punches start at the file's first
    // byte and reach past it, so only the footer's copy stands.
    let readable_head = skip <= start;
    let head = if readable_head {
        read_at(reader, start, 12)?
    } else {
        vec![0; 12]
    };
    if readable_head {
        if head[0..6] != HEADER_MAGIC {
            return Err(invalid("missing the stream magic bytes"));
        }
        if head[6] != 0 || head[7] & 0xF0 != 0 {
            return Err(invalid("bad stream flags"));
        }
        if crc32(&head[6..8]) != le_u32(&head[8..12]) {
            return Err(invalid("the stream header CRC32 does not match"));
        }
        if head[6..8] != foot[8..10] {
            return Err(invalid("the footer flags differ from the header"));
        }
    }

    // Validate each block header where the records claim it lies, and read
    // its filter chain. Sizes the header stores must agree with the record.
    // A resumed run's first resumeable block is the first whole one: the
    // punches never reach into it (they are rounded down short of the
    // point), so everything past `skip` is there to read.
    let mut blocks = Vec::with_capacity(records.len());
    for (i, (unpadded, uncomp)) in records.iter().enumerate() {
        let off = starts[i];
        let block = if off >= skip {
            validate_block(reader, off, *unpadded, *uncomp, cs)?
        } else {
            Block {
                header_off: off,
                header_len: 0,
                data_off: off,
                comp_len: 0,
                uncomp_len: *uncomp,
                check_off: off,
                unpadded: *unpadded,
                filters: Vec::new(),
                validated: false,
            }
        };
        blocks.push(block);
    }
    let total_uncomp = records
        .iter()
        .try_fold(0u64, |sum, r| sum.checked_add(r.1))
        .ok_or_else(|| invalid("the total uncompressed size overflows"))?;

    Ok((
        Stream {
            check,
            blocks,
            index_off,
            end,
            total_uncomp,
            start,
        },
        start,
    ))
}

fn validate_block<R: Read + Seek>(
    reader: &mut R,
    off: u64,
    unpadded: u64,
    uncomp_len: u64,
    cs: u64,
) -> io::Result<Block> {
    let mut size_byte = [0u8; 1];
    reader.seek(SeekFrom::Start(off))?;
    reader.read_exact(&mut size_byte)?;
    let header_len = u64::from(size_byte[0]) * 4 + 4;
    if size_byte[0] == 0 || header_len > 1024 {
        return Err(invalid("a block header size is out of range"));
    }
    if header_len + cs + 1 > unpadded {
        return Err(invalid("a block header does not fit its index record"));
    }
    let mut header = vec![0u8; header_len as usize];
    header[0] = size_byte[0];
    reader.read_exact(&mut header[1..])?;
    // The CRC32 covers the whole header except itself, size byte included
    // (spec 3.1.7).
    let (body, crc) = header.split_at(header.len() - 4);
    if crc32(body) != le_u32(crc) {
        return Err(invalid("the block header CRC32 does not match"));
    }

    let mut cur = Cur::new(&body[1..]); // past the size byte, to the flags
    let flags = cur.byte()?;
    if flags & 0x3C != 0 {
        return Err(invalid("reserved bits are set in a block header"));
    }
    let comp_len = unpadded - header_len - cs;
    if flags & 0x40 != 0 && cur.varint()? != comp_len {
        return Err(invalid(
            "a block header's compressed size lies against the index",
        ));
    }
    if flags & 0x80 != 0 && cur.varint()? != uncomp_len {
        return Err(invalid(
            "a block header's uncompressed size lies against the index",
        ));
    }
    let nfilters = u64::from(flags & 0x03) + 1;
    let mut filters = Vec::new();
    for _ in 0..nfilters {
        let id = cur.varint()?;
        let psize = cur.varint()?;
        if psize > 64 || id >= 1 << 62 {
            return Err(invalid("an out of range filter entry"));
        }
        let props = cur.take(psize as usize)?.to_vec();
        filters.push(decode_filter(id, &props)?);
    }
    if cur.rest().iter().any(|b| *b != 0) {
        return Err(invalid("a block header's padding is not null"));
    }

    match filters.last() {
        Some(Filter::Delta { .. } | Filter::Bcj { .. }) => {
            return Err(invalid("the block's last filter cannot be last"));
        }
        None => return Err(invalid("a block with no filters")),
        _ => {}
    }
    if filters[..filters.len() - 1].iter().any(|f| f.is_lzma2()) {
        return Err(invalid("LZMA2 must be the block's last filter"));
    }

    let data_off = off + header_len;
    let block_pad = (4 - comp_len % 4) % 4;
    Ok(Block {
        header_off: off,
        header_len,
        data_off,
        comp_len,
        uncomp_len,
        check_off: data_off + comp_len + block_pad,
        unpadded,
        filters,
        validated: true,
    })
}

fn decode_filter(id: u64, props: &[u8]) -> io::Result<Filter> {
    Ok(match id {
        ID_LZMA2 => {
            let bits = match props {
                [b] if b & 0xC0 == 0 => u64::from(b & 0x3F),
                _ => return Err(invalid("bad LZMA2 properties")),
            };
            if bits > 40 {
                return Err(invalid("an LZMA2 dictionary beyond the format"));
            }
            // Spec 5.3.1: one bit of mantissa, five of exponent.
            let dict_size = if bits == 40 {
                u32::MAX
            } else {
                (2u32 | (bits & 1) as u32) << (bits / 2 + 11)
            };
            Filter::Lzma2 { dict_size }
        }
        ID_DELTA => match props {
            [b] => Filter::Delta {
                dist: u32::from(*b) + 1,
            },
            _ => return Err(invalid("bad delta properties")),
        },
        arch @ ID_BCJ_FIRST..=ID_BCJ_LAST => {
            let start = match props {
                [] => 0,
                [a, b, c, d] => u32::from_le_bytes([*a, *b, *c, *d]),
                _ => return Err(invalid("bad BCJ properties size")),
            };
            if u64::from(start) % Filter::bcj_alignment(arch) != 0 {
                return Err(invalid("a BCJ start offset is misaligned"));
            }
            Filter::Bcj { arch, start }
        }
        other => Filter::Unknown { id: other },
    })
}

/// A positioned byte source: reads at an offset without disturbing the
/// cursor, which the block validation interleaves with index work.
fn read_at<R: Read + Seek>(reader: &mut R, off: u64, n: usize) -> io::Result<Vec<u8>> {
    reader.seek(SeekFrom::Start(off))?;
    let mut buf = vec![0u8; n];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

/// A byte cursor over an in-memory header or index.
struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }

    fn byte(&mut self) -> io::Result<u8> {
        let v = *self
            .b
            .get(self.i)
            .ok_or_else(|| invalid("a truncated block header"))?;
        self.i += 1;
        Ok(v)
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.i + n > self.b.len() {
            return Err(invalid("a truncated block header"));
        }
        let v = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(v)
    }

    /// The spec's multibyte integer (section 1.2): seven bits per byte,
    /// little endian, never continued by a zero byte.
    fn varint(&mut self) -> io::Result<u64> {
        let mut v = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            if shift > 0 && b == 0 {
                return Err(invalid("a multibyte integer continued into a zero byte"));
            }
            v |= u64::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                return Err(invalid("a multibyte integer overflows 63 bits"));
            }
        }
    }

    fn rest(&self) -> &'a [u8] {
        &self.b[self.i..]
    }
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn crc32(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// The fixtures come from the real xz CLI 5.8.4 and liblzma 5.x
    /// (through Python's lzma module for the check variants), documented in
    /// crates/core/src/archive/fixtures/xz/.
    fn fixture(name: &str) -> &'static [u8] {
        match name {
            "t1" => include_bytes!("fixtures/xz/xz-t1.xz"),
            "multi" => include_bytes!("fixtures/xz/xz-multi.xz"),
            "bcj" => include_bytes!("fixtures/xz/xz-bcj.xz"),
            "delta" => include_bytes!("fixtures/xz/xz-delta.xz"),
            "crc32" => include_bytes!("fixtures/xz/xz-crc32.xz"),
            "sha256" => include_bytes!("fixtures/xz/xz-sha256.xz"),
            "nocheck" => include_bytes!("fixtures/xz/xz-nocheck.xz"),
            "multistream" => include_bytes!("fixtures/xz/xz-multistream.xz"),
            "uneven" => include_bytes!("fixtures/xz/xz-uneven.xz"),
            "bad-magic" => include_bytes!("fixtures/xz/xz-bad-magic.xz"),
            "bad-flags" => include_bytes!("fixtures/xz/xz-bad-flags.xz"),
            "bad-blockhdr" => include_bytes!("fixtures/xz/xz-bad-blockhdr.xz"),
            "truncated" => include_bytes!("fixtures/xz/xz-truncated.xz"),
            _ => unreachable!(),
        }
    }

    fn parse(name: &str) -> io::Result<Xz> {
        let bytes = fixture(name);
        Xz::parse(&mut Cursor::new(bytes), bytes.len() as u64)
    }

    #[test]
    fn single_thread_stream_has_one_block() {
        let xz = parse("t1").unwrap();
        assert_eq!(xz.streams.len(), 1);
        let s = &xz.streams[0];
        assert_eq!(s.check, Check::Crc64);
        assert_eq!(s.blocks.len(), 1);
        let b = &s.blocks[0];
        assert_eq!(b.uncomp_len, 8192);
        assert_eq!(b.filters, vec![Filter::Lzma2 { dict_size: 8 << 20 }]);
        assert_eq!(xz.uncompressed, 8192);
    }

    #[test]
    fn multi_block_stream_walks_every_block() {
        let xz = parse("multi").unwrap();
        let s = &xz.streams[0];
        assert_eq!(s.blocks.len(), 2);
        assert_eq!(xz.uncompressed, 8192);
        // The blocks tile the stream: the second starts where the first
        // check ends, rounded out to four.
        let (a, b) = (&s.blocks[0], &s.blocks[1]);
        let a_total = (a.unpadded - s.check.size()).div_ceil(4) * 4 + s.check.size();
        assert_eq!(b.header_off, a.header_off + a_total);
        assert!(a.comp_len > 0);
    }

    #[test]
    fn filter_chains_parse() {
        let xz = parse("bcj").unwrap();
        assert_eq!(
            xz.streams[0].blocks[0].filters,
            vec![
                Filter::Bcj { arch: 4, start: 0 },
                Filter::Lzma2 { dict_size: 8 << 20 }
            ]
        );
        let xz = parse("delta").unwrap();
        assert_eq!(
            xz.streams[0].blocks[0].filters,
            vec![
                Filter::Delta { dist: 4 },
                Filter::Lzma2 { dict_size: 8 << 20 }
            ]
        );
    }

    #[test]
    fn check_types_parse() {
        assert_eq!(parse("crc32").unwrap().streams[0].check, Check::Crc32);
        assert_eq!(parse("nocheck").unwrap().streams[0].check, Check::None);
        assert_eq!(parse("sha256").unwrap().streams[0].check, Check::Sha256);
    }

    #[test]
    fn concatenated_streams_are_all_found() {
        let xz = parse("multistream").unwrap();
        assert_eq!(xz.streams.len(), 2);
        assert_eq!(xz.uncompressed, 16384);
        assert_eq!(xz.streams[1].blocks.len(), 1);
        assert_eq!(
            xz.streams[1].start, xz.streams[0].end,
            "the fixtures were concatenated with no padding between them"
        );
    }

    #[test]
    fn uneven_blocks_keep_their_places() {
        // `xz -T4 -6 --block-size=8MiB` over very compressible data: the
        // last block is smaller than the others, which once sent every
        // block to the wrong start. The parse validates every header CRC.
        let xz = parse("uneven").unwrap();
        let s = &xz.streams[0];
        assert_eq!(s.blocks.len(), 4);
        assert_eq!(s.check, Check::Crc64);
        assert_ne!(
            s.blocks[3].unpadded, s.blocks[0].unpadded,
            "the fixture must have blocks of different unpadded sizes"
        );
        assert_eq!(xz.uncompressed, 29491200);
        for w in s.blocks.windows(2) {
            let (pair, window) = (&w[0], &w[1]);
            let a_total = (pair.unpadded - s.check.size()).div_ceil(4) * 4 + s.check.size();
            assert_eq!(window.header_off, pair.header_off + a_total);
        }
    }

    #[test]
    fn resume_skip_trusts_only_what_the_index_says_of_a_freed_block() {
        let full = parse("multi").unwrap();
        let skip = full.streams[0].blocks[1].header_off;
        let bytes = fixture("multi");
        let resumed = Xz::parse_resumed(&mut Cursor::new(bytes), bytes.len() as u64, skip).unwrap();
        assert_eq!(resumed.streams.len(), 1, "a resume is a single-stream read");
        let (full, s) = (&full.streams[0], &resumed.streams[0]);
        assert_eq!(s.blocks.len(), full.blocks.len());
        for (i, b) in s.blocks.iter().enumerate() {
            let whole = &full.blocks[i];
            assert_eq!(b.header_off, whole.header_off, "block {}", i + 1);
            assert_eq!(b.uncomp_len, whole.uncomp_len, "block {}", i + 1);
            assert_eq!(
                b.validated,
                b.header_off >= skip,
                "block {} is {} the resume point",
                i + 1,
                if b.header_off >= skip {
                    "past"
                } else {
                    "before"
                }
            );
        }
    }

    #[test]
    fn an_index_beyond_the_cap_is_refused_as_unsupported() {
        let bytes = fixture("multi");
        let mut bytes = bytes.to_vec();
        let n = bytes.len();
        // The footer's backward size, made to claim a 64 MiB index, with the
        // footer CRC fixed so the record itself is consistent.
        bytes[n - 8..n - 4].copy_from_slice(&0x0100_0000u32.to_le_bytes());
        let crc = crc32(&bytes[n - 8..n - 2]);
        bytes[n - 12..n - 8].copy_from_slice(&crc.to_le_bytes());
        let err = Xz::parse(&mut Cursor::new(&bytes), bytes.len() as u64).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported, "{err}");
    }

    #[test]
    fn corrupt_structures_are_refused() {
        for name in ["bad-magic", "bad-flags", "bad-blockhdr", "truncated"] {
            let err = parse(name).expect_err(&format!("{name} must not parse"));
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{name}");
        }
    }

    #[test]
    fn crc32_matches_the_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
