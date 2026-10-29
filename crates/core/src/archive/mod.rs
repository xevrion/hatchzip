//! What the engine needs from an archive format: the archive split into
//! units, byte ranges that can be truncated away once every file in them is
//! safely on disk, and a way to decode one unit.

#[cfg(feature = "rar")]
mod rar;
pub mod resumable;
mod sevenz;
pub mod storage;
mod tar;
mod tarbz2;
pub mod targz;
pub mod tarxz;
mod xz;
mod zip;

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

use crate::journal::Resume;
use storage::{Storage, Tail};
use targz::Event;
use targz::Profile;

/// An archive that is one compressed stream, decodable only from the front:
/// everything it holds is inside one decode pass, so instead of units
/// extracted from the end, the whole archive is one unit extracted front to
/// back, and the engine frees the compressed bytes behind the decoder. See
/// `targz` and `tarxz`.
pub trait SolidStream: Send + Sync {
    /// What a scan measured about the output: see [`targz::Profile`].
    fn profile(&self) -> &targz::Profile;
    /// Walks the archive from `from`, or from its start. The walk depends
    /// on nothing but the archive, so the box outlives the cloneable
    /// handle it came from.
    fn walk(&self, from: Option<&Resume>) -> Result<Box<dyn SolidWalker>>;
}

/// The walk a [`SolidStream`] produces: the tar's events and, at the
/// [`Event::Point`] ones, how decoding can restart.
pub trait SolidWalker {
    /// The next event. `Data` is at most `max` bytes, borrowed until the
    /// next call.
    fn next(&mut self, max: usize) -> Result<Event<'_>>;
    /// The resume point at the `Point` event just returned. Its `unit` is
    /// left for the engine to fill in.
    fn point(&self) -> Result<Resume>;
    /// The archive byte a resume point here would restart in. Cheap: for
    /// measuring only.
    fn point_byte(&self) -> Option<u64>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// The link target is the item's data.
    Symlink,
    /// Hard links point at an earlier entry, which is extracted later than
    /// the link when going from the end, so they are made at the very end.
    HardLink(PathBuf),
}

#[derive(Clone, Debug)]
pub struct Item {
    pub path: PathBuf,
    pub size: u64,
    pub kind: Kind,
    pub mode: Option<u32>,
    pub mtime: Option<SystemTime>,
}

pub struct Unit {
    /// Where the unit's bytes begin. Once it is committed the archive is
    /// truncated to this offset.
    pub start: u64,
    /// The format's own handle for the unit: a ZIP entry index, a 7z block.
    pub key: usize,
    pub items: Vec<Item>,
    /// Decoding it needs the archive's password.
    pub encrypted: bool,
}

/// The archive is encrypted and nothing can be read without its password.
/// Front ends look for this error to ask for one.
#[derive(Debug)]
pub struct PasswordNeeded;

impl std::fmt::Display for PasswordNeeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("this archive is password protected")
    }
}

impl std::error::Error for PasswordNeeded {}

/// The password doesn't open the archive.
#[derive(Debug)]
pub struct WrongPassword;

impl std::fmt::Display for WrongPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("incorrect password")
    }
}

impl std::error::Error for WrongPassword {}

/// The archive is damaged, found while opening it and before anything was
/// touched. Front ends look for this error to report damage as such.
#[derive(Debug)]
pub struct Damaged(pub anyhow::Error);

impl std::fmt::Display for Damaged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for Damaged {}

/// The archive is valid but uses something this build does not extract: an
/// unknown or unprovided check, an unknown filter, several xz streams, or a
/// dictionary too large to hold. Front ends report it as unsupported, never
/// as damage.
#[derive(Debug)]
pub struct Unsupported(pub anyhow::Error);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for Unsupported {}

/// The format name of RAR archives in the journal.
pub const RAR: &str = "rar";

/// Whether this build extracts RAR archives. Builds without UnRAR refuse
/// them before anything is touched.
pub const RAR_SUPPORTED: bool = cfg!(feature = "rar");

/// Why a build without UnRAR refuses a RAR archive.
pub fn no_rar() -> anyhow::Error {
    anyhow::anyhow!(
        "this build of HatchZip does not include RAR support, and the archive was not \
         modified. Download the release build from hatchzip.dev or \
         github.com/xevrion/hatchzip/releases"
    )
}

/// How the password of an encrypted archive is proven before anything is
/// touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasswordCheck {
    /// Decode the start of the smallest encrypted unit: a wrong password
    /// fails at once, or within the first blocks.
    Sample,
    /// The headers hold a check value for the password, so
    /// [`Format::prove_password`] answers without decoding anything.
    Headers,
    /// Only the checksum at the end of a file can tell, so the smallest
    /// encrypted unit is decoded whole, writing nothing.
    WholeUnit,
}

impl Unit {
    /// Saturates instead of wrapping: sizes come straight from the archive.
    pub fn size(&self) -> u64 {
        self.items
            .iter()
            .fold(0u64, |sum, i| sum.saturating_add(i.size))
    }
}

pub trait Format: Send + Sync {
    /// A worker decodes units on one thread. `threads` is a hint for codecs
    /// that can decode a single stream on several cores.
    fn worker(&self, threads: u32) -> Result<Box<dyn Worker + '_>>;

    /// For a unit holding one file whose data [`resumable`] can decode from
    /// the middle: where that data lies. Such units can free their archive
    /// bytes while they are extracted instead of only once they are done.
    fn stream(&self, _unit: &Unit) -> Result<Option<Stream>> {
        Ok(None)
    }

    /// For an archive that is one compressed stream, decodable only from the
    /// front: its single unit, extracted front to back while the bytes behind
    /// the decoder are freed.
    fn solid(&self) -> Option<&dyn SolidStream> {
        None
    }

    /// The decoder finds a unit only by reading everything before it, and
    /// one handle can't be shared between threads. A batch is then decoded
    /// on one thread by one worker, lowest unit first, so the worker can
    /// walk forward instead of starting over for every unit.
    fn serial(&self) -> bool {
        false
    }

    /// Why the archive can't be extracted in place, if it can't: it can
    /// still be extracted while it is kept.
    fn refuses_in_place(&self) -> Option<String> {
        None
    }

    fn password_check(&self) -> PasswordCheck {
        PasswordCheck::Sample
    }

    /// Proves the password from the headers alone; only called when
    /// [`password_check`](Self::password_check) says it can.
    fn prove_password(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Stored,
    /// Raw deflate, as ZIP stores it.
    Deflate,
}

#[derive(Clone, Debug)]
pub struct Stream {
    /// Where the file's data begins. The headers before it stay untouched
    /// until the unit is committed, so the archive still parses on resume.
    pub data_start: u64,
    /// Length of the data in the archive.
    pub packed: u64,
    pub codec: Codec,
    /// CRC-32 of the output, if the format records one.
    pub crc: Option<u32>,
}

pub trait Worker {
    /// Decodes `unit`, handing each item to `sink` in order.
    fn extract(&mut self, unit: &Unit, sink: &mut dyn Sink) -> Result<()>;
}

pub trait Sink {
    fn item(&mut self, item: &Item, data: &mut dyn Read) -> Result<()>;
}

pub struct Opened {
    pub name: &'static str,
    pub format: Box<dyn Format>,
    pub units: Vec<Unit>,
    /// Everything from here to the end is saved before anything is reclaimed.
    pub tail_start: u64,
}

/// Parses the archive. `resume` carries the format recorded by an earlier
/// run, the tail it saved and its resume point, which a compressed tar is
/// read from; a fresh run sniffs the format instead. The password is only
/// needed for encrypted archives; whether it is right is found out when a
/// unit is decoded (see `Job::check_password`).
pub fn open(
    storage: &Arc<Storage>,
    resume: Option<(&str, Arc<Tail>, Option<&Resume>)>,
    password: Option<&str>,
) -> Result<Opened> {
    let (name, tail, point) = match resume {
        Some((name, tail, point)) => (name.to_string(), Some(tail), point),
        None => (sniff(storage)?.to_string(), None, None),
    };
    match name.as_str() {
        "zip" => zip::open(storage, tail, password),
        "7z" => sevenz::open(storage, tail, password),
        "tar" => tar::open(storage, tail),
        #[cfg(feature = "rar")]
        RAR => rar::open(storage, tail.is_some(), password),
        #[cfg(not(feature = "rar"))]
        RAR => Err(no_rar()),
        targz::NAME => targz::open(storage, point),
        tarbz2::NAME => tarbz2::open(storage, point),
        tarxz::NAME => tarxz::open(storage, point),
        other => bail!("unknown archive format '{other}' in the journal"),
    }
}

/// What turns the first `keep` bytes of a half-extracted archive back into an
/// archive any tool opens.
pub struct NewIndex {
    /// Written at `keep`.
    pub append: Vec<u8>,
    /// Written over bytes below `keep`: the 7z start header, which has to
    /// point at the new header.
    pub patches: Vec<(u64, Vec<u8>)>,
}

/// Lists the entries of a [`SolidStream`]'s walk from `from` once to the
/// end, and measures how the output runs ahead of the decoded bytes, which
/// the space check needs. Damage found here, in the pass that runs before
/// anything is touched and again from the resume point of a resumed run,
/// stops the run.
fn scan(solid: &dyn SolidStream, from: Option<&Resume>) -> Result<(Vec<Item>, Profile)> {
    let from_byte = from.map_or(0, |r| r.bit / 8);
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
                let consumed = w
                    .point_byte()
                    .unwrap_or(from_byte)
                    .saturating_sub(from_byte);
                p.lead = p.lead.max(out as i128 - consumed as i128);
                p.gap = p.gap.max(out - last);
                last = out;
            }
            Event::End => break,
        }
    }
    p.gap = p.gap.max(out - last);
    p.files = out;
    tar::size_links(&mut items);
    p.links = items
        .iter()
        .filter(|i| i.kind != Kind::File)
        .fold(0u64, |sum, i| {
            let size = i.size.checked_next_multiple_of(4096).unwrap_or(u64::MAX);
            sum.saturating_add(size)
        });
    Ok((items, p))
}

/// Builds the [`NewIndex`] for the entries that start below `keep`, from
/// the tail an earlier run saved. `hard_links` are tar links already cut off
/// whose targets are still in the archive.
pub fn rebuild_index(
    format: &str,
    storage: &Storage,
    tail: Arc<Tail>,
    keep: u64,
    hard_links: &[(&Path, &Path)],
) -> Result<NewIndex> {
    let (append, patches) = match format {
        "zip" => (zip::rebuild_index(&tail, keep)?, Vec::new()),
        "tar" => (tar::closing(hard_links)?, Vec::new()),
        "7z" => {
            let (header, start) = sevenz::rebuild_header(storage, tail, keep)?;
            (header, vec![(0, start)])
        }
        // No index: the last volume left only needs its end header back.
        #[cfg(feature = "rar")]
        RAR => (rar::closing(storage, keep)?, Vec::new()),
        #[cfg(not(feature = "rar"))]
        RAR => return Err(no_rar()),
        other => bail!("unknown archive format '{other}' in the journal"),
    };
    Ok(NewIndex { append, patches })
}

fn sniff(storage: &Storage) -> Result<&'static str> {
    let mut head = [0u8; 512];
    let mut r = storage.reader(None);
    let n = read_up_to(&mut r, &mut head)?;
    let head = &head[..n];
    let starts = |magic: &[u8]| head.starts_with(magic);
    if starts(b"7z\xBC\xAF\x27\x1C") {
        return Ok("7z");
    }
    let is_tar = |head: &[u8]| head.len() >= 263 && &head[257..262] == b"ustar";
    if is_tar(head) {
        return Ok("tar");
    }
    let ext = storage.primary().to_string_lossy().to_ascii_lowercase();
    if starts(b"\x1F\x8B") {
        // Look inside: a plain .gz of one file is not an archive.
        let mut gz = resumable::Gzip::new(
            storage.reader(None),
            storage.original_len(),
            &resumable::GzPoint::start(),
        )?;
        let mut first = [0u8; 512];
        let mut n = 0;
        while n < first.len() {
            match gz
                .read(&mut first[n..])
                .context("reading the start of the compressed stream")?
            {
                0 => break,
                k => n += k,
            }
        }
        let named = [".tar.gz", ".tgz", ".taz"]
            .iter()
            .any(|e| ext.ends_with(e) || ext.contains(&format!("{e}.")));
        if is_tar(&first[..n]) || named {
            return Ok(targz::NAME);
        }
        bail!(
            "this is a gzip-compressed file that does not contain a tar archive. Only \
             compressed tar archives (.tar.gz) can be extracted"
        );
    }
    if starts(b"\xFD7zXZ\x00") {
        // A .xz only ever comes as a compressed tar here; anything else is
        // refused once the stream opens.
        return Ok(tarxz::NAME);
    }
    let named_bzip2 = named_bzip2(
        &storage
            .primary()
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
    );
    if starts(b"BZh") || named_bzip2 {
        let first = (|| -> Result<_> {
            let mut bz = tarbz2::Bzip2::new(
                storage.reader(None),
                storage.original_len(),
                &tarbz2::BzPoint::start(),
            )?;
            let mut first = [0; 512];
            let mut n = 0;
            while n < first.len() {
                let k = bz.read(&mut first[n..])?;
                if k == 0 {
                    break;
                }
                n += k;
            }
            Ok((first, n))
        })()
        .map_err(|e| anyhow::Error::new(Damaged(e)))
        .context(
            "checking the bzip2 archive (nothing was changed); if it is damaged, download it again",
        )?;
        if is_tar(&first.0[..first.1]) || named_bzip2 {
            return Ok(tarbz2::NAME);
        }
        bail!(
            "this is a bzip2-compressed file that does not contain a tar archive; the file was not changed. Decompress it with bzip2, or use a .tar.bz2 archive"
        );
    }
    if starts(b"\x28\xB5\x2F\xFD") {
        bail!(
            "this is a compressed stream (.zst). In-place extraction of these \
             compressed streams is not supported yet"
        );
    }
    if starts(b"Rar!\x1A\x07") {
        return Ok(RAR);
    }
    if ext.ends_with(".tar") || ext.contains(".tar.") {
        return Ok("tar");
    }
    // Anything else gets a ZIP parse: self-extracting archives put an
    // executable in front, so the signature isn't at the start.
    Ok("zip")
}

/// The file's own name says `.tar.bz2`, alone or before a numeric split
/// suffix such as `.001`. The folders it sits in don't count.
fn named_bzip2(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let base = match name.rsplit_once('.') {
        Some((base, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => &name,
    };
    [".tar.bz2", ".tbz2", ".tbz"]
        .iter()
        .any(|e| base.ends_with(e))
}

fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Turns an archive member name into a relative path that can't leave the
/// output folder. Leading slashes are dropped, as tar does; `..`, drive
/// letters and NUL bytes are refused.
pub fn safe_path(name: &str) -> Option<PathBuf> {
    if name.contains('\0') {
        return None;
    }
    let mut out = PathBuf::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return None,
            p if p.contains(':') => return None,
            p => out.push(p),
        }
    }
    let clean = out.components().all(|c| matches!(c, Component::Normal(_)));
    (clean && !out.as_os_str().is_empty()).then_some(out)
}

/// Unix permission bits as 7-Zip and Info-ZIP store them in the high half of
/// the Windows attributes.
fn unix_mode_from_attributes(attrs: u32) -> Option<u32> {
    (attrs & 0x8000 != 0).then_some(attrs >> 16)
}

const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

fn is_symlink_mode(mode: Option<u32>) -> bool {
    mode.is_some_and(|m| m & S_IFMT == S_IFLNK)
}

#[cfg(test)]
mod named_tests {
    use super::named_bzip2;

    #[test]
    fn only_the_file_name_and_a_split_number_select_bzip2() {
        for yes in ["a.tar.bz2", "A.TBZ2", "a.tbz", "a.tar.bz2.001", "a.tbz2.12"] {
            assert!(named_bzip2(yes), "{yes}");
        }
        for no in [
            "a.zip",
            "a.tar.bz2.old",
            "a.tar.bz2.",
            "a.bz2",
            "a.tar.gz.001",
        ] {
            assert!(!named_bzip2(no), "{no}");
        }
    }
}
