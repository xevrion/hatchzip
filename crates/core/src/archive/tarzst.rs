//! A `.zst`-compressed tar is a sequence of independent zstd frames, each
//! holding the next stretch of one tar. A frame decodes only from its start,
//! so a frame boundary is a resume point that needs no decoder state and no
//! window, exactly like a gzip member start, and the engine frees the frames
//! behind the decoder by punching them away at those points. The tar parser
//! is the one every compressed tar shares, see `targz`.
//!
//! The `zstd` CLI writes one frame however many threads it uses, so most
//! `.tar.zst` files have no boundary at all and cannot free anything while
//! they decode. They are refused for in-place extraction with a message
//! naming `--keep-archive`, which decodes them whole and leaves the archive
//! as it was. `pzstd` and concatenated files write several frames and free
//! space as they go.
//!
//! The frames are parsed here, block by block, before anything is decoded:
//! their offsets, whether each carries the XXH64 checksum, and the window its
//! header declares. The `zstd` crate then decodes one bounded frame at a
//! time, so a decoder that lies about its input cannot read into the next
//! frame, and every frame's checksum is verified before its bytes can be
//! punched.

use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};

use super::storage::{Reader, Storage};
use super::targz::{self, CodecKind, Event, Feed, Profile, StreamPoint, TarAt, TarStream};
use super::{
    Format, Item, Kind, Opened, Sink, SolidStream, SolidWalker, Unit, Unsupported, Worker,
};
use crate::journal::Resume;

pub const NAME: &str = "tar.zst";

/// Decoded output is handed out in pieces of at most this much, as targz
/// does.
const BUF: usize = targz::BUF;

/// The largest window a frame may name. It is allocated whole while the
/// frame decodes; zstd's presets top out at 128 MiB, and `--long=31` asks
/// for 2 GiB, which this refuses before anything is touched.
const MAX_WINDOW: u64 = 1 << 30;

const FRAME_MAGIC: u32 = 0xFD2F_B528;
/// Skippable frames use the low nibble of this magic, `0x184D2A50` through
/// `0x184D2A5F`. `pzstd` writes one before every frame and the seekable
/// format puts its table in one at the end.
const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;

/// One zstd frame, as the container describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Frame {
    /// Where the frame's magic begins: what a resume point records.
    start: u64,
    /// First byte after the frame, checksum included.
    end: u64,
    /// Whether the frame header says the XXH64 checksum follows its last
    /// block.
    checksum: bool,
    /// The window the header declares, in bytes.
    window: u64,
    /// The dictionary the header names, if any.
    dictionary: Option<u32>,
}

/// Reads every frame from `from` (0 for the whole archive, or a resumed
/// run's frame start) to the end, skipping skippable frames. Damage here is
/// found before anything is touched.
fn parse_frames<R: Read + Seek>(reader: &mut R, len: u64, from: u64) -> Result<Vec<Frame>> {
    fn taken<R: Read + Seek>(reader: &mut R, at: u64, buf: &mut [u8]) -> Result<()> {
        reader.seek(SeekFrom::Start(at))?;
        reader.read_exact(buf).map_err(|e| {
            anyhow::anyhow!("the archive is shorter than its zstd frame says; it is damaged: {e}")
        })
    }

    fn u32_at<R: Read + Seek>(reader: &mut R, at: u64) -> Result<u32> {
        let mut b = [0u8; 4];
        taken(reader, at, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn le<R: Read + Seek>(reader: &mut R, at: u64, n: usize) -> Result<u64> {
        let mut b = [0u8; 8];
        taken(reader, at, &mut b[..n])?;
        Ok(u64::from_le_bytes(b))
    }

    if from > len {
        bail!("the journal's resume point is past the end of the archive");
    }
    let mut at = from;
    let mut frames = Vec::new();
    while at < len {
        if len - at < 4 {
            bail!(
                "the archive ends {} bytes into a zstd frame header; it is truncated",
                len - at
            );
        }
        let magic = u32_at(reader, at)?;
        if magic & 0xFFFF_FFF0 == SKIPPABLE_MAGIC {
            if frames.is_empty() && from > 0 {
                bail!(
                    "the journal's resume point does not name a zstd frame header; the archive \
                     will not be modified"
                );
            }
            if len - at < 8 {
                bail!("the archive ends inside a skippable zstd frame; it is truncated");
            }
            let size = u64::from(u32_at(reader, at + 4)?);
            at = at
                .checked_add(8 + size)
                .filter(|end| *end <= len)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "a skippable zstd frame says it holds {size} bytes, more than the \
                         archive has; it is truncated"
                    )
                })?;
            continue;
        }
        if magic != FRAME_MAGIC {
            if from > 0 && frames.is_empty() {
                bail!(
                    "the journal's resume point does not name a zstd frame header; the archive \
                     will not be modified"
                );
            }
            bail!("the bytes at offset {at} are not a zstd frame header; the archive is damaged");
        }
        if len - at < 5 {
            bail!("the archive ends inside a zstd frame header; it is truncated");
        }

        let mut header = [0u8; 14];
        let n = (len - at - 4).min(header.len() as u64) as usize;
        taken(reader, at + 4, &mut header[..n])?;
        let descriptor = header[0];
        if descriptor & 0x18 != 0 {
            bail!("a zstd frame header sets a reserved bit; the archive is damaged");
        }
        let single = descriptor & 0x20 != 0;
        let checksum = descriptor & 0x04 != 0;
        let dict_size = [0usize, 1, 2, 4][(descriptor & 0x03) as usize];
        let fcs_size = match descriptor >> 6 {
            0 if single => 1,
            0 => 0,
            1 => 2,
            2 => 4,
            _ => 8,
        };

        let mut pos = at + 5;
        let window = if single {
            0 // Filled in from the content size below.
        } else {
            if n < 2 {
                bail!("the archive ends inside a zstd frame header; it is truncated");
            }
            let wd = header[1];
            pos += 1;
            let exp = u64::from(wd >> 3);
            let mant = u64::from(wd & 0x07);
            (1u64 << (10 + exp)) + (mant << (exp + 3))
        };
        let dictionary = match dict_size {
            0 => None,
            1 | 2 | 4 => {
                let id = le(reader, pos, dict_size)?;
                pos += dict_size as u64;
                u32::try_from(id).ok().filter(|id| *id != 0)
            }
            _ => unreachable!(),
        };
        let content = if fcs_size == 0 {
            None
        } else {
            let value = le(reader, pos, fcs_size)?;
            pos += fcs_size as u64;
            Some(if fcs_size == 2 { value + 256 } else { value })
        };
        let window = if single {
            content.ok_or_else(|| {
                anyhow::anyhow!("a zstd frame declares a single segment but no content size")
            })?
        } else {
            window
        };

        loop {
            if pos + 3 > len {
                bail!("the archive ends inside a zstd frame; it is truncated");
            }
            let mut h = [0u8; 3];
            taken(reader, pos, &mut h)?;
            let header = u32::from_le_bytes([h[0], h[1], h[2], 0]);
            pos += 3;
            let last = header & 1 != 0;
            let kind = (header >> 1) & 0x03;
            let size = u64::from(header >> 3);
            let data = match kind {
                0 | 2 => size,
                1 => 1,
                _ => bail!("a zstd frame uses a reserved block type; the archive is damaged"),
            };
            pos = pos
                .checked_add(data)
                .filter(|end| *end <= len)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "a zstd block says it holds {data} bytes, more than the archive has; it is \
                     truncated"
                    )
                })?;
            if last {
                break;
            }
        }
        if checksum {
            pos = pos
                .checked_add(4)
                .filter(|end| *end <= len)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "the archive ends before a zstd frame's checksum; it is truncated"
                    )
                })?;
        }
        frames.push(Frame {
            start: at,
            end: pos,
            checksum,
            window,
            dictionary,
        });
        at = pos;
    }
    if frames.is_empty() {
        bail!(
            "the archive holds no zstd frame{}",
            if from > 0 {
                " from the journal's resume point; it will not be modified"
            } else {
                ""
            }
        );
    }
    Ok(frames)
}

/// What the format refuses about a parsed stream before anything is
/// decoded. A window over the cap or a dictionary is valid zstd that this
/// build cannot extract in place.
fn policy(frames: &[Frame]) -> Result<()> {
    if frames.is_empty() {
        bail!("the archive holds no zstd frame");
    }
    for f in frames {
        if let Some(id) = f.dictionary {
            bail!(
                "a frame of the archive needs the zstd dictionary {id}, and dictionaries are \
                 not supported"
            );
        }
        if f.window > MAX_WINDOW {
            bail!(
                "a frame of the archive needs a {} window, beyond the {} limit, so it cannot be \
                 decoded here. Recompress it without --long, or extract it with another program",
                crate::human(f.window),
                crate::human(MAX_WINDOW)
            );
        }
    }
    Ok(())
}

/// The container with what the format refuses about it, and a resumed
/// run's walk.
pub struct Solid {
    storage: Arc<Storage>,
    frames: Vec<Frame>,
    /// How many frames came before the first one here. A fresh run starts at
    /// 0; a resumed one starts after the frames an earlier run punched away,
    /// so the stream is still known to have had more than one.
    from_index: u64,
    from: Option<Resume>,
    profile: Profile,
}

impl Solid {
    fn new(storage: Arc<Storage>, from: Option<&Resume>) -> Result<Self> {
        let (from_byte, from_index) = match from {
            None => (0, 0),
            Some(r) => {
                let s = r.stream.as_ref().ok_or_else(|| {
                    anyhow!("the journal's resume point does not belong to a compressed tar")
                })?;
                if s.kind != CodecKind::Zstd {
                    bail!(
                        "the journal's resume point belongs to {} tar, not this archive's",
                        match s.kind {
                            CodecKind::Gzip => "a gzip member of a",
                            CodecKind::Bzip2 => "a bzip2 block of a",
                            CodecKind::Xz => "an xz block of a",
                            CodecKind::Zstd => "a zstd frame of a",
                        }
                    );
                }
                if r.bit % 8 != 0 {
                    bail!(
                        "the journal's resume point is not on a zstd frame boundary; the archive \
                         will not be modified"
                    );
                }
                (r.bit / 8, s.block)
            }
        };
        let frames = {
            let mut reader = storage.reader(None);
            parse_frames(&mut reader, storage.original_len(), from_byte)
                .context("reading the zstd stream's structure")?
        };
        // A resume reads the same container, so it meets the same refusals;
        // a journal that names a frame this build cannot decode fails here
        // rather than halfway into the stream.
        policy(&frames).map_err(|e| anyhow::Error::new(Unsupported(e)))?;
        Ok(Self {
            storage,
            frames,
            from_index,
            from: from.cloned(),
            profile: Profile::default(),
        })
    }

    /// Walks the whole of what `from` leaves, or the whole archive.
    pub fn walk(&self, from: Option<&Resume>) -> Result<Walker> {
        Walker::new(self, from)
    }
}

/// Whether the zstd stream opens with a tar header, reading only as far as
/// the first one. Used to tell a plain `.zst` of one file from a compressed
/// tar before either is offered.
pub(crate) fn holds_tar(storage: &Storage) -> Result<bool> {
    let storage = Arc::new(Storage::open(storage.primary(), false)?);
    let solid = Solid::new(storage.clone(), None)?;
    let mut feed = ZstdFeed::new(storage.clone(), solid.frames.clone(), 0, 0);
    let mut first = [0u8; 512];
    let mut n = 0;
    while n < first.len() {
        match feed.fill(&mut first[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(first[..n].len() >= 263 && &first[257..262] == b"ustar")
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
    // not extract it. Everything else that stops the read is damage.
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
        format: Box::new(TarZst { solid }),
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

struct TarZst {
    solid: Solid,
}

impl Format for TarZst {
    fn worker(&self, _threads: u32) -> Result<Box<dyn Worker + '_>> {
        Ok(Box::new(TarZstWorker { solid: &self.solid }))
    }

    fn solid(&self) -> Option<&dyn SolidStream> {
        Some(&self.solid)
    }

    fn refuses_in_place(&self) -> Option<String> {
        let frames = &self.solid.frames;
        // On a resume, frames before the point were punched away, but the
        // count they contributed is in the journal.
        let whole = self.solid.from_index + (frames.len() as u64) < 2;
        let unchecked = frames.iter().any(|f| !f.checksum);
        Some(match (whole, unchecked) {
            (true, true) => String::from(
                "the archive's whole tar is inside one zstd frame and its frames carry no \
                 checksum, so nothing of it can be freed as it is extracted",
            ),
            (true, false) => String::from(
                "the archive's whole tar is inside one zstd frame, so nothing of the archive \
                 can be freed as it is extracted",
            ),
            (false, true) => String::from(
                "the archive's zstd frames do not all carry a checksum, so damage inside them \
                 could not be detected before the bytes behind are freed",
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

/// The compressed bytes of one frame and nothing else: a decoder that lies
/// about how much of the frame it has decoded cannot read into the next one.
struct Bytes {
    reader: Reader,
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
                "the archive is shorter than its frames say",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// The window the decoder may allocate for a frame, as a log. The header's
/// size was already checked against the cap; this keeps a decoder that reads
/// a different header from growing past it.
fn window_log(window: u64) -> u32 {
    let mut log = 10;
    while (1u64 << log) < window && log < 30 {
        log += 1;
    }
    log
}

/// The zstd side of a walk: it decodes one frame at a time into the tar
/// parser, and verifies each frame's checksum before the next frame starts.
struct ZstdFeed {
    frames: Vec<Frame>,
    storage: Arc<Storage>,
    /// Absolute index of `frames[0]`, so points keep counting across a
    /// resume.
    base: u64,
    /// Which frame to decode, relative to `frames`.
    frame: usize,
    decoder: Option<zstd::stream::read::Decoder<'static, BufReader<Bytes>>>,
    /// A byte the end-of-frame probe read, handed out on the next fill.
    pending: Option<u8>,
    ended: bool,
}

impl ZstdFeed {
    fn new(storage: Arc<Storage>, frames: Vec<Frame>, base: u64, frame: usize) -> Self {
        Self {
            frames,
            storage,
            base,
            frame,
            decoder: None,
            pending: None,
            ended: false,
        }
    }

    fn build(&self) -> Result<zstd::stream::read::Decoder<'static, BufReader<Bytes>>> {
        let f = &self.frames[self.frame];
        let mut reader = self.storage.reader(None);
        reader
            .seek(SeekFrom::Start(f.start))
            .with_context(|| format!("reading zstd frame {}", self.base + self.frame as u64 + 1))?;
        let bytes = Bytes {
            reader,
            left: f.end - f.start,
        };
        let mut decoder = zstd::stream::read::Decoder::new(bytes).with_context(|| {
            format!(
                "starting the decoder for zstd frame {}",
                self.base + self.frame as u64 + 1
            )
        })?;
        decoder
            .window_log_max(window_log(f.window))
            .with_context(|| {
                format!(
                    "setting the window limit for zstd frame {}",
                    self.base + self.frame as u64 + 1
                )
            })?;
        Ok(decoder.single_frame())
    }

    /// The frame just ended: consume and verify what is left of it, then
    /// move on. A checksum mismatch is an error here, before a point can be
    /// journaled.
    fn finish(&mut self) -> Result<()> {
        if let Some(mut decoder) = self.decoder.take() {
            decoder.finish_frame().with_context(|| {
                format!(
                    "the archive's zstd frame {} is damaged or fails its checksum",
                    self.base + self.frame as u64 + 1
                )
            })?;
        }
        self.frame += 1;
        self.ended = self.frame == self.frames.len();
        Ok(())
    }
}

impl Feed for ZstdFeed {
    fn fill(&mut self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() || self.ended {
            return Ok(0);
        }
        let mut start = 0;
        if let Some(byte) = self.pending.take() {
            buf[0] = byte;
            start = 1;
            if buf.len() == 1 {
                return Ok(1);
            }
        }
        loop {
            if self.decoder.is_none() {
                anyhow::ensure!(
                    self.frame < self.frames.len(),
                    "the walk asks for input past the last zstd frame"
                );
                self.decoder = Some(self.build()?);
            }
            let n = self
                .decoder
                .as_mut()
                .expect("the frame's decoder was just built")
                .read(&mut buf[start..])
                .with_context(|| {
                    format!(
                        "decoding zstd frame {}; the archive is damaged",
                        self.base + self.frame as u64 + 1
                    )
                })?;
            if n == 0 {
                // A frame with no output of its own. The tar parser must not
                // see an end here while frames remain, so go on in this fill.
                self.finish()?;
                if self.ended || start == buf.len() {
                    return Ok(start);
                }
                continue;
            }
            // Read one byte ahead: when the frame ends at these bytes, the
            // checksum is verified now and `restartable` can announce the
            // point before the tar parser asks for the next frame.
            let mut probe = [0u8; 1];
            let more = self
                .decoder
                .as_mut()
                .expect("the frame's decoder is still open")
                .read(&mut probe)
                .with_context(|| {
                    format!(
                        "reading the end of zstd frame {}; the archive is damaged",
                        self.base + self.frame as u64 + 1
                    )
                })?;
            if more == 0 {
                self.finish()?;
            } else {
                self.pending = Some(probe[0]);
            }
            return Ok(start + n);
        }
    }

    fn restartable(&self) -> bool {
        // A frame boundary is the one place this stream restarts from, and
        // its checksum was just verified.
        self.decoder.is_none() && !self.ended
    }

    fn point(&self, tar_pos: u64, at: TarAt, written: u64) -> Result<Resume> {
        let f = self
            .frames
            .get(self.frame)
            .ok_or_else(|| anyhow!("the stream has no zstd frame a resume point could follow"))?;
        Ok(Resume {
            unit: 0,
            written,
            // Frame starts are whole bytes.
            bit: f.start * 8,
            crc: 0,
            stream: Some(Box::new(StreamPoint {
                kind: CodecKind::Zstd,
                member_start: false,
                member_crc: 0,
                member_len: 0,
                window: Vec::new(),
                block: self.base + self.frame as u64,
                level: None,
                combined_crc: None,
                tar_pos,
                at,
            })),
        })
    }

    fn point_byte(&self) -> Option<u64> {
        if self.ended {
            None
        } else {
            self.frames.get(self.frame).map(|f| f.start)
        }
    }
}

/// Walks the archive from `from`, or from its start, listing what is left
/// and measuring it. Damage anywhere is found here, before anything is
/// touched.
pub struct Walker {
    inner: TarStream<ZstdFeed>,
}

impl Walker {
    fn new(solid: &Solid, from: Option<&Resume>) -> Result<Self> {
        let feed = match from {
            None => ZstdFeed::new(solid.storage.clone(), solid.frames.clone(), 0, 0),
            Some(r) => {
                let s = r.stream.as_ref().ok_or_else(|| {
                    anyhow!("the journal's resume point does not belong to a compressed tar")
                })?;
                if s.kind != CodecKind::Zstd {
                    bail!(
                        "the journal's resume point belongs to {} tar, not this archive's",
                        match s.kind {
                            CodecKind::Gzip => "a gzip member of a",
                            CodecKind::Bzip2 => "a bzip2 block of a",
                            CodecKind::Xz => "an xz block of a",
                            CodecKind::Zstd => "a zstd frame of a",
                        }
                    );
                }
                if solid.frames.first().is_none_or(|f| f.start != r.bit / 8) {
                    bail!(
                        "the journal's resume point is inconsistent; the archive will not be \
                         modified"
                    );
                }
                if !matches!(s.at, TarAt::File { .. }) && r.written != 0 {
                    bail!(
                        "the journal's resume point claims a file is open where none is; \
                         the archive will not be modified"
                    );
                }
                ZstdFeed::new(solid.storage.clone(), solid.frames.clone(), s.block, 0)
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
struct TarZstWorker<'a> {
    solid: &'a Solid,
}

impl Worker for TarZstWorker<'_> {
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
                    sink.item(&item, &mut io::Cursor::new(link.unwrap_or_default()))?;
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

    fn bytes(name: &str) -> &'static [u8] {
        match name {
            "one" => include_bytes!("fixtures/zst/zst-one.tar.zst"),
            "two" => include_bytes!("fixtures/zst/zst-two.tar.zst"),
            "skip" => include_bytes!("fixtures/zst/zst-skip.tar.zst"),
            "seekable" => include_bytes!("fixtures/zst/zst-seekable.tar.zst"),
            "pzstd" => include_bytes!("fixtures/zst/zst-pzstd.tar.zst"),
            "nocheck" => include_bytes!("fixtures/zst/zst-nocheck-one.tar.zst"),
            "nocheck_two" => include_bytes!("fixtures/zst/zst-nocheck-two.tar.zst"),
            "long27" => include_bytes!("fixtures/zst/zst-long27.tar.zst"),
            "long31" => include_bytes!("fixtures/zst/zst-long31.tar.zst"),
            "dict" => include_bytes!("fixtures/zst/zst-dict.tar.zst"),
            "bad_check" => include_bytes!("fixtures/zst/zst-bad-check.tar.zst"),
            "truncated" => include_bytes!("fixtures/zst/zst-truncated.tar.zst"),
            "bad_magic" => include_bytes!("fixtures/zst/zst-bad-magic.tar.zst"),
            "notar" => include_bytes!("fixtures/zst/zst-notar.tar.zst"),
            _ => unreachable!(),
        }
    }

    fn frames(name: &str) -> Vec<Frame> {
        let b = bytes(name);
        parse_frames(&mut Cursor::new(b), b.len() as u64, 0).unwrap()
    }

    fn refused(name: &str) -> String {
        let b = bytes(name);
        format!(
            "{:#}",
            parse_frames(&mut Cursor::new(b), b.len() as u64, 0).unwrap_err()
        )
    }

    #[test]
    fn one_frame_covers_the_whole_file() {
        let b = bytes("one");
        let f = frames("one");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].start, 0);
        assert_eq!(f[0].end, b.len() as u64);
        assert!(f[0].checksum);
        // The tar is 260 KiB and the frame declares no separate window, so
        // its content size is the window.
        assert_eq!(f[0].window, 266_240);
        assert_eq!(f[0].dictionary, None);
    }

    #[test]
    fn two_frames_are_listed_in_order() {
        let f = frames("two");
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].start, 0);
        assert_eq!(f[1].start, f[0].end);
        assert_eq!(f[1].end, bytes("two").len() as u64);
        assert!(f.iter().all(|f| f.checksum));
    }

    #[test]
    fn skippable_frames_are_not_frames() {
        let f = frames("skip");
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].start, 0);
        assert!(f[1].start > f[0].end, "the skippable frame sits between");
    }

    #[test]
    fn a_trailing_seek_table_is_skipped() {
        let f = frames("seekable");
        assert_eq!(f.len(), 2);
        assert!(
            f[1].end < bytes("seekable").len() as u64,
            "the table follows the last frame"
        );
    }

    /// `pzstd` writes a skippable frame before every frame, not only the
    /// first, and several real frames follow.
    #[test]
    fn pzstd_frames_survive_their_skippable_headers() {
        let f = frames("pzstd");
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].start, 12, "the leading skippable frame comes first");
        assert_eq!(f[1].start, f[0].end + 12);
    }

    #[test]
    fn frames_without_a_check_say_so() {
        assert!(!frames("nocheck")[0].checksum);
        assert!(frames("nocheck_two").iter().all(|f| !f.checksum));
    }

    #[test]
    fn a_window_descriptor_is_read_as_declared() {
        assert_eq!(frames("long27")[0].window, 1 << 27);
    }

    #[test]
    fn a_dictionary_is_refused() {
        let err = format!("{:#}", policy(&frames("dict")).unwrap_err());
        assert!(err.contains("dictionary"), "got: {err}");
    }

    #[test]
    fn a_window_over_the_cap_is_refused() {
        let err = format!("{:#}", policy(&frames("long31")).unwrap_err());
        assert!(err.contains("window"), "got: {err}");
        assert!(err.contains("1.1 GB"), "got: {err}");
    }

    #[test]
    fn a_resume_point_must_name_a_frame_start() {
        let b = bytes("two");
        let second = frames("two")[1].start;
        let rest = parse_frames(&mut Cursor::new(b), b.len() as u64, second).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].start, second);

        let err = format!(
            "{:#}",
            parse_frames(&mut Cursor::new(b), b.len() as u64, 100).unwrap_err()
        );
        assert!(err.contains("resume point"), "got: {err}");
    }

    fn storage_of(
        name: &str,
        as_name: &str,
    ) -> (
        tempfile::TempDir,
        std::sync::Arc<super::super::storage::Storage>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(as_name);
        std::fs::write(&path, bytes(name)).unwrap();
        let storage =
            std::sync::Arc::new(super::super::storage::Storage::open(&path, false).unwrap());
        (dir, storage)
    }

    fn opened(name: &str) -> (tempfile::TempDir, super::super::Opened) {
        let (dir, storage) = storage_of(name, name);
        let opened = open(&storage, None).unwrap();
        (dir, opened)
    }

    fn open_error(name: &str, as_name: &str) -> String {
        let (_dir, storage) = storage_of(name, as_name);
        match open(&storage, None) {
            Ok(_) => panic!("{name} opened"),
            Err(e) => format!("{e:#}"),
        }
    }

    fn files(
        w: &mut dyn super::super::SolidWalker,
    ) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        use super::super::targz::Event;
        let mut all = std::collections::BTreeMap::new();
        let mut current: Option<(std::path::PathBuf, Vec<u8>)> = None;
        loop {
            match w.next(usize::MAX).unwrap() {
                Event::Entry { item, .. } => {
                    if item.kind == super::super::Kind::File {
                        current = Some((item.path.clone(), Vec::new()));
                    }
                }
                Event::Data(d) => {
                    if let Some((_, buf)) = &mut current {
                        buf.extend_from_slice(d);
                    }
                }
                Event::FileEnd => {
                    if let Some((path, data)) = current.take() {
                        assert!(all.insert(path, data).is_none());
                    }
                }
                Event::Point => {}
                Event::End => break,
            }
        }
        all
    }

    /// A run of empty frames must not end the walk and must not recurse per
    /// frame; ten thousand is far past any stack.
    #[test]
    fn many_empty_frames_before_the_tar_are_skipped() {
        let mut empty = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        empty
            .set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))
            .unwrap();
        let empty = empty.finish().unwrap();

        let mut many = Vec::new();
        for _ in 0..10_000 {
            many.extend_from_slice(&empty);
        }
        many.extend_from_slice(bytes("one"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty-frames.tar.zst");
        std::fs::write(&path, many).unwrap();
        let storage =
            std::sync::Arc::new(super::super::storage::Storage::open(&path, false).unwrap());
        let opened = open(&storage, None).unwrap();
        let mut w = opened.format.solid().unwrap().walk(None).unwrap();
        let all = files(&mut *w);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn a_real_archive_walks_to_its_entries() {
        let (_dir, o) = opened("one");
        let mut w = o.format.solid().unwrap().walk(None).unwrap();
        let all = files(&mut *w);
        assert_eq!(all.len(), 3);
        assert_eq!(
            all[std::path::Path::new("game/data/random.bin")].len(),
            128 * 1024
        );
        assert_eq!(
            all[std::path::Path::new("game/data/text.txt")].len(),
            128 * 1024
        );
        assert!(all.contains_key(std::path::Path::new("game/README.txt")));
    }

    #[test]
    fn a_single_frame_is_refused_in_place() {
        let (_dir, o) = opened("one");
        let why = o
            .format
            .refuses_in_place()
            .expect("one frame cannot be freed");
        assert!(why.contains("one zstd frame"), "got: {why}");
    }

    #[test]
    fn several_checked_frames_free_space() {
        for name in ["two", "skip", "seekable", "pzstd"] {
            let (_dir, o) = opened(name);
            assert!(
                o.format.refuses_in_place().is_none(),
                "{name}: {}",
                o.format.refuses_in_place().unwrap_or_default()
            );
        }
    }

    #[test]
    fn frames_without_a_check_are_refused_in_place() {
        for name in ["nocheck", "nocheck_two"] {
            let (_dir, o) = opened(name);
            let why = o.format.refuses_in_place().unwrap();
            assert!(why.contains("checksum"), "{name}: {why}");
        }
    }

    #[test]
    fn damage_is_refused_before_anything_is_touched() {
        for (name, says) in [
            ("bad_check", "damaged"),
            ("truncated", "truncated"),
            ("bad_magic", "zstd frame"),
        ] {
            let err = open_error(name, name);
            assert!(err.contains(says), "{name}: {err}");
        }
    }

    #[test]
    fn a_window_over_the_cap_or_a_dictionary_is_unsupported() {
        for name in ["long31", "dict"] {
            let (_dir, storage) = storage_of(name, name);
            let err = match open(&storage, None) {
                Ok(_) => panic!("{name} opened"),
                Err(e) => e,
            };
            assert!(err.is::<super::super::Unsupported>(), "{name}: {err:#}");
        }
    }

    #[test]
    fn a_bare_zstd_file_is_not_an_archive() {
        let (_dir, storage) = storage_of("notar", "notar.zst");
        let err = match super::super::open(&storage, None, None) {
            Ok(_) => panic!("a plain zstd file opened as an archive"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("does not contain a tar archive"), "got: {err}");
    }

    #[test]
    fn a_resumed_walk_continues_where_the_point_is() {
        let (_dir, o) = opened("two");
        let mut w = o.format.solid().unwrap().walk(None).unwrap();
        let mut point = None;
        loop {
            match w.next(usize::MAX).unwrap() {
                super::super::targz::Event::Point => {
                    point = Some(w.point().unwrap());
                    break;
                }
                super::super::targz::Event::End => break,
                _ => {}
            }
        }
        let point = point.expect("two frames have a boundary");
        let (_dir2, storage) = storage_of("two", "two");
        let o = open(&storage, Some(&point)).unwrap();
        let mut w = o.format.solid().unwrap().walk(Some(&point)).unwrap();
        let resumed = files(&mut *w);
        assert!(!resumed.is_empty(), "the walk found the rest of the tar");
    }

    #[test]
    fn a_resume_still_knows_the_stream_was_multi_frame() {
        let (_dir, o) = opened("two");
        let mut w = o.format.solid().unwrap().walk(None).unwrap();
        let mut point = None;
        loop {
            match w.next(usize::MAX).unwrap() {
                super::super::targz::Event::Point => {
                    point = Some(w.point().unwrap());
                    break;
                }
                super::super::targz::Event::End => break,
                _ => {}
            }
        }
        let point = point.expect("two frames have a boundary");
        let (_dir2, storage) = storage_of("two", "two");
        let o = open(&storage, Some(&point)).unwrap();
        assert!(
            o.format.refuses_in_place().is_none(),
            "a resumed multi-frame stream still frees space"
        );
    }

    #[test]
    fn bad_magic_and_truncation_are_refused() {
        let err = refused("bad_magic");
        assert!(err.contains("zstd frame"), "got: {err}");
        let err = refused("truncated");
        assert!(
            err.contains("shorter") || err.contains("truncated") || err.contains("ends"),
            "got: {err}"
        );
    }
}
