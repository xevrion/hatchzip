//! RAR volume sets (`name.part1.rar`, `name.part2.rar`, ... or `name.rar`,
//! `name.r00`, ...). Each volume is a complete RAR file of its own, so the
//! archive is freed a whole volume at a time: nothing is ever cut out of the
//! middle of one, and what is left is always a set UnRAR can still open.
//!
//! The headers are read here, so the volume of every file is known. A file
//! belongs to the volume its header is in. After the files of some volumes
//! are out, every volume past the last one an unextracted file still reaches
//! into can go. Volumes that free nothing on their own because a file spans
//! them are merged into one unit.
//!
//! Decoding is UnRAR's. It is driven through its C interface (`unrar_sys`) so
//! the data comes out of a callback in chunks, which a helper thread turns
//! into the reader the engine's sinks take.

use std::ffi::{c_int, c_uint};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use unrar_sys as native;

use super::storage::{Reader, Storage};
use super::{
    Damaged, Format, Item, Kind, Opened, PasswordNeeded, Sink, Unit, Worker, WrongPassword,
    is_symlink_mode, safe_path,
};
use crate::platform;

pub const NAME: &str = "rar";

const SIG5: &[u8] = b"Rar!\x1A\x07\x01\x00";
const SIG4: &[u8] = b"Rar!\x1A\x07\x00";

/// RAR5 headers are at most 2 MiB by the format's own limit.
const MAX_HEADER: u64 = 2 << 20;

/// What the headers of one file say, in one volume. A file that spans
/// volumes has one of these in each.
struct Header {
    volume: usize,
    name: String,
    dir: bool,
    size: u64,
    before: bool,
    after: bool,
    encrypted: bool,
    link: bool,
    solid: bool,
    mode: Option<u32>,
    mtime: Option<SystemTime>,
}

#[derive(Default)]
struct Parsed {
    headers: Vec<Header>,
    /// Some volume says the archive is solid.
    solid: bool,
    /// The last volume says more volumes follow.
    more: bool,
}

struct Rar {
    storage: Arc<Storage>,
    password: Option<String>,
}

pub fn open(storage: &Arc<Storage>, resuming: bool, password: Option<&str>) -> Result<Opened> {
    let end = storage.current_len().context("reading the volumes")?;
    // Units are planned against the set as it began, so that a resumed run
    // finds the same ones the first run did.
    let whole = storage.original_len();
    let volumes: Vec<_> = storage
        .parts()
        .iter()
        .take_while(|p| p.start < end)
        .collect();
    if volumes.is_empty() {
        bail!("{} is empty", storage.primary().display());
    }
    let mut reader = storage.reader(None);
    let mut parsed = Parsed::default();
    for (v, part) in volumes.iter().enumerate() {
        parse_volume(&mut reader, v, part.start, part.len, &mut parsed)
            .map_err(|e| anyhow::Error::new(Damaged(e)))
            .with_context(|| format!("reading {}", part.path.display()))?;
    }
    if parsed.more && !resuming {
        bail!(
            "{} is not the whole set: the last volume found says more follow. Put every volume \
             in one folder",
            volumes.last().expect("checked above").path.display()
        );
    }
    if parsed.solid || parsed.headers.iter().any(|h| h.solid) {
        bail!(
            "this RAR archive is solid: every file is compressed against the ones before it, \
             so no volume can be freed until all of it is extracted, and extracting it in \
             place would need free space for the whole output next to the whole archive. \
             Repack it without 'solid' to extract it in place"
        );
    }

    let files = files(&parsed.headers, volumes.len(), resuming)?;
    if files.iter().any(|f| f.link) {
        bail!("this RAR archive holds links, which are not supported yet");
    }

    // After the files of every volume from `v` up are extracted, the volumes
    // past the last one any earlier file reaches into are free.
    let mut cuts = Vec::with_capacity(volumes.len());
    let mut reach: Option<usize> = None;
    for v in 0..volumes.len() {
        let keep = reach.map_or(0, |r| r + 1);
        cuts.push(storage.parts().get(keep).map_or(whole, |p| p.start));
        for f in files.iter().filter(|f| f.first == v) {
            reach = reach.max(Some(f.last));
        }
    }

    // Groups that free the same bytes are one unit: extracting the first
    // alone would leave the archive as long as before, which the engine
    // would take for a unit that is already done. So would the highest
    // group if it frees nothing, so it joins the unit below it.
    let mut ranges: Vec<(u64, Vec<usize>)> = Vec::new();
    for v in (0..volumes.len()).filter(|&v| files.iter().any(|f| f.first == v)) {
        match ranges.last_mut() {
            Some((cut, groups)) if *cut == cuts[v] => groups.push(v),
            _ => ranges.push((cuts[v], vec![v])),
        }
    }
    if ranges.len() > 1 && ranges.last().is_some_and(|(cut, _)| *cut >= whole) {
        let (_, groups) = ranges.pop().expect("checked above");
        ranges.last_mut().expect("checked above").1.extend(groups);
    }
    let mut units = Vec::with_capacity(ranges.len());
    for (cut, groups) in ranges {
        let members: Vec<&File> = files.iter().filter(|f| groups.contains(&f.first)).collect();
        units.push(Unit {
            start: cut,
            key: groups[0],
            items: members.iter().map(|f| f.item()).collect::<Result<_>>()?,
            encrypted: members.iter().any(|f| f.encrypted),
        });
    }

    Ok(Opened {
        name: NAME,
        format: Box::new(Rar {
            storage: storage.clone(),
            password: password.map(str::to_string),
        }),
        units,
        // Every volume is a whole RAR file with its own headers, so nothing
        // needs saving.
        tail_start: end,
    })
}

struct File {
    first: usize,
    last: usize,
    name: String,
    dir: bool,
    size: u64,
    encrypted: bool,
    link: bool,
    mode: Option<u32>,
    mtime: Option<SystemTime>,
}

impl File {
    fn item(&self) -> Result<Item> {
        let path = safe_path(&self.name).ok_or_else(|| {
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
}

/// Joins the per-volume headers of files that span volumes into one entry
/// per file, with the volumes it starts and ends in.
fn files(headers: &[Header], volumes: usize, resuming: bool) -> Result<Vec<File>> {
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
            name: h.name.clone(),
            dir: h.dir,
            size: h.size,
            encrypted: h.encrypted,
            link: h.link,
            mode: h.mode,
            mtime: h.mtime,
        });
        if h.after {
            open = Some(files.len() - 1);
        }
    }
    // A resumed run sees the end of what is left: a file that began there and
    // is already extracted has lost its continuation.
    if open.is_some() && !resuming {
        bail!("the last volume ends in the middle of a file; a volume is missing");
    }
    debug_assert!(files.iter().all(|f| f.last < volumes));
    Ok(files)
}

// -- Headers --------------------------------------------------------------

struct Volume<'a> {
    r: &'a mut Reader,
    start: u64,
    len: u64,
}

impl Volume<'_> {
    fn read(&mut self, at: u64, n: u64) -> Result<Vec<u8>> {
        if at + n > self.len {
            bail!("a header runs past the end of the volume");
        }
        self.r.seek(SeekFrom::Start(self.start + at))?;
        let mut buf = vec![0; n as usize];
        self.r.read_exact(&mut buf)?;
        Ok(buf)
    }
}

fn parse_volume(
    reader: &mut Reader,
    v: usize,
    start: u64,
    len: u64,
    out: &mut Parsed,
) -> Result<()> {
    let mut vol = Volume {
        r: reader,
        start,
        len,
    };
    let sig = vol.read(0, 8.min(len))?;
    if sig.starts_with(SIG5) {
        parse5(&mut vol, v, out)
    } else if sig.starts_with(SIG4) {
        parse4(&mut vol, v, out)
    } else {
        bail!("not a RAR volume (no signature at its start)")
    }
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

fn parse5(vol: &mut Volume, v: usize, out: &mut Parsed) -> Result<()> {
    let mut pos = 8u64;
    let mut saw_main = false;
    loop {
        if pos >= vol.len {
            bail!("the volume has no end of archive header");
        }
        let peek = vol.read(pos, 16.min(vol.len - pos))?;
        let crc = u32le(&peek, 0)?;
        let mut at = 4;
        let size = vint(&peek, &mut at)?;
        if size == 0 || size > MAX_HEADER {
            bail!("a header has an impossible size");
        }
        let block = vol.read(pos + 4, (at - 4) as u64 + size)?;
        if crc32fast::hash(&block) != crc {
            bail!("a header fails its checksum");
        }
        let body = &block[at - 4..];
        let mut p = 0;
        let kind = vint(body, &mut p)?;
        let flags = vint(body, &mut p)?;
        let extra = if flags & 1 != 0 {
            vint(body, &mut p)? as usize
        } else {
            0
        };
        let data = if flags & 2 != 0 {
            vint(body, &mut p)?
        } else {
            0
        };
        let next = pos + 4 + (at as u64 - 4) + size + data;
        let extra_at = body
            .len()
            .checked_sub(extra)
            .filter(|&e| e >= p)
            .ok_or_else(|| anyhow!("a header's extra area is larger than the header"))?;
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
            2 => out
                .headers
                .push(file5(v, flags, &body[p..extra_at], &body[extra_at..], p)?),
            4 => bail!(
                "this RAR archive has encrypted file names. Archives with encrypted headers are \
                 not supported yet"
            ),
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

fn file5(v: usize, hflags: u64, b: &[u8], extra: &[u8], _at: usize) -> Result<Header> {
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
    if flags & 4 != 0 {
        p += 4;
    }
    let info = vint(b, &mut p)?;
    let host = vint(b, &mut p)?;
    let n = vint(b, &mut p)? as usize;
    let name = b
        .get(p..p + n)
        .ok_or_else(|| anyhow!("a header is cut short"))?;
    if flags & 8 != 0 {
        bail!("an entry has no recorded size");
    }
    let mode = (host == 1).then_some(attr as u32);

    let (mut encrypted, mut link) = (false, is_symlink_mode(mode));
    let mut e = 0;
    while e < extra.len() {
        let len = vint(extra, &mut e)? as usize;
        let start = e;
        let kind = vint(extra, &mut e)?;
        match kind {
            1 => encrypted = true,
            5 => link = true,
            _ => {}
        }
        e = start + len;
    }
    Ok(Header {
        volume: v,
        name: String::from_utf8_lossy(name).into_owned(),
        dir: flags & 1 != 0,
        size,
        before: hflags & 8 != 0,
        after: hflags & 0x10 != 0,
        encrypted,
        link,
        solid: info & 0x40 != 0,
        mode,
        mtime,
    })
}

fn parse4(vol: &mut Volume, v: usize, out: &mut Parsed) -> Result<()> {
    let mut pos = 7u64;
    let mut saw_main = false;
    loop {
        if pos + 7 > vol.len {
            bail!("the volume has no end of archive header");
        }
        let head = vol.read(pos, 7)?;
        let kind = head[2];
        let flags = u16le(&head, 3)?;
        let size = u64::from(u16le(&head, 5)?);
        if size < 7 {
            bail!("a header has an impossible size");
        }
        let block = vol.read(pos, size)?;
        if u32::from(u16le(&block, 0)?) != crc32fast::hash(&block[2..]) & 0xFFFF {
            bail!("a header fails its checksum");
        }
        let mut next = pos + size;
        match kind {
            0x73 => {
                if flags & 0x80 != 0 {
                    bail!(
                        "this RAR archive has encrypted file names. Archives with encrypted \
                         headers are not supported yet"
                    );
                }
                saw_main = true;
                out.solid |= flags & 0x08 != 0;
            }
            0x74 => {
                let h = file4(v, flags, &block)?;
                next += u64::from(u32le(&block, 7)?);
                if flags & 0x100 != 0 {
                    next += u64::from(u32le(&block, 32)?) << 32;
                }
                out.headers.push(h);
            }
            0x7B => {
                if !saw_main {
                    bail!("the volume has no main header");
                }
                out.more = flags & 1 != 0;
                return Ok(());
            }
            _ => {
                if flags & 0x8000 != 0 {
                    next += u64::from(u32le(&block, 7)?);
                }
            }
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
        name,
        dir,
        size: unp | high,
        before: flags & 1 != 0,
        after: flags & 2 != 0,
        encrypted: flags & 4 != 0,
        link: is_symlink_mode(mode),
        solid: flags & 0x10 != 0,
        mode,
        // The times are DOS local times, which can't be placed on the
        // timeline without the zone the archive was made in.
        mtime: None,
    })
}

/// RAR4 stores a name as an ASCII form, a NUL, and a compact encoding of the
/// Unicode one.
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
    let (mut i, mut flags, mut bits, mut pos) = (0, 0u8, 0u8, 0usize);
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
            _ => {
                let len = enc[i];
                i += 1;
                if len & 0x80 != 0 {
                    let Some(&fix) = enc.get(i) else { break };
                    i += 1;
                    for _ in 0..(len & 0x7F) + 2 {
                        let b = plain.get(pos).copied().unwrap_or(0);
                        out.push(u16::from(b.wrapping_add(fix)) | high);
                        pos += 1;
                    }
                } else {
                    for _ in 0..len + 2 {
                        out.push(u16::from(plain.get(pos).copied().unwrap_or(0)));
                        pos += 1;
                    }
                }
            }
        }
    }
    String::from_utf16_lossy(&out)
}

// -- Decoding -------------------------------------------------------------

impl Format for Rar {
    fn worker(&self, _threads: u32) -> Result<Box<dyn Worker + '_>> {
        Ok(Box::new(RarWorker { fmt: self }))
    }
}

struct RarWorker<'a> {
    fmt: &'a Rar,
}

/// What the decoder thread says. Data of one file arrives between its
/// `File` and its `End`.
enum Msg {
    File,
    Data(Vec<u8>),
    End(std::result::Result<(), String>),
}

/// UnRAR is not documented as safe to run on several threads, so only one
/// archive is decoded at a time.
static UNRAR: Mutex<()> = Mutex::new(());

impl Worker for RarWorker<'_> {
    fn extract(&mut self, unit: &Unit, sink: &mut dyn Sink) -> Result<()> {
        let fmt = self.fmt;
        if unit.encrypted && fmt.password.is_none() {
            return Err(PasswordNeeded.into());
        }
        let path = fmt.storage.parts()[unit.key].path.clone();
        let names: Vec<(std::path::PathBuf, bool)> = unit
            .items
            .iter()
            .map(|i| (i.path.clone(), i.kind == Kind::Dir))
            .collect();
        let password = fmt.password.clone();
        let (tx, rx) = sync_channel::<Msg>(4);

        thread::scope(|s| {
            let decoder = s.spawn(move || {
                let _one = UNRAR.lock().unwrap_or_else(|e| e.into_inner());
                decode(&path, password.as_deref(), &names, &tx)
            });
            let mut failed: Option<anyhow::Error> = None;
            for item in &unit.items {
                let mut data = Chunks {
                    rx: &rx,
                    buf: Vec::new(),
                    at: 0,
                    done: item.kind == Kind::Dir,
                };
                if item.kind != Kind::Dir {
                    match rx.recv() {
                        Ok(Msg::File) => {}
                        _ => break,
                    }
                }
                let r = sink.item(item, &mut data);
                if let Err(e) = r {
                    failed = Some(e);
                    break;
                }
                // The sink may stop short of the end of the data.
                if io::copy(&mut data, &mut io::sink()).is_err() {
                    break;
                }
            }
            // Dropping the receiver makes the callback ask UnRAR to stop.
            drop(rx);
            let decoded = decoder
                .join()
                .map_err(|_| anyhow!("the RAR decoder crashed"))?;
            match (failed, decoded) {
                (Some(e), _) => Err(e),
                (None, r) => r,
            }
        })
    }
}

/// The data of one file as the decoder thread sends it.
struct Chunks<'a> {
    rx: &'a std::sync::mpsc::Receiver<Msg>,
    buf: Vec<u8>,
    at: usize,
    done: bool,
}

impl Read for Chunks<'_> {
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
                    return Err(io::Error::other(e));
                }
                Ok(Msg::File) | Err(_) => {
                    self.done = true;
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

struct Shared {
    tx: SyncSender<Msg>,
    password: Option<std::ffi::CString>,
    wide_password: Option<Vec<native::WCHAR>>,
}

extern "C" fn callback(
    msg: c_uint,
    user: native::LPARAM,
    p1: native::LPARAM,
    p2: native::LPARAM,
) -> c_int {
    // SAFETY: `user` is the address of the `Shared` that `decode` keeps
    // alive for as long as the archive is open.
    let ctx = unsafe { &*(user as *const Shared) };
    match msg {
        native::UCM_PROCESSDATA => {
            let data = unsafe { std::slice::from_raw_parts(p1 as *const u8, p2 as usize) };
            match ctx.tx.send(Msg::Data(data.to_vec())) {
                Ok(()) => 1,
                // The consumer is gone: stop.
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
        native::UCM_NEEDPASSWORD => match &ctx.password {
            Some(pw) => {
                let room = p2 as usize;
                let bytes = pw.as_bytes_with_nul();
                if bytes.len() > room {
                    return -1;
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p1 as *mut u8, bytes.len())
                };
                1
            }
            None => -1,
        },
        native::UCM_NEEDPASSWORDW => match &ctx.wide_password {
            Some(pw) => {
                let room = p2 as usize;
                if pw.len() > room {
                    return -1;
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(pw.as_ptr(), p1 as *mut native::WCHAR, pw.len())
                };
                1
            }
            None => -1,
        },
        _ => 0,
    }
}

fn wide(s: &str) -> Vec<native::WCHAR> {
    let mut out: Vec<native::WCHAR> = if std::mem::size_of::<native::WCHAR>() == 2 {
        s.encode_utf16().map(|u| u as native::WCHAR).collect()
    } else {
        s.chars().map(|c| c as native::WCHAR).collect()
    };
    out.push(0);
    out
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
        native::ERAR_NO_MEMORY => "out of memory",
        native::ERAR_MISSING_PASSWORD => "a password is needed",
        native::ERAR_BAD_PASSWORD => "incorrect password",
        native::ERAR_EREFERENCE => "a file refers to one that is not in the archive",
        _ => "an unknown error",
    }
}

/// Walks the archive from the volume at `path` and sends the data of every
/// file in `names`, in order. Continuations of files that began in earlier
/// volumes are skipped.
fn decode(
    path: &Path,
    password: Option<&str>,
    names: &[(std::path::PathBuf, bool)],
    tx: &SyncSender<Msg>,
) -> Result<()> {
    let name = platform::RarName::new(path).context("the volume's name")?;
    let ctx = Shared {
        tx: tx.clone(),
        password: password.and_then(|p| std::ffi::CString::new(p).ok()),
        wide_password: password.map(wide),
    };
    let mut open = name.open_data(native::RAR_OM_EXTRACT);
    open.callback = Some(callback);
    open.user_data = &ctx as *const Shared as native::LPARAM;
    let handle = unsafe { native::RAROpenArchiveEx(&open) };
    if handle.is_null() {
        bail!(
            "opening {}: {} (code {})",
            path.display(),
            describe(open.open_result as c_int),
            open.open_result
        );
    }
    struct Close(*const native::Handle);
    impl Drop for Close {
        fn drop(&mut self) {
            unsafe { native::RARCloseArchive(self.0) };
        }
    }
    let _close = Close(handle);
    if let Some(pw) = &ctx.password {
        unsafe { native::RARSetPassword(handle, pw.as_ptr()) };
    }

    // The header struct is large; keep it off the stack.
    let mut header: Box<native::HeaderDataEx> =
        unsafe { Box::new(std::mem::zeroed::<native::HeaderDataEx>()) };
    let mut next = 0;
    while next < names.len() {
        header.comment_buffer = std::ptr::null_mut();
        header.comment_buffer_size = 0;
        let status = unsafe { native::RARReadHeaderEx(handle, &*header) };
        if status == native::ERAR_END_ARCHIVE {
            bail!("the volumes end before '{}'", names[next].0.display());
        }
        if status != native::ERAR_SUCCESS {
            return Err(failure(status, password.is_some())
                .context(format!("reading a header in {}", path.display())));
        }
        if header.flags & native::RHDF_SPLITBEFORE != 0 {
            unsafe {
                native::RARProcessFileW(
                    handle,
                    native::RAR_SKIP,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            continue;
        }
        let entry = wide_to_string(&header.filename_w);
        let (want, dir) = &names[next];
        if safe_path(&entry).as_ref() != Some(want) {
            bail!(
                "the archive changed while it was being read: expected '{}', found '{entry}'",
                want.display()
            );
        }
        if *dir {
            unsafe {
                native::RARProcessFileW(
                    handle,
                    native::RAR_SKIP,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            next += 1;
            continue;
        }
        if tx.send(Msg::File).is_err() {
            return Ok(());
        }
        let status = unsafe {
            native::RARProcessFileW(handle, native::RAR_TEST, std::ptr::null(), std::ptr::null())
        };
        if status == native::ERAR_SUCCESS {
            if tx.send(Msg::End(Ok(()))).is_err() {
                return Ok(());
            }
            next += 1;
            continue;
        }
        // If the reader is gone it stopped on its own error, which it reports.
        let said = format!("{entry}: {}", describe(status));
        if tx.send(Msg::End(Err(said))).is_err() {
            return Ok(());
        }
        return Err(failure(status, password.is_some()).context(format!("decoding '{entry}'")));
    }
    Ok(())
}

fn failure(code: c_int, had_password: bool) -> anyhow::Error {
    match code {
        native::ERAR_MISSING_PASSWORD => PasswordNeeded.into(),
        native::ERAR_BAD_PASSWORD => WrongPassword.into(),
        // A wrong password on an archive without a password check shows up
        // as data that fails its checksum.
        native::ERAR_BAD_DATA if had_password => WrongPassword.into(),
        c => anyhow!("{}", describe(c)),
    }
}
