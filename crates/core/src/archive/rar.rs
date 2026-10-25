//! RAR archives: one `.rar` file or a volume set (`name.part1.rar`,
//! `name.part2.rar`, ... or `name.rar`, `name.r00`, ...).
//!
//! A RAR archive that is not solid stores its files one after another, each
//! header followed by the file's data, much like tar. So every file is a
//! unit: the headers of every volume are read here, files are extracted from
//! the last one first, and after each batch the archive is truncated to the
//! header of the lowest file in it. A volume is deleted once every file that
//! starts in it or reaches into it is out. A file split over several volumes
//! belongs to the volume it starts in, so its later pieces only go once all
//! of it is committed. What is left is always a prefix of the set whose
//! headers are untouched, which UnRAR still reads.
//!
//! Decoding is UnRAR's, through its C interface. UnRAR finds a file only by
//! reading every header before it, so a batch is decoded on one thread,
//! lowest file first, by one archive handle that walks forward. A helper
//! thread turns UnRAR's data callback into the reader the engine's sinks
//! take. A stored file that lies whole in one volume is plain bytes at a
//! known offset, so the engine can stream it and free its space as it goes,
//! as it does for tar.
//!
//! A solid archive compresses every file against the ones before it and
//! decodes only front to back, and UnRAR can't save its decoder's state, so
//! a crash could never resume. It is one unit, extracted only while the
//! archive is kept.
//!
//! An archive made with "Encrypt file names" (`rar -hp`) encrypts its
//! headers too. They are decrypted here with the password (see `crypt`), so
//! the same parser reads them, and every offset is the real one on disk:
//! a file starts at the IV or salt in front of its header, which is where a
//! truncation cuts.

mod crypt;

use std::collections::{HashMap, HashSet};
use std::ffi::{c_int, c_uint};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use unrar_ng_sys as native;

use super::storage::{Part, Reader, Storage};
use super::{
    Codec, Damaged, Format, Item, Kind, Opened, PasswordCheck, PasswordNeeded, Sink, Stream, Unit,
    Worker, WrongPassword, is_symlink_mode, safe_path,
};
use crate::platform;

pub use super::RAR as NAME;

const SIG5: &[u8] = b"Rar!\x1A\x07\x01\x00";
const SIG4: &[u8] = b"Rar!\x1A\x07\x00";

/// RAR 5 headers are at most 2 MiB by the format's own limit.
const MAX_HEADER: u64 = 2 << 20;

/// UnRAR refuses a larger dictionary unless asked, and decoding with one
/// takes that much memory. RAR 7 can write up to 64 GB.
const MAX_DICT: u64 = 4 << 30;

/// UnRAR keeps a password to this many characters, terminator included.
const MAX_PASSWORD: usize = 128;

/// Room for the longest name UnRAR reads (`MAXPATHSIZE`).
const NAME_ROOM: usize = 0x10000;

/// Something this build can't extract, as opposed to damage. Refused before
/// anything is touched, like damage, but reported as what it is.
#[derive(Debug)]
struct Unsupported(String);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unsupported {}

fn unsupported(why: impl Into<String>) -> anyhow::Error {
    Unsupported(why.into()).into()
}

/// The RAR 5 password check value of an encrypted file: 64 bits derived from
/// the password with PBKDF2, so a wrong password is caught without decoding.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Check {
    /// Log2 of the PBKDF2 iteration count.
    count: u8,
    salt: [u8; 16],
    value: [u8; 8],
}

impl Check {
    /// What UnRAR's `CryptData::SetKey50` derives: PBKDF2-HMAC-SHA256 run 32
    /// iterations past the key, folded into 8 bytes.
    fn matches(&self, password: &str) -> bool {
        let mut derived = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(
            rar_password(password).as_bytes(),
            &self.salt,
            (1u32 << self.count) + 32,
            &mut derived,
        );
        let mut folded = [0u8; 8];
        for (i, b) in derived.iter().enumerate() {
            folded[i % 8] ^= b;
        }
        folded == self.value
    }
}

/// The password as UnRAR uses it: cut to `MAX_PASSWORD - 1` wide characters,
/// which are UTF-16 units on Windows and code points elsewhere.
fn rar_password(password: &str) -> String {
    let mut wide = wide(password);
    wide.truncate(MAX_PASSWORD - 1);
    wide_to_string(&wide)
}

/// What the headers of one file say, in one volume. A file that spans
/// volumes has one of these in each.
struct Header {
    volume: usize,
    /// Where the header begins in the joined archive.
    start: u64,
    /// Where the data in this volume begins, and how long it is.
    data: u64,
    packed: u64,
    name: String,
    rar5: bool,
    dir: bool,
    size: u64,
    before: bool,
    after: bool,
    encrypted: bool,
    check: Option<Check>,
    link: bool,
    solid: bool,
    stored: bool,
    crc: Option<u32>,
    dict: u64,
    /// A compression version UnRAR knows.
    known: bool,
    mode: Option<u32>,
    mtime: Option<SystemTime>,
}

#[derive(Default)]
struct Parsed {
    headers: Vec<Header>,
    /// Some volume says the archive is solid.
    solid: bool,
    /// The last volume read says more volumes follow.
    more: bool,
    /// Some volume is RAR 4, whose encrypted files have no check value.
    rar4: bool,
    /// Whether the first volume's headers are encrypted. UnRAR refuses a
    /// set whose volumes differ, and so does HatchZip.
    encrypted_headers: Option<bool>,
    /// The password is known to be right: it matched the check value of
    /// the archive encryption header, or a header decrypted with it passed
    /// its checksum. Until then, a header that fails is a wrong password;
    /// from then on, it is damage.
    proven: bool,
    /// Header keys derived so far, by salt (and iteration count). The
    /// derivation is slow on purpose, and the volumes of a set, or every
    /// header of a RAR 4 archive, name the same salt.
    keys5: HashMap<([u8; 16], u8), [u8; 32]>,
    keys4: Keys4,
}

/// RAR 4 header keys and IVs, by salt.
type Keys4 = HashMap<[u8; 8], ([u8; 16], [u8; 16])>;

impl Parsed {
    fn agree(&mut self, v: usize, encrypted: bool) -> Result<()> {
        match self.encrypted_headers {
            None => self.encrypted_headers = Some(encrypted),
            Some(first) if first != encrypted => bail!(
                "volume {} {} encrypted file names, and the first volume {}",
                v + 1,
                if encrypted { "has" } else { "does not have" },
                if first { "does" } else { "does not" }
            ),
            Some(_) => {}
        }
        Ok(())
    }
}

/// One file, joined from its pieces in every volume it spans.
struct File {
    /// The volumes it starts and ends in.
    first: usize,
    last: usize,
    /// Its first header, in the joined archive.
    start: u64,
    /// Where its data lies in each volume, in order.
    pieces: Vec<(u64, u64)>,
    name: String,
    rar5: bool,
    dir: bool,
    size: u64,
    encrypted: bool,
    check: Option<Check>,
    link: bool,
    solid: bool,
    stored: bool,
    crc: Option<u32>,
    dict: u64,
    known: bool,
    mode: Option<u32>,
    mtime: Option<SystemTime>,
}

impl File {
    /// The name as it is extracted. RAR 5 separates folders with `/` only,
    /// so a `\` is part of a name, which Windows can't hold; WinRAR writes
    /// it as `_` there, and so does HatchZip everywhere, so the output is
    /// the same on every system. RAR 4 names use `\`.
    fn path_name(&self) -> String {
        clean_name(&self.name, self.rar5)
    }

    fn item(&self) -> Result<Item> {
        let path = safe_path(&self.path_name()).ok_or_else(|| {
            anyhow!(
                "'{}' would be written outside the destination folder; extraction refused",
                self.name
            )
        })?;
        Ok(Item {
            path,
            size: if self.dir { 0 } else { self.size },
            kind: if self.dir { Kind::Dir } else { Kind::File },
            mode: self.mode,
            mtime: self.mtime,
        })
    }

    /// Whether UnRAR's name for this file extracts to the same place.
    fn same_name(&self, unrar: &str) -> bool {
        // UnRAR reports names with the separator of the system it runs on,
        // `\` on Windows; the headers always use `/` in RAR 5.
        let unrar = unrar.replace(std::path::MAIN_SEPARATOR, "/");
        let ours = safe_path(&self.path_name());
        ours.is_some() && ours == safe_path(&clean_name(&unrar, self.rar5))
    }
}

fn clean_name(name: &str, rar5: bool) -> String {
    if rar5 {
        name.replace('\\', "_")
    } else {
        name.to_string()
    }
}

struct Rar {
    storage: Arc<Storage>,
    password: Option<String>,
    files: Vec<File>,
    solid: bool,
    rar4: bool,
}

pub fn open(storage: &Arc<Storage>, resuming: bool, password: Option<&str>) -> Result<Opened> {
    let end = storage.current_len().context("reading the volumes")?;
    let volumes: Vec<_> = storage
        .parts()
        .iter()
        .take_while(|p| p.start < end)
        .collect();
    if volumes.is_empty() {
        if resuming {
            // Nothing of the set is left, so every file is out and only the
            // cleanup is left to do.
            return Ok(Opened {
                name: NAME,
                format: Box::new(Rar {
                    storage: storage.clone(),
                    password: password.map(str::to_string),
                    files: Vec::new(),
                    solid: false,
                    rar4: false,
                }),
                units: Vec::new(),
                tail_start: end,
            });
        }
        bail!("{} is empty", storage.primary().display());
    }

    let mut reader = storage.reader(None);
    let mut parsed = Parsed::default();
    for (v, part) in volumes.iter().enumerate() {
        // Volumes of an older, longer set with the same name can follow the
        // last one. They are not part of this set, and extracting it would
        // delete them.
        if v > 0 && !parsed.more {
            bail!(
                "{} follows the last volume of the set, so it belongs to another one. Move it \
                 out of the folder and try again",
                part.path.display()
            );
        }
        let live = (end - part.start).min(part.len);
        if let Err(e) = parse_volume(&mut reader, v, part, live, password, &mut parsed) {
            // Front ends ask for a password on these, so they stay as they are.
            if e.is::<PasswordNeeded>() || e.is::<WrongPassword>() {
                return Err(e);
            }
            let e = if e.is::<Unsupported>() {
                e
            } else {
                anyhow::Error::new(Damaged(e))
            };
            return Err(e.context(format!("reading {}", part.path.display())));
        }
    }
    if parsed.more && !resuming {
        bail!(
            "{} is not the whole set: the last volume found says more follow. Put every volume \
             in one folder",
            volumes.last().expect("checked above").path.display()
        );
    }

    let files = files(&parsed.headers, resuming).map_err(|e| anyhow::Error::new(Damaged(e)))?;
    refuse_unsupported(&files)?;
    let solid = parsed.solid || files.iter().any(|f| f.solid);

    // Extraction finds each file by the name UnRAR reports. A name UnRAR
    // reads differently, such as a legacy name in a code page, would only
    // be noticed halfway through, after volumes are gone.
    let listed = unrar_names(&volumes[0].path, files.len(), password)?;
    for (i, f) in files.iter().enumerate() {
        let theirs = listed.get(i);
        if !theirs.is_some_and(|n| f.same_name(n)) {
            bail!(
                "UnRAR reads the name of '{}' differently ({}), so it cannot be extracted \
                 reliably. The archive was not modified",
                f.name,
                theirs.map_or("missing".to_string(), |n| format!("'{n}'"))
            );
        }
    }

    let mut units = Vec::with_capacity(files.len());
    if solid {
        // Decoded front to back as one stream; see `Format::refuses_in_place`.
        if let Some(first) = files.first() {
            units.push(Unit {
                start: first.start,
                key: 0,
                items: files.iter().map(File::item).collect::<Result<_>>()?,
                encrypted: files.iter().any(|f| f.encrypted),
            });
        }
    } else {
        for (i, f) in files.iter().enumerate() {
            units.push(Unit {
                start: f.start,
                key: i,
                items: vec![f.item()?],
                encrypted: f.encrypted,
            });
        }
    }

    Ok(Opened {
        name: NAME,
        format: Box::new(Rar {
            storage: storage.clone(),
            password: password.map(str::to_string),
            files,
            solid,
            rar4: parsed.rar4,
        }),
        units,
        // There is no central index, and every volume carries its own
        // headers, so nothing needs saving.
        tail_start: end,
    })
}

/// What this build can't extract, refused before anything is touched.
fn refuse_unsupported(files: &[File]) -> Result<()> {
    if let Some(f) = files.iter().find(|f| f.link) {
        bail!(
            "'{}' is a link or a file reference. RAR archives with links are not supported yet",
            f.name
        );
    }
    if let Some(f) = files.iter().find(|f| !f.known) {
        bail!(
            "'{}' is compressed with a RAR version this build of HatchZip cannot decode",
            f.name
        );
    }
    if let Some(f) = files.iter().find(|f| f.dict > MAX_DICT) {
        bail!(
            "'{}' is compressed with a {} GB dictionary, which needs that much memory to extract. \
             HatchZip extracts RAR archives with dictionaries of up to 4 GB",
            f.name,
            f.dict >> 30
        );
    }
    Ok(())
}

/// Joins the per-volume headers of files that span volumes into one entry
/// per file, with the volumes it starts and ends in.
fn files(headers: &[Header], resuming: bool) -> Result<Vec<File>> {
    let mut files: Vec<File> = Vec::new();
    let mut open: Option<usize> = None;
    for h in headers {
        if h.before {
            let Some(at) = open.take() else {
                bail!(
                    "volume {} starts with the rest of '{}', but the volume before it does not \
                     end with its start",
                    h.volume + 1,
                    h.name
                );
            };
            let f = &mut files[at];
            if f.name != h.name || f.last + 1 != h.volume {
                bail!("the volumes do not line up at '{}'", h.name);
            }
            f.last = h.volume;
            f.pieces.push((h.data, h.packed));
            if h.after {
                open = Some(at);
            }
            continue;
        }
        if open.is_some() {
            bail!("the volumes do not line up before '{}'", h.name);
        }
        files.push(File {
            first: h.volume,
            last: h.volume,
            start: h.start,
            pieces: vec![(h.data, h.packed)],
            name: h.name.clone(),
            rar5: h.rar5,
            dir: h.dir,
            size: h.size,
            encrypted: h.encrypted,
            check: h.check.clone(),
            link: h.link,
            solid: h.solid,
            stored: h.stored,
            crc: h.crc,
            dict: h.dict,
            known: h.known,
            mode: h.mode,
            mtime: h.mtime,
        });
        if h.after {
            open = Some(files.len() - 1);
        }
    }
    // A resumed run reads only what is left, which ends where the last
    // commit cut it; never inside a file it still has to extract.
    if open.is_some() && !resuming {
        bail!("the last volume ends in the middle of a file; a volume is missing");
    }
    Ok(files)
}

// -- Headers --------------------------------------------------------------

struct Volume<'a> {
    r: &'a mut Reader,
    /// Where the volume begins in the joined archive.
    start: u64,
    /// Its original length, and how much of it is still there.
    len: u64,
    live: u64,
}

impl Volume<'_> {
    fn read(&mut self, at: u64, n: u64) -> Result<Vec<u8>> {
        if at.checked_add(n).is_none_or(|e| e > self.live) {
            bail!("a header runs past the end of the volume");
        }
        self.r.seek(SeekFrom::Start(self.start + at))?;
        let mut buf = vec![0; n as usize];
        self.r.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Whether `pos` is past what is left. Only a resumed run's last volume
    /// ends early, at a header, where an earlier run cut it.
    fn ended(&self, pos: u64) -> Result<bool> {
        if pos < self.live {
            return Ok(false);
        }
        if self.live < self.len {
            return Ok(true);
        }
        bail!("the volume has no end of archive header")
    }
}

fn parse_volume(
    reader: &mut Reader,
    v: usize,
    part: &Part,
    live: u64,
    password: Option<&str>,
    out: &mut Parsed,
) -> Result<()> {
    let mut vol = Volume {
        r: reader,
        start: part.start,
        len: part.len,
        live,
    };
    let sig = vol.read(0, 8.min(live))?;
    if sig.starts_with(SIG5) {
        parse5(&mut vol, v, password, out)
    } else if sig.starts_with(SIG4) {
        out.rar4 = true;
        parse4(&mut vol, v, password, out)
    } else {
        bail!("not a RAR volume (no signature at its start)")
    }
}

/// What reading the header at `pos` gave. While the password is not proven,
/// a header that fails to decrypt into a valid one means a wrong password;
/// a read that failed is still an error of its own.
fn decrypted<T>(read: Result<T>, encrypted: bool, out: &mut Parsed) -> Result<T> {
    match read {
        Ok(r) => {
            out.proven |= encrypted;
            Ok(r)
        }
        Err(e) if encrypted && !out.proven && !e.is::<io::Error>() => Err(WrongPassword.into()),
        Err(e) => Err(e),
    }
}

/// The password as RAR 4 derives its keys from it: UnRAR's wide characters,
/// cut to their low 16 bits, little-endian.
fn utf16_raw(password: &str) -> Vec<u8> {
    wide(&rar_password(password))
        .iter()
        .flat_map(|&c| {
            let c = c as u32;
            [c as u8, (c >> 8) as u8]
        })
        .collect()
}

/// Reads a RAR5 variable length integer: seven bits a byte, low first.
fn vint(b: &[u8], at: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *b.get(*at).ok_or_else(|| anyhow!("a header is cut short"))?;
        *at += 1;
        if shift < 64 {
            v |= u64::from(byte & 0x7F) << shift;
        }
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    bail!("a header holds an oversized number")
}

fn u32le(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes(s.try_into().expect("four bytes")))
        .ok_or_else(|| anyhow!("a header is cut short"))
}

fn u16le(b: &[u8], at: usize) -> Result<u16> {
    b.get(at..at + 2)
        .map(|s| u16::from_le_bytes(s.try_into().expect("two bytes")))
        .ok_or_else(|| anyhow!("a header is cut short"))
}

fn unknown_encryption() -> anyhow::Error {
    unsupported("this RAR archive uses an encryption method this build of HatchZip cannot decode")
}

/// One RAR 5 header as stored at `pos`, checked against its checksum: the
/// size field and the header after it, which the checksum covers, where the
/// header proper starts in those bytes, and where its data begins. With
/// `key`, the header is encrypted, behind its IV and padded to whole AES
/// blocks, as UnRAR's `ReadHeader50` reads it.
fn read5(vol: &mut Volume, pos: u64, key: Option<&[u8; 32]>) -> Result<(Vec<u8>, usize, u64)> {
    let size_of = |head: &[u8]| -> Result<(u32, usize, u64)> {
        let crc = u32le(head, 0)?;
        let mut at = 4;
        let size = vint(head, &mut at)?;
        if size == 0 || size > MAX_HEADER {
            bail!("a header has an impossible size");
        }
        Ok((crc, at, size))
    };
    let (crc, block, at, data_at) = match key {
        None => {
            let peek = vol.read(pos, 16.min(vol.live - pos))?;
            let (crc, at, size) = size_of(&peek)?;
            let block = vol.read(pos + 4, (at - 4) as u64 + size)?;
            (crc, block, at - 4, pos + at as u64 + size)
        }
        Some(key) => {
            let iv = vol.read(pos, crypt::BLOCK)?;
            let mut first = vol.read(pos + crypt::BLOCK, crypt::BLOCK)?;
            crypt::decrypt5(key, &iv, &mut first);
            let (crc, at, size) = size_of(&first)?;
            let len = at as u64 + size;
            let padded = len.next_multiple_of(crypt::BLOCK);
            let mut plain = vol.read(pos + crypt::BLOCK, padded)?;
            crypt::decrypt5(key, &iv, &mut plain);
            plain.truncate(len as usize);
            plain.drain(..4);
            (crc, plain, at - 4, pos + crypt::BLOCK + padded)
        }
    };
    if crc32fast::hash(&block) != crc {
        bail!("a header fails its checksum");
    }
    Ok((block, at, data_at))
}

fn parse5(vol: &mut Volume, v: usize, password: Option<&str>, out: &mut Parsed) -> Result<()> {
    let mut pos = 8u64;
    let mut saw_main = false;
    // Set by the archive encryption header: every header after it is
    // encrypted with this key.
    let mut key: Option<[u8; 32]> = None;
    loop {
        if vol.ended(pos)? {
            return Ok(());
        }
        let read = read5(vol, pos, key.as_ref());
        let (block, at, data_at) = decrypted(read, key.is_some(), out)?;
        let body = &block[at..];
        let mut p = 0;
        let kind = vint(body, &mut p)?;
        let flags = vint(body, &mut p)?;
        let extra = if flags & 1 != 0 {
            vint(body, &mut p)?
        } else {
            0
        };
        let data = if flags & 2 != 0 {
            vint(body, &mut p)?
        } else {
            0
        };
        let next = data_at
            .checked_add(data)
            .filter(|&n| n <= vol.len)
            .ok_or_else(|| anyhow!("a header's data runs past the end of the volume"))?;
        let extra_at = usize::try_from(extra)
            .ok()
            .and_then(|e| body.len().checked_sub(e))
            .filter(|&e| e >= p)
            .ok_or_else(|| anyhow!("a header's extra area is larger than the header"))?;
        // The archive encryption header comes first, right after the
        // signature, or not at all.
        if pos == 8 {
            out.agree(v, kind == 4)?;
        }
        match kind {
            1 => {
                let archive = vint(body, &mut p)?;
                if archive & 2 != 0 {
                    let number = vint(body, &mut p)?;
                    if number != v as u64 {
                        bail!(
                            "this is volume {} of its set, found in position {}",
                            number + 1,
                            v + 1
                        );
                    }
                }
                saw_main = true;
                out.solid |= archive & 4 != 0;
            }
            2 => {
                let mut h = file5(v, flags, &body[p..extra_at], &body[extra_at..])?;
                h.start = vol.start + pos;
                h.data = vol.start + data_at;
                h.packed = data;
                out.headers.push(h);
            }
            4 => {
                if pos != 8 {
                    bail!("an archive encryption header is out of place");
                }
                let (salt, count, check) = archive_crypt5(&body[p..extra_at])?;
                let Some(password) = password else {
                    return Err(PasswordNeeded.into());
                };
                if let Some(check) = check {
                    if !check.matches(password) {
                        return Err(WrongPassword.into());
                    }
                    out.proven = true;
                }
                let k = *out
                    .keys5
                    .entry((salt, count))
                    .or_insert_with(|| crypt::key5(&rar_password(password), &salt, count));
                key = Some(k);
            }
            5 => {
                let end = vint(body, &mut p)?;
                if !saw_main {
                    bail!("the volume has no main header");
                }
                out.more = end & 1 != 0;
                return Ok(());
            }
            _ => {}
        }
        pos = next;
    }
}

fn file5(v: usize, hflags: u64, b: &[u8], extra: &[u8]) -> Result<Header> {
    let mut p = 0;
    let flags = vint(b, &mut p)?;
    let size = vint(b, &mut p)?;
    let attr = vint(b, &mut p)?;
    let mtime = if flags & 2 != 0 {
        let t = u32le(b, p)?;
        p += 4;
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(u64::from(t)))
    } else {
        None
    };
    let crc = if flags & 4 != 0 {
        let c = u32le(b, p)?;
        p += 4;
        Some(c)
    } else {
        None
    };
    let info = vint(b, &mut p)?;
    let host = vint(b, &mut p)?;
    let n = usize::try_from(vint(b, &mut p)?).unwrap_or(usize::MAX);
    let name = b
        .get(p..p.saturating_add(n))
        .ok_or_else(|| anyhow!("a header is cut short"))?;
    if flags & 8 != 0 {
        bail!("an entry has no recorded size");
    }
    let dir = flags & 1 != 0;
    let mode = (host == 1).then_some(attr as u32);

    // Compression information: the algorithm's version in bits 0 to 5 (0 is
    // RAR 5, 1 is RAR 7), solid in bit 6, the method in bits 7 to 9 (0 is
    // stored), and the dictionary in bits 10 and up, as UnRAR reads it.
    let version = info & 0x3F;
    let dict = if dir || version > 1 {
        0
    } else {
        let base = 0x20000u64 << ((info >> 10) & if version == 0 { 0x0F } else { 0x1F });
        if version == 1 {
            base + base / 32 * ((info >> 15) & 0x1F)
        } else {
            base
        }
    };

    let (mut encrypted, mut check, mut link) = (false, None, is_symlink_mode(mode));
    let mut e = 0;
    while e < extra.len() {
        let len = usize::try_from(vint(extra, &mut e)?).unwrap_or(usize::MAX);
        let end = e
            .checked_add(len)
            .filter(|&end| end <= extra.len())
            .ok_or_else(|| anyhow!("a header's extra record runs past its end"))?;
        let kind = vint(extra, &mut e)?;
        match kind {
            1 => {
                encrypted = true;
                check = crypt5(&extra[e..end])?;
            }
            5 => link = true,
            _ => {}
        }
        e = end;
    }
    Ok(Header {
        volume: v,
        start: 0,
        data: 0,
        packed: 0,
        name: String::from_utf8_lossy(name).into_owned(),
        rar5: true,
        dir,
        size,
        before: hflags & 8 != 0,
        after: hflags & 0x10 != 0,
        encrypted,
        check,
        link,
        solid: info & 0x40 != 0,
        stored: (info >> 7) & 7 == 0,
        crc,
        dict,
        known: version <= 1,
        mode,
        mtime,
    })
}

/// The encryption record of a RAR 5 file: its password check value, if it
/// has a valid one.
fn crypt5(r: &[u8]) -> Result<Option<Check>> {
    let (flags, count, salt, p) = crypt_start5(r)?;
    // Then the 16 bytes of the AES initialization vector.
    check_value5(r, p + 16, flags, count, salt)
}

/// The archive encryption header of RAR 5, which comes before every
/// encrypted header: the salt and iteration count of their key, and the
/// password check value, if it has a valid one. It has no IV of its own;
/// every header after it carries one.
fn archive_crypt5(r: &[u8]) -> Result<([u8; 16], u8, Option<Check>)> {
    let (flags, count, salt, p) = crypt_start5(r)?;
    Ok((salt, count, check_value5(r, p, flags, count, salt)?))
}

/// What a file's encryption record and the archive encryption header begin
/// with: their flags, the log2 of the PBKDF2 iteration count, and the salt,
/// then where the rest begins.
fn crypt_start5(r: &[u8]) -> Result<(u64, u8, [u8; 16], usize)> {
    let mut p = 0;
    let version = vint(r, &mut p)?;
    let flags = vint(r, &mut p)?;
    let count = *r.get(p).ok_or_else(|| anyhow!("a header is cut short"))?;
    p += 1;
    // UnRAR's limits: encryption version 0, at most 2^24 iterations.
    if version > 0 || count > 24 {
        return Err(unknown_encryption());
    }
    let salt: [u8; 16] = r
        .get(p..p + 16)
        .ok_or_else(|| anyhow!("a header is cut short"))?
        .try_into()
        .expect("16 bytes");
    Ok((flags, count, salt, p + 16))
}

/// The password check value at `p`, present when bit 0 of `flags` is set.
fn check_value5(
    r: &[u8],
    p: usize,
    flags: u64,
    count: u8,
    salt: [u8; 16],
) -> Result<Option<Check>> {
    let cut = || anyhow!("a header is cut short");
    if flags & 1 == 0 {
        return Ok(None);
    }
    let value: [u8; 8] = r
        .get(p..p + 8)
        .ok_or_else(cut)?
        .try_into()
        .expect("8 bytes");
    let sum = r.get(p + 8..p + 12).ok_or_else(cut)?;
    // The value carries a checksum of its own, so damage to it is not taken
    // for a wrong password; UnRAR ignores a value that fails it.
    if Sha256::digest(value)[..4] != *sum {
        return Ok(None);
    }
    Ok(Some(Check { count, salt, value }))
}

/// One RAR 4 header as stored at `pos`, checked against its checksum, and
/// where it ends. With `crypt`, the password in 16-bit units and the keys
/// derived so far, the header is encrypted, behind its salt and padded to
/// whole AES blocks, as UnRAR's `ReadHeader15` reads it.
fn read4(vol: &mut Volume, pos: u64, crypt: Option<(&[u8], &mut Keys4)>) -> Result<(Vec<u8>, u64)> {
    let size_of = |head: &[u8]| -> Result<u64> {
        let size = u64::from(u16le(head, 5)?);
        if size < 7 {
            bail!("a header has an impossible size");
        }
        Ok(size)
    };
    let (block, end) = match crypt {
        None => {
            let size = size_of(&vol.read(pos, 7)?)?;
            (vol.read(pos, size)?, pos + size)
        }
        Some((password, keys)) => {
            let salt: [u8; 8] = vol.read(pos, 8)?.try_into().expect("eight bytes");
            let (key, iv) = *keys
                .entry(salt)
                .or_insert_with(|| crypt::key4(password, &salt));
            let at = pos + 8;
            let mut first = vol.read(at, crypt::BLOCK)?;
            crypt::decrypt4(&key, &iv, &mut first);
            let size = size_of(&first)?;
            let padded = size.next_multiple_of(crypt::BLOCK);
            let mut plain = vol.read(at, padded)?;
            crypt::decrypt4(&key, &iv, &mut plain);
            plain.truncate(size as usize);
            (plain, at + padded)
        }
    };
    if u32::from(u16le(&block, 0)?) != crc32fast::hash(&block[2..]) & 0xFFFF {
        bail!("a header fails its checksum");
    }
    Ok((block, end))
}

fn parse4(vol: &mut Volume, v: usize, password: Option<&str>, out: &mut Parsed) -> Result<()> {
    let mut pos = 7u64;
    let mut saw_main = false;
    // Set by a main header that says the headers are encrypted: the password
    // as RAR 4 derives keys from it.
    let mut encrypted: Option<Vec<u8>> = None;
    loop {
        if vol.ended(pos)? {
            return Ok(());
        }
        let read = read4(
            vol,
            pos,
            encrypted.as_deref().map(|pw| (pw, &mut out.keys4)),
        );
        let (block, end) = decrypted(read, encrypted.is_some(), out)?;
        let kind = block[2];
        let flags = u16le(&block, 3)?;
        let data = match kind {
            // File and service headers: the packed size, and its high half
            // for large files.
            0x74 | 0x7A => {
                let high = if flags & 0x100 != 0 {
                    u64::from(u32le(&block, 32)?) << 32
                } else {
                    0
                };
                u64::from(u32le(&block, 7)?) | high
            }
            _ if flags & 0x8000 != 0 => u64::from(u32le(&block, 7)?),
            _ => 0,
        };
        let next = end
            .checked_add(data)
            .filter(|&n| n <= vol.len)
            .ok_or_else(|| anyhow!("a header's data runs past the end of the volume"))?;
        match kind {
            0x73 => {
                if saw_main {
                    bail!("the volume has a second main header");
                }
                let hp = flags & 0x80 != 0;
                out.agree(v, hp)?;
                if hp {
                    let Some(password) = password else {
                        return Err(PasswordNeeded.into());
                    };
                    encrypted = Some(utf16_raw(password));
                }
                saw_main = true;
                out.solid |= flags & 0x08 != 0;
            }
            0x74 => {
                let mut h = file4(v, flags, &block)?;
                h.start = vol.start + pos;
                h.data = vol.start + end;
                h.packed = data;
                out.headers.push(h);
            }
            0x7B => {
                if !saw_main {
                    bail!("the volume has no main header");
                }
                out.more = flags & 1 != 0;
                return Ok(());
            }
            _ => {}
        }
        pos = next;
    }
}

fn file4(v: usize, flags: u16, b: &[u8]) -> Result<Header> {
    let host = b
        .get(15)
        .copied()
        .ok_or_else(|| anyhow!("a header is cut short"))?;
    let unp = u64::from(u32le(b, 11)?);
    let crc = u32le(b, 16)?;
    let method = b
        .get(25)
        .copied()
        .ok_or_else(|| anyhow!("a header is cut short"))?;
    let n = usize::from(u16le(b, 26)?);
    let attr = u32le(b, 28)?;
    let (high, at) = if flags & 0x100 != 0 {
        (u64::from(u32le(b, 36)?) << 32, 40)
    } else {
        (0, 32)
    };
    let raw = b
        .get(at..at + n)
        .ok_or_else(|| anyhow!("a header is cut short"))?;
    let name = if flags & 0x200 != 0 {
        decode_name(raw)
    } else {
        String::from_utf8_lossy(raw).into_owned()
    };
    let dir = flags & 0xE0 == 0xE0;
    let mode = (host == 3).then_some(attr);
    Ok(Header {
        volume: v,
        start: 0,
        data: 0,
        packed: 0,
        name,
        rar5: false,
        dir,
        size: unp | high,
        before: flags & 1 != 0,
        after: flags & 2 != 0,
        encrypted: flags & 4 != 0,
        check: None,
        link: is_symlink_mode(mode),
        solid: flags & 0x10 != 0,
        stored: method == 0x30,
        crc: Some(crc),
        // At most 4 MB in RAR 4.
        dict: 0,
        known: true,
        mode,
        // The times are DOS local times, which can't be placed on the
        // timeline without the zone the archive was made in.
        mtime: None,
    })
}

/// RAR4 stores a name as a plain form (OEM on Windows, UTF-8 elsewhere), a
/// NUL, and a compact encoding of the Unicode one, as UnRAR's
/// `EncodeFileName::Decode` reads it.
fn decode_name(raw: &[u8]) -> String {
    let Some(nul) = raw.iter().position(|&b| b == 0) else {
        return String::from_utf8_lossy(raw).into_owned();
    };
    let (plain, enc) = (&raw[..nul], &raw[nul + 1..]);
    let Some((&high, enc)) = enc.split_first() else {
        return String::from_utf8_lossy(plain).into_owned();
    };
    let high = u16::from(high) << 8;
    let mut out: Vec<u16> = Vec::new();
    let (mut i, mut flags, mut bits) = (0, 0u8, 0u8);
    while i < enc.len() {
        if bits == 0 {
            flags = enc[i];
            i += 1;
            bits = 8;
            if i >= enc.len() {
                break;
            }
        }
        bits -= 2;
        match (flags >> bits) & 3 {
            0 => {
                out.push(u16::from(enc[i]));
                i += 1;
            }
            1 => {
                out.push(u16::from(enc[i]) | high);
                i += 1;
            }
            2 => {
                let Some(pair) = enc.get(i..i + 2) else { break };
                out.push(u16::from_le_bytes([pair[0], pair[1]]));
                i += 2;
            }
            // A run copied from the plain form at the same position as the
            // character it gives, which is not where the previous run ended
            // once another opcode has given a character.
            _ => {
                let len = enc[i];
                i += 1;
                let fix = if len & 0x80 != 0 {
                    let Some(&fix) = enc.get(i) else { break };
                    i += 1;
                    Some(fix)
                } else {
                    None
                };
                for _ in 0..(len & 0x7F) + 2 {
                    let Some(&b) = raw.get(out.len()) else { break };
                    out.push(match fix {
                        Some(fix) => u16::from(b.wrapping_add(fix)) | high,
                        None => u16::from(b),
                    });
                }
            }
        }
    }
    if out.is_empty() {
        return String::from_utf8_lossy(plain).into_owned();
    }
    String::from_utf16_lossy(&out)
}

/// What ends a restored set: an end of archive header, in the format of the
/// volume `keep` falls in, saying no volume follows. Everything below `keep`
/// is untouched files with their headers.
pub fn closing(storage: &Storage, keep: u64) -> Result<Vec<u8>> {
    let part = storage
        .parts()
        .iter()
        .rev()
        .find(|p| p.start < keep)
        .ok_or_else(|| anyhow!("nothing of the archive is left to restore"))?;
    // The signature and the first header's type and flags; every volume
    // left holds at least its main header.
    let mut head = [0u8; 16];
    let mut r = storage.reader(None);
    r.seek(SeekFrom::Start(part.start))?;
    r.read_exact(&mut head)?;
    let sig = &head[..8];
    // The end header would have to be encrypted with a key derived from the
    // password, which restore does not take, and the restored set could not
    // be checked without it either.
    let encrypted = if sig.starts_with(SIG5) {
        let mut at = 12;
        vint(&head, &mut at).and_then(|_| vint(&head, &mut at))? == 4
    } else {
        sig.starts_with(SIG4) && u16le(&head, 10)? & 0x80 != 0
    };
    if encrypted {
        bail!(
            "{} has encrypted file names, and restore cannot rebuild such an archive. Run \
             extract with the password to complete the extraction instead",
            part.path.display()
        );
    }
    if sig.starts_with(SIG5) {
        // Size 3: type 5, no header flags, no end flags.
        let body = [3u8, 5, 0, 0];
        let mut out = crc32fast::hash(&body).to_le_bytes().to_vec();
        out.extend(body);
        Ok(out)
    } else if sig.starts_with(SIG4) {
        let mut b = vec![0, 0, 0x7B, 0x00, 0x40, 7, 0];
        let crc = (crc32fast::hash(&b[2..]) & 0xFFFF) as u16;
        b[..2].copy_from_slice(&crc.to_le_bytes());
        Ok(b)
    } else {
        bail!("{} is not a RAR volume", part.path.display())
    }
}

// -- Decoding -------------------------------------------------------------

impl Format for Rar {
    fn worker(&self, _threads: u32) -> Result<Box<dyn Worker + '_>> {
        Ok(Box::new(RarWorker {
            fmt: self,
            session: None,
        }))
    }

    fn serial(&self) -> bool {
        true
    }

    /// A stored file that lies whole in one volume, with a CRC-32 to check
    /// it by, is bytes the engine can read and free itself.
    fn stream(&self, unit: &Unit) -> Result<Option<Stream>> {
        let f = &self.files[unit.key];
        if self.solid || f.dir || f.encrypted || !f.stored || f.first != f.last {
            return Ok(None);
        }
        let (Some(crc), [(data, packed)]) = (f.crc, f.pieces.as_slice()) else {
            return Ok(None);
        };
        if *packed != f.size {
            return Ok(None);
        }
        Ok(Some(Stream {
            data_start: *data,
            packed: *packed,
            codec: Codec::Stored,
            crc: Some(crc),
        }))
    }

    fn refuses_in_place(&self) -> Option<String> {
        self.solid.then(|| {
            "this RAR archive is solid: each file is compressed together with the files before \
             it, so it can only be decoded from the start in one pass. Its space could not be \
             released until the whole archive is extracted, and an interrupted extraction \
             could not resume, so it cannot be extracted in place"
                .to_string()
        })
    }

    fn password_check(&self) -> PasswordCheck {
        let encrypted = || self.files.iter().filter(|f| f.encrypted && !f.dir);
        if !self.rar4 && encrypted().all(|f| f.check.is_some()) {
            PasswordCheck::Headers
        } else {
            PasswordCheck::WholeUnit
        }
    }

    fn prove_password(&self) -> Result<()> {
        let Some(password) = self.password.as_deref() else {
            return Err(PasswordNeeded.into());
        };
        let checks: HashSet<&Check> = self.files.iter().filter_map(|f| f.check.as_ref()).collect();
        if checks.iter().all(|c| c.matches(password)) {
            Ok(())
        } else {
            Err(WrongPassword.into())
        }
    }
}

struct RarWorker<'a> {
    fmt: &'a Rar,
    /// Open between units of one batch, positioned after the last one.
    session: Option<Session>,
}

impl Worker for RarWorker<'_> {
    fn extract(&mut self, unit: &Unit, sink: &mut dyn Sink) -> Result<()> {
        if unit.encrypted && self.fmt.password.is_none() {
            return Err(PasswordNeeded.into());
        }
        let mut result = Ok(());
        for (i, item) in unit.items.iter().enumerate() {
            result = self.extract_one(unit.key + i, item, sink);
            if result.is_err() {
                break;
            }
        }
        if result.is_err() {
            // Wherever UnRAR stopped, the next unit starts from a fresh open.
            self.session = None;
        }
        result
    }
}

impl RarWorker<'_> {
    fn extract_one(&mut self, index: usize, item: &Item, sink: &mut dyn Sink) -> Result<()> {
        let fmt = self.fmt;
        if item.kind == Kind::Dir {
            // Nothing to decode; the walk steps over its header later.
            return sink.item(item, &mut io::empty());
        }
        if self.session.as_ref().is_none_or(|s| s.next > index) {
            // Dropped first: only one handle may be open at a time.
            self.session = None;
            let f = &fmt.files[index];
            let (volume, next) = if fmt.solid {
                (0, 0)
            } else {
                let next = fmt
                    .files
                    .iter()
                    .position(|g| g.first == f.first)
                    .unwrap_or(index);
                (f.first, next)
            };
            let path = &fmt.storage.parts()[volume].path;
            self.session = Some(Session::open(
                path,
                native::RAR_OM_EXTRACT,
                fmt.password.as_deref(),
                next,
            )?);
        }
        let session = self.session.as_mut().expect("opened above");
        let (tx, rx) = sync_channel::<Msg>(4);
        thread::scope(|s| {
            let decoder = s.spawn(move || session.decode(index, &fmt.files, fmt.rar4, tx));
            let mut data = Chunks {
                rx,
                buf: Vec::new(),
                at: 0,
                done: false,
                failed: false,
            };
            // The sink may stop short of the end of the data; the rest is
            // read so the decoder finishes and checks its checksum.
            let sunk = sink.item(item, &mut data).and_then(|()| {
                io::copy(&mut data, &mut io::sink())?;
                Ok(())
            });
            let decoder_failed = data.failed;
            // Dropping the receiver makes the callback ask UnRAR to stop.
            drop(data);
            let decoded = decoder
                .join()
                .map_err(|_| anyhow!("the RAR decoder crashed"))?;
            match (sunk, decoded) {
                (Ok(()), r) => r,
                // The sink failed on what the decoder reported; the
                // decoder's error says more, such as a wrong password.
                (Err(_), Err(e)) if decoder_failed => Err(e),
                (Err(e), _) => Err(e),
            }
        })
    }
}

/// What the decoder thread says about one file: its data, then the end.
enum Msg {
    Data(Vec<u8>),
    End(std::result::Result<(), String>),
}

/// The data of one file as the decoder thread sends it.
struct Chunks {
    rx: Receiver<Msg>,
    buf: Vec<u8>,
    at: usize,
    done: bool,
    /// The decoder reported an error.
    failed: bool,
}

impl Read for Chunks {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.at == self.buf.len() {
            if self.done {
                return Ok(0);
            }
            match self.rx.recv() {
                Ok(Msg::Data(d)) => {
                    self.buf = d;
                    self.at = 0;
                }
                Ok(Msg::End(Ok(()))) => {
                    self.done = true;
                    return Ok(0);
                }
                Ok(Msg::End(Err(e))) => {
                    self.done = true;
                    self.failed = true;
                    return Err(io::Error::other(e));
                }
                Err(_) => {
                    self.done = true;
                    self.failed = true;
                    return Err(io::Error::other("the RAR decoder stopped early"));
                }
            }
        }
        let n = out.len().min(self.buf.len() - self.at);
        out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// UnRAR keeps some state in globals, its error code among them, and is not
/// documented as safe on several threads, so only one archive handle is open
/// at a time. A handle lives across calls and threads, which a `MutexGuard`
/// can't, hence a flag.
static BUSY: Mutex<bool> = Mutex::new(false);
static FREED: Condvar = Condvar::new();

struct Turn;

impl Turn {
    fn take() -> Self {
        let mut busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
        while *busy {
            busy = FREED.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        *busy = true;
        Turn
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        *BUSY.lock().unwrap_or_else(|e| e.into_inner()) = false;
        FREED.notify_one();
    }
}

/// What the callback needs, at an address that stays put for as long as the
/// archive is open.
struct Shared {
    /// Where file data goes while a file is decoded.
    data: Mutex<Option<SyncSender<Msg>>>,
    password: Option<Vec<native::WCHAR>>,
}

/// One open UnRAR handle, walking the set forward.
struct Session {
    handle: *const native::Handle,
    shared: Box<Shared>,
    header: Box<native::HeaderDataEx>,
    /// UnRAR's `FileNameW` holds 1023 characters; this takes any name.
    name: Vec<native::WCHAR>,
    /// The index in `Rar::files` of the file the next header read belongs
    /// to.
    next: usize,
    _turn: Turn,
}

// SAFETY: the handle is only used by one thread at a time, through `&mut`,
// and UnRAR ties no state to the thread that opened it.
unsafe impl Send for Session {}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe { native::RARCloseArchive(self.handle) };
    }
}

impl Session {
    fn open(path: &Path, mode: c_uint, password: Option<&str>, next: usize) -> Result<Self> {
        let turn = Turn::take();
        let name = platform::RarName::new(path).context("the volume's name")?;
        let shared = Box::new(Shared {
            data: Mutex::new(None),
            password: password.map(wide),
        });
        let mut open = name.open_data(mode);
        open.callback = Some(callback);
        open.user_data = &*shared as *const Shared as native::LPARAM;
        let handle = unsafe { native::RAROpenArchiveEx(&open) };
        let result = open.open_result;
        if handle.is_null() || result != 0 {
            if !handle.is_null() {
                unsafe { native::RARCloseArchive(handle) };
            }
            return Err(failure(result as c_int, false))
                .with_context(|| format!("UnRAR could not open {}", path.display()));
        }
        Ok(Self {
            handle,
            shared,
            header: Box::default(),
            name: vec![0; NAME_ROOM],
            next,
            _turn: turn,
        })
    }

    fn read_header(&mut self) -> c_int {
        let h = &mut *self.header;
        h.comment_buffer = std::ptr::null_mut();
        h.comment_buffer_size = 0;
        h.redir_name = std::ptr::null_mut();
        h.redir_name_size = 0;
        h.file_name_ex = self.name.as_mut_ptr();
        h.file_name_ex_size = self.name.len() as c_uint;
        // UnRAR writes the header, so the pointer comes from a unique borrow
        // even though the binding declares it const.
        unsafe { native::RARReadHeaderEx(self.handle, std::ptr::addr_of_mut!(*h).cast_const()) }
    }

    fn entry_name(&self) -> String {
        wide_to_string(&self.name)
    }

    fn process(&mut self, op: c_int) -> c_int {
        unsafe { native::RARProcessFileW(self.handle, op, std::ptr::null(), std::ptr::null()) }
    }

    fn skip(&mut self) -> Result<()> {
        match self.process(native::RAR_SKIP) {
            native::ERAR_SUCCESS => Ok(()),
            s => Err(failure(s, false)).context("reading past a file"),
        }
    }

    /// Reads headers up to the one of `files[index]`, checking the name of
    /// every file on the way, and leaves that header current.
    fn seek(&mut self, index: usize, files: &[File]) -> Result<()> {
        loop {
            let status = self.read_header();
            if status == native::ERAR_END_ARCHIVE {
                bail!("the archive ends before '{}'", files[index].name);
            }
            if status != native::ERAR_SUCCESS {
                return Err(failure(status, false)).context("reading the headers");
            }
            // The rest of a file that began in an earlier volume.
            let flags = self.header.flags;
            if flags & native::RHDF_SPLITBEFORE != 0 {
                self.skip()?;
                continue;
            }
            let name = self.entry_name();
            let Some(f) = files.get(self.next) else {
                bail!("the archive changed while it was being read: found '{name}'");
            };
            if !f.same_name(&name) {
                bail!(
                    "the archive changed while it was being read: expected '{}', found '{name}'",
                    f.name
                );
            }
            if self.next == index {
                return Ok(());
            }
            self.skip()?;
            self.next += 1;
        }
    }

    /// Decodes `files[index]` and sends its data, then the outcome.
    fn decode(
        &mut self,
        index: usize,
        files: &[File],
        rar4: bool,
        tx: SyncSender<Msg>,
    ) -> Result<()> {
        let result = self.seek(index, files).and_then(|()| {
            *self.shared.data.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
            let status = self.process(native::RAR_TEST);
            *self.shared.data.lock().unwrap_or_else(|e| e.into_inner()) = None;
            self.next += 1;
            match status {
                native::ERAR_SUCCESS => Ok(()),
                // RAR 4 has no password check value: a wrong password shows
                // as data that fails its checksum.
                native::ERAR_BAD_DATA if rar4 && files[index].encrypted => {
                    Err(WrongPassword.into())
                }
                s => Err(failure(s, files[index].encrypted)),
            }
        });
        let said = result.as_ref().map(|_| ()).map_err(|e| format!("{e:#}"));
        // If the reader is gone it stopped on its own error, which it reports.
        let _ = tx.send(Msg::End(said));
        result.with_context(|| format!("decoding '{}'", files[index].name))
    }
}

extern "C" fn callback(
    msg: c_uint,
    user: native::LPARAM,
    p1: native::LPARAM,
    p2: native::LPARAM,
) -> c_int {
    // SAFETY: `user` is the address of the `Shared` its session keeps alive
    // for as long as the archive is open.
    let shared = unsafe { &*(user as *const Shared) };
    match msg {
        native::UCM_PROCESSDATA => {
            let sender = shared.data.lock().unwrap_or_else(|e| e.into_inner());
            let Some(tx) = sender.as_ref() else {
                return 1;
            };
            let data = unsafe { std::slice::from_raw_parts(p1 as *const u8, p2 as usize) };
            match tx.send(Msg::Data(data.to_vec())) {
                Ok(()) => 1,
                // The reader is gone: stop.
                Err(_) => -1,
            }
        }
        // A volume that is there is reported; one that isn't is a failure.
        native::UCM_CHANGEVOLUME | native::UCM_CHANGEVOLUMEW => {
            if p2 == native::RAR_VOL_NOTIFY {
                1
            } else {
                -1
            }
        }
        // Wide only: UnRAR asks for that first, and it carries any
        // character on every system.
        native::UCM_NEEDPASSWORDW => match &shared.password {
            Some(pw) => {
                let room = usize::try_from(p2).unwrap_or(0);
                if room == 0 {
                    return -1;
                }
                let n = pw.len().min(room - 1);
                let dst = p1 as *mut native::WCHAR;
                unsafe {
                    std::ptr::copy_nonoverlapping(pw.as_ptr(), dst, n);
                    *dst.add(n) = 0;
                }
                1
            }
            None => -1,
        },
        native::UCM_NEEDPASSWORD => -1,
        // Includes UCM_LARGEDICT: such files are refused when the archive
        // is opened, and a 0 here refuses any other.
        _ => 0,
    }
}

fn wide(s: &str) -> Vec<native::WCHAR> {
    if std::mem::size_of::<native::WCHAR>() == 2 {
        s.encode_utf16().map(|u| u as native::WCHAR).collect()
    } else {
        s.chars().map(|c| c as native::WCHAR).collect()
    }
}

fn wide_to_string(w: &[native::WCHAR]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    if std::mem::size_of::<native::WCHAR>() == 2 {
        let units: Vec<u16> = w[..end].iter().map(|&c| c as u32 as u16).collect();
        String::from_utf16_lossy(&units)
    } else {
        w[..end]
            .iter()
            .map(|&c| char::from_u32(c as u32).unwrap_or('\u{FFFD}'))
            .collect()
    }
}

fn describe(code: c_int) -> &'static str {
    match code {
        native::ERAR_BAD_DATA => "the data is damaged or fails its checksum",
        native::ERAR_BAD_ARCHIVE => "the archive is damaged",
        native::ERAR_UNKNOWN_FORMAT => "the archive uses a format UnRAR does not know",
        native::ERAR_EOPEN => "a volume could not be opened",
        native::ERAR_EREAD => "a read failed",
        native::ERAR_NO_MEMORY => "not enough memory",
        native::ERAR_MISSING_PASSWORD => "a password is required",
        native::ERAR_BAD_PASSWORD => "incorrect password",
        native::ERAR_EREFERENCE => "a file refers to one that is not in the archive",
        native::ERAR_LARGE_DICT => "the dictionary is larger than 4 GB",
        _ => "an unknown error",
    }
}

fn failure(code: c_int, encrypted: bool) -> anyhow::Error {
    match code {
        native::ERAR_MISSING_PASSWORD => PasswordNeeded.into(),
        native::ERAR_BAD_PASSWORD => WrongPassword.into(),
        native::ERAR_BAD_DATA if encrypted => {
            anyhow!("the data is damaged or fails its checksum, or the password is incorrect")
        }
        c => anyhow!("{} (UnRAR error {c})", describe(c)),
    }
}

/// The names of the first `want` files of the set as UnRAR reports them, in
/// order, continuations skipped.
fn unrar_names(first: &Path, want: usize, password: Option<&str>) -> Result<Vec<String>> {
    // Only encrypted file names need the password here.
    let mut session = Session::open(first, native::RAR_OM_LIST, password, 0)?;
    let mut names = Vec::with_capacity(want);
    while names.len() < want {
        let status = session.read_header();
        if status == native::ERAR_END_ARCHIVE {
            break;
        }
        if status != native::ERAR_SUCCESS {
            return Err(failure(status, false)).context("UnRAR could not list the set");
        }
        names.push(session.entry_name());
        if names.len() == want {
            break;
        }
        session.skip().context("UnRAR could not list the set")?;
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A check value made by RAR 6.24 for the password "secret", copied from
    /// the encryption record of `crates/cli/tests/fixtures/rar/rar5-pw`.
    #[test]
    fn the_rar5_check_value_tells_right_from_wrong() {
        let record = fixture_record();
        let check = crypt5(&record)
            .unwrap()
            .expect("the fixture has a check value");
        assert!(check.matches("secret"));
        assert!(!check.matches("Secret"));
        assert!(!check.matches(""));
    }

    fn fixture_record() -> Vec<u8> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../cli/tests/fixtures/rar/rar5-pw/set.part1.rar");
        let bytes = std::fs::read(path).unwrap();
        // Find the first file header and return its encryption record.
        let mut pos = 8usize;
        loop {
            let mut at = pos + 4;
            let size = vint(&bytes, &mut at).unwrap() as usize;
            let body = &bytes[at..at + size];
            let mut p = 0;
            let kind = vint(body, &mut p).unwrap();
            let flags = vint(body, &mut p).unwrap();
            let extra = if flags & 1 != 0 {
                vint(body, &mut p).unwrap() as usize
            } else {
                0
            };
            let data = if flags & 2 != 0 {
                vint(body, &mut p).unwrap() as usize
            } else {
                0
            };
            if kind == 2 && extra > 0 {
                let ex = &body[body.len() - extra..];
                let mut e = 0;
                while e < ex.len() {
                    let len = vint(ex, &mut e).unwrap() as usize;
                    let end = e + len;
                    let k = vint(ex, &mut e).unwrap();
                    if k == 1 {
                        return ex[e..end].to_vec();
                    }
                    e = end;
                }
            }
            pos = at + size + data;
        }
    }

    #[test]
    fn rar5_names_keep_backslashes_as_part_of_the_name() {
        assert_eq!(
            clean_name("game/back\\slash.txt", true),
            "game/back_slash.txt"
        );
        assert_eq!(clean_name("game\\file.txt", false), "game\\file.txt");
    }

    #[test]
    fn rar4_unicode_names_decode_as_unrar_reads_them() {
        // Written by RAR 6.24 for Linux: the plain form is UTF-8.
        let mut raw = "game\\naïve résumé\\日本語.txt".as_bytes().to_vec();
        raw.extend([0, 0x65, 0xC0, 0x05, 0xEF, b'v', b'e', 0, b' ', b'r', 0xE9]);
        raw.extend([b's', 0, b'u', b'm', 0xE9, b'\\', 0x68, 0xE5, 0x2C, 0x67]);
        raw.extend([0x9E, 0x8A, b'.', 0, b't', b'x', b't']);
        assert_eq!(decode_name(&raw), "game\\naïve résumé\\日本語.txt");
        // One character given by itself, then a run copied from the plain
        // form at the run's own position, not from where it starts.
        assert_eq!(decode_name(b"?abcd\0\x00\x30\xE9\x02"), "éabcd");
    }

    #[test]
    fn passwords_are_cut_where_unrar_cuts_them() {
        let long = "p".repeat(300);
        assert_eq!(rar_password(&long).len(), MAX_PASSWORD - 1);
        assert_eq!(rar_password("short"), "short");
    }
}
