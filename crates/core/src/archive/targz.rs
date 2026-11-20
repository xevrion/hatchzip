//! A gzip-compressed tar is one compressed stream holding every entry, and a
//! gzip stream can only be decoded from the front. So unlike every other
//! format it is not split into units extracted from the end: the whole
//! archive is one unit, extracted front to back, and the engine frees the
//! compressed bytes behind the decoder by punching holes, at resume points
//! (see `Run::stream_solid` in the engine).
//!
//! [`Walker`] decodes the stream and parses the tar inside it into events,
//! and says where decoding can restart: at a deflate block boundary or a
//! gzip member's start that falls where the tar parser's state is simple to
//! write down. Opening the archive walks it once, start to end, before
//! anything is touched: that lists the entries, checks every gzip CRC-32 and
//! length, and measures how far the output runs ahead of the compressed
//! bytes, which the space check needs. A resumed run walks from its resume
//! point instead, since the bytes before it may be gone. The extraction
//! walks the same way with the same buffer sizes, so it meets exactly the
//! points the scan measured.
//!
//! The tar parser's own state machine is in [`TarStream`], driven by the
//! decoder hooks of [`Feed`]. `tarxz` runs the same thing over xz blocks,
//! so both compressed tars parse identically down to their events.

use std::io::{self, Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::resumable::{GzPoint, Gzip};
use super::storage::Storage;
use super::{Format, Item, Kind, Opened, Sink, SolidStream, SolidWalker, Unit, Worker, safe_path};
use crate::fsutil;
use crate::journal::Resume;

pub const NAME: &str = "tar.gz";

/// Decoded output is handed out in pieces of at most this much.
pub(super) const BUF: usize = 1024 * 1024;

/// Long-name and pax records in front of one entry, together. GNU tar and
/// pax write a few hundred bytes; anything this large is refused.
pub(super) const MAX_HEADER: u64 = 1024 * 1024;

/// The decoder's and the tar parser's state at a resume point, beyond the
/// bit position and output length the journal's [`Resume`] already holds.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct StreamPoint {
    /// Which codec the point belongs to, so a journal can never be read as
    /// the wrong compressed tar's: a `.tar.xz` point says `xz`. Older
    /// tar.gz journals have no field and read as `gzip`, which was the only
    /// codec then.
    #[serde(default, skip_serializing_if = "CodecKind::is_gzip")]
    pub kind: CodecKind,
    /// Gzip or bzip2: the point is where a gzip member's or a bzip2
    /// stream's header begins, not a block boundary inside one. Written even
    /// at its default, and so are the three below: 0.1.8 reads this journal
    /// structure and has no defaults, so a field left out makes it report
    /// the journal as damaged. Only fields added after 0.1.8 may be omitted.
    #[serde(default)]
    pub member_start: bool,
    /// Gzip: CRC-32 and length of the member's output before the point,
    /// which its trailer is checked against.
    #[serde(default)]
    pub member_crc: u32,
    #[serde(default)]
    pub member_len: u64,
    /// Gzip: the member's last 32 KiB of output before the point, which
    /// deflate back-references may reach into. It is part tar headers and
    /// part several files, so unlike a single ZIP entry's window it can't
    /// be read back from one output file. Xz needs no window: a block
    /// borrows nothing from before it.
    #[serde(default, with = "hex")]
    pub window: Vec<u8>,
    /// Xz: which block of the stream the point is at, where its header
    /// begins. The blocks before it never need decoding again.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub block: u64,
    /// Bzip2: the stream's level digit and the combined CRC of its blocks
    /// before the point, which the stream trailer is checked against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub combined_crc: Option<u32>,
    /// Offset of the point in the uncompressed tar.
    pub tar_pos: u64,
    pub at: TarAt,
}

/// Which compressed-stream codec owns a resume point. The stream state it
/// carries belongs to exactly one; a point read under another kind is
/// refused, before anything is touched.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CodecKind {
    #[default]
    Gzip,
    Xz,
    Bzip2,
}

impl CodecKind {
    pub fn is_gzip(&self) -> bool {
        *self == Self::Gzip
    }
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// Where the point falls in the tar.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TarAt {
    /// At the start of an entry's header records.
    Boundary,
    /// Inside the data of a regular file, which starts at `data_start` in
    /// the tar. How much of it is written is the resume point's `written`.
    File {
        path: PathBuf,
        size: u64,
        mode: Option<u32>,
        /// Seconds from 1970, negative before it.
        mtime: Option<i64>,
        data_start: u64,
    },
    /// Inside bytes that extract to nothing: padding, or the data of an
    /// entry that is not a regular file. `left` of them remain.
    Skip { left: u64 },
    /// Past the end-of-archive marker, where only zeros may follow.
    End,
}

/// What a scan measured about the output, relative to where it started, in
/// bytes on disk (each file rounded up to 4 KiB).
#[derive(Clone, Debug, Default)]
pub struct Profile {
    /// Regular files, as they are written.
    pub files: u64,
    /// Links, made at the very end. Counted at their target's size, since
    /// they are copied where the filesystem has no hard links.
    pub links: u64,
    /// The most the files written so far ever exceed the compressed bytes
    /// consumed, at any point where decoding can restart. Negative while
    /// the output stays below the compressed size.
    pub lead: i128,
    /// The most file output between two consecutive such points, or
    /// between the start or end and the nearest one: how far the output can
    /// run ahead before anything can be freed.
    pub gap: u64,
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

pub enum Event<'a> {
    /// An entry begins. A regular file's data follows as `Data`, then
    /// `FileEnd`; `done` is how much of it an earlier run already wrote. For
    /// a symlink, `link` is its target.
    Entry {
        item: Item,
        link: Option<Vec<u8>>,
        done: u64,
    },
    Data(&'a [u8]),
    FileEnd,
    /// Decoding can restart right here; [`Walker::point`] describes how.
    Point,
    /// The tar ended, and so did the compressed stream, every check done.
    End,
}

/// The parser's state between events, shared by the tar.xz walker.
pub(super) enum State {
    /// Collecting an entry's header records.
    Header(Vec<u8>),
    File {
        item: Item,
        data_start: u64,
        left: u64,
        pad: u64,
    },
    Skip(u64),
    Trailer,
}

impl State {
    /// Whether the parser's state here can be written down in a
    /// [`StreamPoint`]. Inside an entry's header records it can't.
    pub(super) fn at_rest(&self) -> bool {
        match self {
            State::Header(held) => held.is_empty(),
            State::File { .. } | State::Skip(_) | State::Trailer => true,
        }
    }
}

/// The decoder hooks a [`TarStream`] needs from its compressed stream.
pub(super) trait Feed {
    /// Fills `buf` with more decoded bytes; 0 means the stream is over. A
    /// call returns at most enough bytes to reach somewhere a point falls.
    fn fill(&mut self, buf: &mut [u8]) -> Result<usize>;
    /// Whether the bytes the last [`fill`](Self::fill) ended at make a
    /// resume point.
    fn restartable(&self) -> bool;
    /// The resume point those bytes make, as the journal stores it. `at`
    /// and `written` describe where the tar got to, which the codec stores
    /// beside its own state.
    fn point(&self, tar_pos: u64, at: TarAt, written: u64) -> Result<Resume>;
    /// The archive byte a resume point here would restart in. Cheap, for
    /// the scan, which only measures.
    fn point_byte(&self) -> Option<u64>;
}

impl Feed for Gzip {
    fn fill(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.read(buf)
    }

    fn restartable(&self) -> bool {
        self.restart_bit().is_some()
    }

    fn point(&self, tar_pos: u64, at: TarAt, written: u64) -> Result<Resume> {
        let point = self
            .restart_point()?
            .ok_or_else(|| anyhow!("the compressed stream cannot be resumed here"))?;
        Ok(Resume {
            unit: 0,
            written,
            bit: point.bit,
            crc: 0,
            stream: Some(Box::new(StreamPoint {
                kind: CodecKind::Gzip,
                member_start: point.member_start,
                member_crc: point.crc,
                member_len: point.len,
                window: point.window,
                block: 0,
                level: None,
                combined_crc: None,
                tar_pos,
                at,
            })),
        })
    }

    fn point_byte(&self) -> Option<u64> {
        self.restart_bit().map(|(bit, _)| bit / 8)
    }
}

/// The tar parser a compressed stream feeds: it hands out the archive's
/// events and says where decoding can restart. The walk depends on nothing
/// but the stream, so a [`TarStream`] outlives whatever handle made it.
pub(super) struct TarStream<F: Feed> {
    feed: F,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
    /// Offset in the tar of `buf[pos]`.
    tar_pos: u64,
    state: State,
    /// The decoder can restart where `buf` ends.
    restartable: bool,
    /// A file a resumed run continues, announced by the first event.
    resumed: Option<(Item, u64)>,
}

impl<F: Feed> TarStream<F> {
    /// Builds the tar side of a walk from `from`, or from the start. `from`
    /// must belong to `feed`'s kind of stream; the caller checks that.
    pub(super) fn new(feed: F, from: Option<&Resume>) -> Result<Self> {
        let (tar_pos, state, resumed) = match from {
            None => (0, State::Header(Vec::new()), None),
            Some(r) => {
                let s = r.stream.as_ref().ok_or_else(|| {
                    anyhow!("the journal's resume point does not belong to a compressed tar")
                })?;
                let (state, resumed) = resumed_state(s, r.written)?;
                (s.tar_pos, state, resumed)
            }
        };
        Ok(Self {
            feed,
            buf: vec![0; BUF],
            pos: 0,
            len: 0,
            tar_pos,
            state,
            restartable: false,
            resumed,
        })
    }

    /// The next event. `Data` is at most `max` bytes.
    pub(super) fn next(&mut self, max: usize) -> Result<Event<'_>> {
        if let Some((item, done)) = self.resumed.take() {
            return Ok(Event::Entry {
                item,
                link: None,
                done,
            });
        }
        loop {
            match &self.state {
                State::File { left: 0, pad, .. } => {
                    self.state = State::Skip(*pad);
                    return Ok(Event::FileEnd);
                }
                State::Skip(0) => {
                    self.state = State::Header(Vec::new());
                    continue;
                }
                _ => {}
            }
            if self.pos == self.len {
                if std::mem::take(&mut self.restartable) && self.state.at_rest() {
                    return Ok(Event::Point);
                }
                let n = self.feed.fill(&mut self.buf)?;
                if n == 0 {
                    return end_state(&self.state);
                }
                self.pos = 0;
                self.len = n;
                self.restartable = self.feed.restartable();
            }
            let avail = (self.len - self.pos) as u64;
            match &mut self.state {
                State::File { left, .. } => {
                    let n = (*left).min(avail).min(max as u64) as usize;
                    *left -= n as u64;
                    self.pos += n;
                    self.tar_pos += n as u64;
                    return Ok(Event::Data(&self.buf[self.pos - n..self.pos]));
                }
                State::Skip(left) => {
                    let n = (*left).min(avail);
                    *left -= n;
                    self.pos += n as usize;
                    self.tar_pos += n;
                }
                State::Trailer => {
                    if self.buf[self.pos..self.len].iter().any(|b| *b != 0) {
                        bail!(
                            "data follows the end of the tar archive inside the compressed \
                             stream, at offset {}; extraction refused",
                            self.tar_pos
                        );
                    }
                    self.pos = self.len;
                    self.tar_pos += avail;
                }
                State::Header(held) => match group(held)? {
                    Group::Need(k) => {
                        let n = (k as u64).min(avail) as usize;
                        held.extend_from_slice(&self.buf[self.pos..self.pos + n]);
                        self.pos += n;
                        self.tar_pos += n as u64;
                    }
                    Group::End => self.state = State::Trailer,
                    Group::Complete => {
                        let held = std::mem::take(held);
                        if let Some(event) = begin_entry(&mut self.tar_pos, &mut self.state, &held)?
                        {
                            return Ok(event);
                        }
                    }
                },
            }
        }
    }

    /// The resume point at the `Point` event just returned. Its `unit` is
    /// left for the engine to fill in.
    pub(super) fn point(&self) -> Result<Resume> {
        let (written, at) = match &self.state {
            State::Header(_) => (0, TarAt::Boundary),
            State::File {
                item, data_start, ..
            } => (
                self.tar_pos - data_start,
                TarAt::File {
                    path: item.path.clone(),
                    size: item.size,
                    mode: item.mode,
                    mtime: item.mtime.and_then(fsutil::epoch_secs),
                    data_start: *data_start,
                },
            ),
            State::Skip(left) => (0, TarAt::Skip { left: *left }),
            State::Trailer => (0, TarAt::End),
        };
        self.feed.point(self.tar_pos, at, written)
    }

    /// The archive byte a resume point here would restart in. Cheap, for
    /// the scan, which only measures.
    pub(super) fn point_byte(&self) -> Option<u64> {
        self.feed.point_byte()
    }
}

pub(super) enum Group {
    /// This many more bytes are needed to tell.
    Need(usize),
    /// The records end with an entry's own header.
    Complete,
    /// A zero block: the end of the archive.
    End,
}

/// Whether `held`, the records seen since the last entry ended, is a whole
/// entry header: any GNU long-name and long-link records and a pax record,
/// each with its data, then the entry's own header.
pub(super) fn group(held: &[u8]) -> Result<Group> {
    let mut at = 0usize;
    loop {
        if held.len() < at + 512 {
            return Ok(Group::Need(at + 512 - held.len()));
        }
        let block = &held[at..at + 512];
        if block.iter().all(|b| *b == 0) {
            if at == 0 {
                return Ok(Group::End);
            }
            bail!("reading the tar headers: a long-name or pax record has no entry after it");
        }
        match block[156] {
            b'L' | b'K' | b'x' => {
                let size = ::tar::Header::from_byte_slice(block)
                    .entry_size()
                    .context("reading the tar headers")?;
                let next = (at as u64 + 512).saturating_add(size.saturating_add(511) & !511);
                if next > MAX_HEADER {
                    bail!("reading the tar headers: an entry's header records exceed 1 MiB");
                }
                at = next as usize;
            }
            _ => return Ok(Group::Complete),
        }
    }
}

/// Turns a complete set of header records into the entry they describe,
/// moving `state` past them and `tar_pos` to the data that follows. `None`
/// means the records describe nothing to extract, like padding.
pub(super) fn begin_entry(
    tar_pos: &mut u64,
    state: &mut State,
    held: &[u8],
) -> Result<Option<Event<'static>>> {
    let last = &held[held.len() - 512..];
    if last[156] == b'S' {
        let name = ::tar::Header::from_byte_slice(last)
            .path_bytes()
            .into_owned();
        bail!(
            "'{}' is a sparse file, which is not supported",
            String::from_utf8_lossy(&name)
        );
    }
    let mut archive = ::tar::Archive::new(Cursor::new(held));
    let mut entries = archive.entries().context("reading the tar headers")?;
    let mut e = entries
        .next()
        .ok_or_else(|| anyhow!("reading the tar headers: an entry is incomplete"))?
        .context("reading the tar headers")?;
    let size = e.size();
    let described = super::tar::describe(&mut e)?;
    let data_start = *tar_pos;
    let span = size
        .checked_next_multiple_of(512)
        .ok_or_else(|| anyhow!("reading the tar headers: an entry size is invalid"))?;
    Ok(match described {
        Some((item, _)) if item.kind == Kind::File => {
            *state = State::File {
                item: item.clone(),
                data_start,
                left: size,
                pad: span - size,
            };
            Some(Event::Entry {
                item,
                link: None,
                done: 0,
            })
        }
        Some((item, link)) => {
            *state = State::Skip(span);
            Some(Event::Entry {
                item,
                link,
                done: 0,
            })
        }
        None => {
            *state = State::Skip(span);
            None
        }
    })
}

/// Whether the stream can end with the parser in `state`: it takes the
/// end-of-archive marker, a final entry that GNU tar allows to have no
/// marker, or the trailer after it. Anything else ends the tar early.
pub(super) fn end_state(state: &State) -> Result<Event<'static>> {
    match state {
        State::Trailer => Ok(Event::End),
        State::Header(held) if held.iter().all(|b| *b == 0) => Ok(Event::End),
        _ => bail!("the tar archive inside the compressed stream ends early"),
    }
}

/// The part of the tar state of a resume point that comes before the
/// compressed data's offsets. For gzip, whether the point falls in a header
/// or the entry data, and how much was written.
pub(super) fn resumed_state(s: &StreamPoint, written: u64) -> Result<(State, Option<(Item, u64)>)> {
    Ok(match &s.at {
        TarAt::Boundary => (State::Header(Vec::new()), None),
        TarAt::Skip { left } => (State::Skip(*left), None),
        TarAt::End => (State::Trailer, None),
        TarAt::File {
            path,
            size,
            mode,
            mtime,
            data_start,
        } => {
            let done = s.tar_pos.checked_sub(*data_start);
            let safe = safe_path(&path.to_string_lossy()).as_ref() == Some(path);
            if done != Some(written) || written > *size || !safe {
                bail!(
                    "the journal's resume point is inconsistent; the archive will not be modified"
                );
            }
            let item = Item {
                path: path.clone(),
                size: *size,
                kind: Kind::File,
                mode: *mode,
                mtime: mtime.and_then(fsutil::from_epoch_secs),
            };
            let state = State::File {
                item: item.clone(),
                data_start: *data_start,
                left: size - written,
                pad: size.next_multiple_of(512) - size,
            };
            (state, Some((item, written)))
        }
    })
}

/// Walks the archive from `from`, or from its start, listing what is left
/// and measuring it. Damage anywhere is found here, before anything is
/// touched.
pub struct Walker {
    inner: TarStream<Gzip>,
}

impl Walker {
    fn new(storage: &Storage, from: Option<&Resume>) -> Result<Self> {
        let reader = storage.reader(None);
        let end = storage.original_len();
        let gz = match from {
            None => Gzip::new(reader, end, &GzPoint::start())?,
            Some(r) => {
                let s = r.stream.as_ref().ok_or_else(|| {
                    anyhow!("the journal's resume point does not belong to a compressed tar")
                })?;
                if s.kind != CodecKind::Gzip {
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
                Gzip::new(
                    reader,
                    end,
                    &GzPoint {
                        bit: r.bit,
                        member_start: s.member_start,
                        crc: s.member_crc,
                        len: s.member_len,
                        window: s.window.clone(),
                    },
                )?
            }
        };
        Ok(Self {
            inner: TarStream::new(gz, from)?,
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
        format: Box::new(TarGz { solid }),
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

struct TarGz {
    solid: Solid,
}

impl Format for TarGz {
    fn worker(&self, _threads: u32) -> Result<Box<dyn Worker + '_>> {
        Ok(Box::new(TarGzWorker { solid: &self.solid }))
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
struct TarGzWorker<'a> {
    solid: &'a Solid,
}

impl Worker for TarGzWorker<'_> {
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

/// Bytes as a hex string, which keeps the journal readable JSON.
mod hex {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push_str(&format!("{b:02x}"));
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        if !s.len().is_multiple_of(2) {
            return Err(D::Error::custom("odd-length hex string"));
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(D::Error::custom))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0.1.8 reads this journal structure and has no defaults for the gzip
    /// fields, so a point must write them even at their defaults. Only the
    /// fields added after it may stay out.
    #[test]
    fn a_gzip_point_writes_every_field_older_builds_read() {
        let point = StreamPoint {
            kind: CodecKind::Gzip,
            member_start: false,
            member_crc: 0,
            member_len: 0,
            window: Vec::new(),
            block: 0,
            level: None,
            combined_crc: None,
            tar_pos: 0,
            at: TarAt::Boundary,
        };
        let json = serde_json::to_value(&point).unwrap();
        for field in [
            "member_start",
            "member_crc",
            "member_len",
            "window",
            "tar_pos",
        ] {
            assert!(json.get(field).is_some(), "{field} missing from {json}");
        }
        assert!(json.get("kind").is_none(), "kind is new and stays out");
        assert!(json.get("block").is_none(), "block is new and stays out");
        assert!(json.get("level").is_none(), "level is new and stays out");
        assert!(
            json.get("combined_crc").is_none(),
            "combined_crc is new and stays out"
        );
    }
}
