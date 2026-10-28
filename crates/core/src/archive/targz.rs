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

use std::io::{self, Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::resumable::{GzPoint, Gzip};
use super::storage::Storage;
use super::{Format, Item, Kind, Opened, Sink, Unit, Worker, safe_path};
use crate::fsutil;
use crate::journal::Resume;

pub const NAME: &str = "tar.gz";

/// Decoded output is handed out in pieces of at most this much.
const BUF: usize = 1024 * 1024;

/// Long-name and pax records in front of one entry, together. GNU tar and
/// pax write a few hundred bytes; anything this large is refused.
const MAX_HEADER: u64 = 1024 * 1024;

/// The decoder's and the tar parser's state at a resume point, beyond the
/// bit position and output length the journal's [`Resume`] already holds.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct StreamPoint {
    /// The point is where a gzip member's header begins, not a deflate block
    /// boundary inside a member.
    pub member_start: bool,
    /// CRC-32 and length of the member's output before the point, which
    /// its trailer is checked against.
    pub member_crc: u32,
    pub member_len: u64,
    /// The member's last 32 KiB of output before the point, which deflate
    /// back-references may reach into. It is part tar headers and part
    /// several files, so unlike a single ZIP entry's window it can't be
    /// read back from one output file.
    #[serde(with = "hex")]
    pub window: Vec<u8>,
    /// Offset of the point in the uncompressed tar.
    pub tar_pos: u64,
    pub at: TarAt,
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
    pub profile: Profile,
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
    /// The tar ended, and so did the gzip stream, every member checked.
    End,
}

enum State {
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

pub struct Walker {
    gz: Gzip,
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

impl Walker {
    fn new(storage: &Storage, from: Option<&Resume>) -> Result<Self> {
        let reader = storage.reader(None);
        let end = storage.original_len();
        let (gz, tar_pos, state, resumed) = match from {
            None => (
                Gzip::new(reader, end, &GzPoint::start())?,
                0,
                State::Header(Vec::new()),
                None,
            ),
            Some(r) => {
                let s = r.stream.as_ref().ok_or_else(|| {
                    anyhow!("the journal's resume point does not belong to a compressed tar")
                })?;
                let gz = Gzip::new(
                    reader,
                    end,
                    &GzPoint {
                        bit: r.bit,
                        member_start: s.member_start,
                        crc: s.member_crc,
                        len: s.member_len,
                        window: s.window.clone(),
                    },
                )?;
                let (state, resumed) = resumed_state(s, r.written)?;
                (gz, s.tar_pos, state, resumed)
            }
        };
        Ok(Self {
            gz,
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
    pub fn next(&mut self, max: usize) -> Result<Event<'_>> {
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
                if std::mem::take(&mut self.restartable) && self.at_rest() {
                    return Ok(Event::Point);
                }
                let n = self.gz.read(&mut self.buf)?;
                if n == 0 {
                    return self.end();
                }
                self.pos = 0;
                self.len = n;
                self.restartable = self.gz.restart_bit().is_some();
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
                        if let Some(event) = self.begin(&held)? {
                            return Ok(event);
                        }
                    }
                },
            }
        }
    }

    /// Whether the parser's state here can be written down in a
    /// [`StreamPoint`]. Inside an entry's header records it can't.
    fn at_rest(&self) -> bool {
        match &self.state {
            State::Header(held) => held.is_empty(),
            State::File { .. } | State::Skip(_) | State::Trailer => true,
        }
    }

    /// The resume point at the `Point` event just returned. Its `unit` is
    /// left for the engine to fill in.
    pub fn point(&self) -> Result<Resume> {
        let gz = self
            .gz
            .restart_point()?
            .ok_or_else(|| anyhow!("the compressed stream cannot be resumed here"))?;
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
        Ok(Resume {
            unit: 0,
            written,
            bit: gz.bit,
            crc: 0,
            stream: Some(Box::new(StreamPoint {
                member_start: gz.member_start,
                member_crc: gz.crc,
                member_len: gz.len,
                window: gz.window,
                tar_pos: self.tar_pos,
                at,
            })),
        })
    }

    /// The archive byte a resume point here would restart in. Cheap, for
    /// the scan, which only measures.
    fn point_byte(&self) -> Option<u64> {
        self.gz.restart_bit().map(|(bit, _)| bit / 8)
    }

    /// Turns a complete set of header records into the entry they describe.
    fn begin(&mut self, held: &[u8]) -> Result<Option<Event<'static>>> {
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
        let e = entries
            .next()
            .ok_or_else(|| anyhow!("reading the tar headers: an entry is incomplete"))?
            .context("reading the tar headers")?;
        let size = e.size();
        let described = super::tar::describe(&e)?;
        let data_start = self.tar_pos;
        let span = size
            .checked_next_multiple_of(512)
            .ok_or_else(|| anyhow!("reading the tar headers: an entry size is invalid"))?;
        Ok(match described {
            Some((item, _)) if item.kind == Kind::File => {
                self.state = State::File {
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
                self.state = State::Skip(span);
                Some(Event::Entry {
                    item,
                    link,
                    done: 0,
                })
            }
            None => {
                self.state = State::Skip(span);
                None
            }
        })
    }

    fn end(&self) -> Result<Event<'static>> {
        match &self.state {
            State::Trailer => Ok(Event::End),
            // No end-of-archive marker, or part of one: GNU tar accepts
            // both, and so does the tar crate for uncompressed tar.
            State::Header(held) if held.iter().all(|b| *b == 0) => Ok(Event::End),
            _ => bail!("the tar archive inside the compressed stream ends early"),
        }
    }
}

enum Group {
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
fn group(held: &[u8]) -> Result<Group> {
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

fn resumed_state(s: &StreamPoint, written: u64) -> Result<(State, Option<(Item, u64)>)> {
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

struct TarGz {
    solid: Solid,
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
    let (items, profile) = scan(&solid, from)
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

fn scan(solid: &Solid, from: Option<&Resume>) -> Result<(Vec<Item>, Profile)> {
    let start = from.map_or(0, |r| r.bit / 8);
    let mut w = solid.walk(from)?;
    let mut items = Vec::new();
    let mut p = Profile::default();
    let (mut out, mut last, mut size) = (0u64, 0u64, 0u64);
    loop {
        match w.next(usize::MAX)? {
            Event::Entry { item, .. } => {
                if item.kind == Kind::File {
                    size = item.size;
                }
                items.push(item);
            }
            Event::Data(d) => out += d.len() as u64,
            Event::FileEnd => out += size.next_multiple_of(4096) - size,
            Event::Point => {
                let consumed = w.point_byte().unwrap_or(start).saturating_sub(start);
                p.lead = p.lead.max(out as i128 - consumed as i128);
                p.gap = p.gap.max(out - last);
                last = out;
            }
            Event::End => break,
        }
    }
    p.gap = p.gap.max(out - last);
    p.files = out;
    super::tar::size_links(&mut items);
    p.links = items
        .iter()
        .filter(|i| i.kind != Kind::File)
        .fold(0u64, |sum, i| {
            let size = i.size.checked_next_multiple_of(4096).unwrap_or(u64::MAX);
            sum.saturating_add(size)
        });
    Ok((items, p))
}

impl Format for TarGz {
    fn worker(&self, _threads: u32) -> Result<Box<dyn Worker + '_>> {
        Ok(Box::new(TarGzWorker { solid: &self.solid }))
    }

    fn solid(&self) -> Option<&Solid> {
        Some(&self.solid)
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
