//! ZIP compresses every entry on its own, so each entry is a unit.

use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ::zip::extra_fields::ExtraField;
use ::zip::read::{ArchiveOffset, Config, ZipArchiveMetadata};
use ::zip::result::ZipError;
use ::zip::{CompressionMethod, ZipArchive};
use anyhow::{Context, Result, anyhow, bail};

use super::storage::{self, Part, Reader, Storage, Tail};
use super::{
    Codec, Damaged, Format, Item, Kind, Opened, PasswordNeeded, Sink, Stream, Unit, Unsupported,
    Worker, WrongPassword, safe_path,
};
use crate::fsutil;

struct Zip {
    storage: Arc<Storage>,
    meta: Arc<ZipArchiveMetadata>,
    password: Option<String>,
}

/// Opens the archive. The zip crate defaults to scanning the whole file for
/// a central directory header, which costs a full read of a multi-gigabyte
/// archive before extraction even starts. A normal ZIP starts at offset 0,
/// so try that first and only pay for the scan when the archive has a prefix
/// (self-extractors).
fn open_archive<R>(mut make: impl FnMut() -> R) -> Result<ZipArchive<R>>
where
    R: Read + Seek,
{
    match ZipArchive::with_config(
        Config {
            archive_offset: ArchiveOffset::Known(0),
        },
        make(),
    ) {
        Ok(zip) => Ok(zip),
        Err(_) => ZipArchive::new(make()).context("this is not a ZIP archive HatchZip can read"),
    }
}

pub fn open(
    storage: &Arc<Storage>,
    tail: Option<Arc<Tail>>,
    password: Option<&str>,
) -> Result<Opened> {
    if tail.is_none() {
        refuse_lone_segment(storage)?;
    }
    let resuming = tail.clone();
    let mut zip = open_archive(move || storage.reader(tail.clone()))?;
    let tail_start = match resuming {
        Some(t) => t.start,
        None => fresh_checks(&mut zip, storage.reader(None))?,
    };
    listing("zip", storage, zip, tail_start, password)
}

/// Refusals made before anything is touched, and where the tail starts:
/// after the last entry's data. `records` reads the central directory the
/// zip crate was given.
fn fresh_checks<R: Read + Seek>(zip: &mut ZipArchive<R>, records: Reader) -> Result<u64> {
    if zip.has_overlapping_entries()? {
        bail!(
            "entries in this archive overlap, which in-place extraction \
             cannot handle safely"
        );
    }
    check_records(records, zip.central_directory_start())?;
    let mut end = 0;
    for i in 0..zip.len() {
        let f = zip.by_index_raw(i)?;
        end = end.max(f.data_start().unwrap_or(f.header_start()) + f.compressed_size());
    }
    Ok(end)
}

/// The units of an opened ZIP, plain or split: one per entry.
fn listing<R: Read + Seek>(
    name: &'static str,
    storage: &Arc<Storage>,
    mut zip: ZipArchive<R>,
    tail_start: u64,
    password: Option<&str>,
) -> Result<Opened> {
    // The extended timestamp field (written by Info-ZIP zip) and the NTFS
    // field (written by 7-Zip and Windows tools) hold the modification time
    // in UTC; the DOS time beside them has no zone. An entry already
    // truncated away on resume has no readable header and falls back.
    let utc_mtimes: Vec<Option<SystemTime>> = (0..zip.len())
        .map(|i| {
            zip.by_index_raw(i).ok().and_then(|f| {
                f.extra_data_fields().find_map(|field| match field {
                    ExtraField::ExtendedTimestamp(t) => t
                        .mod_time()
                        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs.into())),
                    ExtraField::Ntfs(ntfs) => fsutil::from_filetime(ntfs.mtime()),
                    _ => None,
                })
            })
        })
        .collect();

    let meta = zip.metadata();
    let mut units = Vec::with_capacity(meta.len());
    for index in 0..meta.len() {
        let e = meta.entry(index)?;
        let name = e.name().map(|n| n.into_owned()).unwrap_or_default();
        // A name such as `dir/..` resolves to the destination folder itself,
        // where no file or link can be made. From zip 9.1 on, `enclosed_name`
        // also returns None for a component Windows cannot create (`CON.txt`,
        // a trailing dot); saying that would hide the reason the name is
        // refused. When the name stays under the destination, `name_problem`
        // is why it still cannot be written; otherwise it is the traversal.
        let path = e
            .enclosed_name()
            .or_else(|| drive_like_inside(&name))
            .filter(|p| e.is_dir() || !p.as_os_str().is_empty())
            .ok_or_else(|| {
                let why = safe_path(&name).and_then(|p| {
                    p.iter().find_map(|part| {
                        crate::platform::name_problem(&part.to_string_lossy())
                    })
                });
                match why {
                    Some(why) => anyhow!(
                        "'{name}' cannot be created at this location: {why}. The archive was not modified"
                    ),
                    None => anyhow!(
                        "'{name}' would be written outside the destination folder; extraction refused"
                    ),
                }
            })?;
        let kind = if e.is_dir() {
            Kind::Dir
        } else if e.is_symlink() {
            Kind::Symlink
        } else {
            Kind::File
        };
        units.push(Unit {
            start: e.header_start(),
            key: index,
            items: vec![Item {
                path,
                size: e.size(),
                kind,
                mode: e.unix_mode(),
                mtime: utc_mtimes
                    .get(index)
                    .copied()
                    .flatten()
                    .or_else(|| e.last_modified().and_then(fsutil::zip_time)),
            }],
            encrypted: e.encrypted(),
        });
    }

    Ok(Opened {
        name,
        format: Box::new(Zip {
            storage: storage.clone(),
            meta,
            password: password.map(str::to_owned),
        }),
        units,
        tail_start,
    })
}

/// zip 9.5 reads every name with Windows rules on every system, so a later
/// component such as the `c:` in Wine's `dosdevices/c:` counts as a drive and
/// `enclosed_name` refuses the whole name, though outside Windows `c:` is an
/// ordinary folder name. Such a name is accepted when HatchZip's own rules
/// find it relative, free of `..`, and creatable on this system. A name that
/// starts at a root or a drive gets no second chance: zip already strips
/// those when they are harmless, so reaching here means it found more wrong.
fn drive_like_inside(name: &str) -> Option<PathBuf> {
    let first = name.split(['/', '\\']).next().unwrap_or_default();
    let drive = first
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphabetic)
        && first.as_bytes().get(1) == Some(&b':');
    if first.is_empty() || drive {
        return None;
    }
    // On Windows the `c:` component already fails here: pushing it replaces
    // the path with a drive, which `safe_path` refuses, and `name_problem`
    // refuses any `:` as well.
    let path = safe_path(name)?;
    path.iter()
        .all(|part| crate::platform::name_problem(&part.to_string_lossy()).is_none())
        .then_some(path)
}

impl Format for Zip {
    fn worker(&self, _threads: u32) -> Result<Box<dyn Worker + '_>> {
        // Safety: the metadata was parsed from this same archive, and workers
        // only read entries below the current truncation point.
        let zip = unsafe {
            ZipArchive::unsafe_new_with_metadata(self.storage.reader(None), self.meta.clone())
        };
        Ok(Box::new(ZipWorker {
            zip,
            password: self.password.as_deref(),
        }))
    }

    fn stream(&self, unit: &Unit) -> Result<Option<Stream>> {
        let e = self.meta.entry(unit.key)?;
        // Encrypted data is decrypted by the zip crate on the way out; read
        // raw, it would be written to disk as it is.
        if unit.encrypted || e.encrypted() {
            return Ok(None);
        }
        let codec = match e.compression() {
            CompressionMethod::Stored => Codec::Stored,
            CompressionMethod::Deflated => Codec::Deflate,
            _ => return Ok(None),
        };
        // Safety: as for workers. The local header is read to find where the
        // data begins, and it is never freed before the entry is committed.
        let mut zip = unsafe {
            ZipArchive::unsafe_new_with_metadata(self.storage.reader(None), self.meta.clone())
        };
        let data_start = zip.by_index_raw(unit.key)?.data_start().ok_or_else(|| {
            anyhow!(
                "cannot locate the start of '{}'",
                unit.items[0].path.display()
            )
        })?;
        Ok(Some(Stream {
            data_start,
            packed: e.compressed_size(),
            codec,
            crc: Some(e.crc32()),
        }))
    }
}

struct ZipWorker<'a> {
    zip: ZipArchive<Reader>,
    password: Option<&'a str>,
}

impl Worker for ZipWorker<'_> {
    fn extract(&mut self, unit: &Unit, sink: &mut dyn Sink) -> Result<()> {
        // The zip crate checks the CRC as the stream ends, so corruption
        // surfaces as a read error inside the sink.
        let opened = match (unit.encrypted, self.password) {
            (false, _) => self.zip.by_index(unit.key),
            (true, Some(pw)) => self.zip.by_index_decrypt(unit.key, pw.as_bytes()),
            (true, None) => return Err(PasswordNeeded.into()),
        };
        let mut data = opened.map_err(|e| match e {
            ZipError::InvalidPassword => anyhow::Error::new(WrongPassword),
            e => anyhow::Error::new(e),
        })?;
        sink.item(&unit.items[0], &mut data)
    }
}

/// Refuses a name two records of the central directory at `dir_start`
/// share. The zip crate keys entries by name, so it keeps only the last of
/// them, and the engine's own duplicate check never sees the others. Tools
/// that read the first copy would show something other than what lands on
/// disk. Also refuses a record on another disk than the first: a ZIP that is
/// one file has only that one, and a split ZIP's records reach the crate
/// rewritten to it (see [`open_split`]).
fn check_records(reader: Reader, dir_start: u64) -> Result<()> {
    use std::io::BufReader;

    let mut r = BufReader::with_capacity(1 << 20, reader);
    r.seek(SeekFrom::Start(dir_start))?;
    let mut seen = std::collections::HashSet::new();
    let mut fixed = [0u8; 46];
    loop {
        // The records end where the end records begin, or with the file.
        match r.read_exact(&mut fixed) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e).context("reading the ZIP central directory"),
        }
        if u32_at(&fixed, 0) != Some(CDFH) {
            return Ok(());
        }
        let len = |at| u16_at(&fixed, at).map_or(0, usize::from);
        let mut name = vec![0; len(28)];
        r.read_exact(&mut name)
            .context("reading the ZIP central directory")?;
        let mut extra = vec![0; len(30)];
        r.read_exact(&mut extra)
            .context("reading the ZIP central directory")?;
        r.seek_relative(len(32) as i64)?;
        if !seen.insert(name.clone()) {
            bail!(
                "'{}' appears more than once in the archive; extraction refused",
                String::from_utf8_lossy(&name)
            );
        }
        // A ZIP64 writer may saturate the disk to 0xFFFF and keep the real
        // one in the ZIP64 extra field.
        let disk = if len(34) == 0xFFFF {
            let mut record = fixed.to_vec();
            record.extend_from_slice(&name);
            record.extend_from_slice(&extra);
            record_values(&record).map_or(0xFFFF, |v| v.disk)
        } else {
            len(34) as u64
        };
        if disk != 0 {
            bail!(
                "'{}' is recorded on part {} of a split ZIP, but this archive is one file. Put \
                 every part of the split ZIP next to it and open its .z01 part. Nothing was \
                 modified",
                String::from_utf8_lossy(&name),
                disk + 1
            );
        }
    }
}

/// The end records of a ZIP that is one file must say so. The last segment
/// of a native split ZIP on its own looks like a ZIP, and entries stored in
/// it even read, but the rest of its index points into the other segments,
/// and extracting it in place would cut that index away.
fn refuse_lone_segment(storage: &Storage) -> Result<()> {
    let len = storage.original_len();
    let want = len.min(END_SEARCH);
    let mut r = storage.reader(None);
    let t = read_at(&mut r, len - want, want as usize)?;
    let Some(at) = find_eocd(&t) else {
        return Ok(());
    };
    let mut this = u32::from(u16_at(&t, at + 4).unwrap_or(0));
    let mut cd = u32::from(u16_at(&t, at + 6).unwrap_or(0));
    let locator = (at >= 20 && u32_at(&t, at - 20) == Some(ZIP64_LOCATOR)).then(|| at - 20);
    let disks = locator.and_then(|l| u32_at(&t, l + 16)).unwrap_or(1);
    // A ZIP64 writer may saturate both to 0xFFFF and keep the real numbers
    // in the ZIP64 end record. One that cannot be found stays saturated,
    // and is refused.
    if let Some(l) = locator
        && (this == 0xFFFF || cd == 0xFFFF)
    {
        // Searched for in what was read, so a self-extractor's offsets do
        // not matter; else where the locator says, if it ends there.
        let found = match zip64_end_in(&t, l) {
            Some(i) => Some(t[i..i + 56].to_vec()),
            None => {
                let loc_at = len - want + l as u64;
                u64_at(&t, l + 8)
                    .filter(|&o| o.checked_add(56).is_some_and(|e| e <= loc_at))
                    .and_then(|o| Some((o, read_at(&mut r, o, 56).ok()?)))
                    .filter(|(o, b)| {
                        u32_at(b, 0) == Some(ZIP64_EOCD)
                            && u64_at(b, 4).and_then(|n| o.checked_add(12)?.checked_add(n))
                                == Some(loc_at)
                    })
                    .map(|(_, b)| b)
            }
        };
        if let Some(rec) = found {
            if this == 0xFFFF {
                this = u32_at(&rec, 16).unwrap_or(0xFFFF);
            }
            if cd == 0xFFFF {
                cd = u32_at(&rec, 20).unwrap_or(0xFFFF);
            }
        }
    }
    if this != 0 || cd != 0 || disks > 1 {
        let path = storage.primary();
        bail!(
            "{} is the last part of a split ZIP: its index points into the other parts, such as \
             {}, which are not next to it. Put every part in the same folder and open it again. \
             Nothing was modified",
            path.display(),
            path.with_extension("z01").display()
        );
    }
    Ok(())
}

const CDFH: u32 = 0x0201_4b50;
const EOCD: u32 = 0x0605_4b50;
const ZIP64_EOCD: u32 = 0x0606_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;
const LOCAL: u32 = 0x0403_4b50;
const DESCRIPTOR: u32 = 0x0807_4b50;

/// How far from the end the end of central directory record can start: its
/// 22 bytes, the longest comment, and the ZIP64 locator before it.
const END_SEARCH: u64 = 22 + 0xFFFF + 20;

/// The end of central directory record in `t`, searched for from the end.
/// The comment length must fit what is left, so a signature inside the
/// comment is not mistaken for the record.
fn find_eocd(t: &[u8]) -> Option<usize> {
    (0..t.len().saturating_sub(21)).rev().find(|&i| {
        u32_at(t, i) == Some(EOCD)
            && u16_at(t, i + 20).is_some_and(|n| i + 22 + n as usize <= t.len())
    })
}

/// The ZIP64 end record in `t` that ends where the locator at `locator`
/// starts, searched for from the end.
fn zip64_end_in(t: &[u8], locator: usize) -> Option<usize> {
    (0..locator.saturating_sub(55)).rev().find(|&i| {
        u32_at(t, i) == Some(ZIP64_EOCD)
            && u64_at(t, i + 4).is_some_and(|n| i as u64 + 12 + n == locator as u64)
    })
}

fn read_at(r: &mut Reader, at: u64, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0; len];
    r.seek(SeekFrom::Start(at))?;
    r.read_exact(&mut buf).context("reading the ZIP archive")?;
    Ok(buf)
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// A new central directory and end records for the entries that start below
/// `keep`, to be written at `keep`. The records come from the saved tail as
/// they are: the local header offsets in them still point at the same bytes,
/// since nothing below `keep` moves.
pub fn rebuild_index(tail: &Tail, keep: u64) -> Result<Vec<u8>> {
    let damaged =
        || anyhow!("the saved ZIP index is damaged. Run extract to complete the extraction");
    let t = &tail.bytes;

    let eocd = find_eocd(t).ok_or_else(damaged)?;
    let comment_len = u16_at(t, eocd + 20).ok_or_else(damaged)? as usize;
    let comment = &t[eocd + 22..eocd + 22 + comment_len];

    let zip64 = eocd >= 20 && u32_at(t, eocd - 20) == Some(ZIP64_LOCATOR);
    let (cd_end, cd_size, cd_offset) = if zip64 {
        let record = zip64_end_in(t, eocd - 20).ok_or_else(damaged)?;
        let size = u64_at(t, record + 40).ok_or_else(damaged)?;
        let offset = u64_at(t, record + 48).ok_or_else(damaged)?;
        (record, size, offset)
    } else {
        let size = u32_at(t, eocd + 12).ok_or_else(damaged)?;
        let offset = u32_at(t, eocd + 16).ok_or_else(damaged)?;
        (eocd, u64::from(size), u64::from(offset))
    };
    let cd_start = (cd_end as u64).checked_sub(cd_size).ok_or_else(damaged)? as usize;
    // Self-extractors made by gluing a stub onto a ZIP keep offsets relative
    // to the ZIP, while adjusted ones count from the start of the file. The
    // difference is carried over, so the new index uses the same convention
    // and the stub still finds it.
    let shift = (tail.start + cd_start as u64)
        .checked_sub(cd_offset)
        .ok_or_else(damaged)?;

    let mut records = Vec::new();
    let mut count = 0u64;
    let mut at = cd_start;
    while at < cd_end {
        if u32_at(t, at) != Some(CDFH) {
            return Err(damaged());
        }
        let name_len = u16_at(t, at + 28).ok_or_else(damaged)? as usize;
        let extra_len = u16_at(t, at + 30).ok_or_else(damaged)? as usize;
        let comment_len = u16_at(t, at + 32).ok_or_else(damaged)? as usize;
        let len = 46 + name_len + extra_len + comment_len;
        let record = t.get(at..at + len).ok_or_else(damaged)?;
        let extra = &record[46 + name_len..46 + name_len + extra_len];
        let offset = local_header_offset(record, extra).ok_or_else(damaged)?;
        if offset + shift < keep {
            records.extend_from_slice(record);
            count += 1;
        }
        at += len;
    }

    let cd_offset = keep - shift;
    let cd_size = records.len() as u64;
    let mut out = records;
    end_records(&mut out, zip64, 0, count, cd_size, cd_offset, comment);
    Ok(out)
}

/// End records for a central directory of `count` records and `cd_size`
/// bytes that starts `cd_offset` into disk `disk`, the last disk, with the
/// ZIP64 end record and its locator first when `zip64` is set or a value
/// needs them. Saturated values tell readers to take them from the ZIP64
/// record.
fn end_records(
    out: &mut Vec<u8>,
    zip64: bool,
    disk: u32,
    count: u64,
    cd_size: u64,
    cd_offset: u64,
    comment: &[u8],
) {
    let zip64 = zip64
        || count >= 0xFFFF
        || cd_size >= 0xFFFF_FFFF
        || cd_offset >= 0xFFFF_FFFF
        || disk >= 0xFFFF;
    if zip64 {
        let record_offset = cd_offset + cd_size;
        out.extend(ZIP64_EOCD.to_le_bytes());
        // Size of the rest of the record, without extensible data.
        out.extend(44u64.to_le_bytes());
        // Made by and needed: 4.5, the first version with ZIP64.
        out.extend(45u16.to_le_bytes());
        out.extend(45u16.to_le_bytes());
        out.extend(disk.to_le_bytes());
        out.extend(disk.to_le_bytes());
        out.extend(count.to_le_bytes());
        out.extend(count.to_le_bytes());
        out.extend(cd_size.to_le_bytes());
        out.extend(cd_offset.to_le_bytes());
        out.extend(ZIP64_LOCATOR.to_le_bytes());
        out.extend(disk.to_le_bytes());
        out.extend(record_offset.to_le_bytes());
        out.extend((disk + 1).to_le_bytes());
    }
    let count16 = count.min(0xFFFF) as u16;
    let disk16 = disk.min(0xFFFF) as u16;
    out.extend(EOCD.to_le_bytes());
    out.extend(disk16.to_le_bytes());
    out.extend(disk16.to_le_bytes());
    out.extend(count16.to_le_bytes());
    out.extend(count16.to_le_bytes());
    out.extend((cd_size.min(0xFFFF_FFFF) as u32).to_le_bytes());
    out.extend((cd_offset.min(0xFFFF_FFFF) as u32).to_le_bytes());
    out.extend((comment.len() as u16).to_le_bytes());
    out.extend_from_slice(comment);
}

/// The record's local header offset, from its ZIP64 extra field when the
/// 32-bit one is saturated. That field lists only the saturated values, in a
/// fixed order: uncompressed size, compressed size, offset.
fn local_header_offset(record: &[u8], mut extra: &[u8]) -> Option<u64> {
    let offset = u32_at(record, 42)?;
    if offset != 0xFFFF_FFFF {
        return Some(offset.into());
    }
    while extra.len() >= 4 {
        let id = u16_at(extra, 0)?;
        let len = u16_at(extra, 2)? as usize;
        let body = extra.get(4..4 + len)?;
        if id == 0x0001 {
            let skip = [24, 20]
                .iter()
                .filter(|&&field| u32_at(record, field) == Some(0xFFFF_FFFF))
                .count();
            return u64_at(body, skip * 8);
        }
        extra = &extra[4 + len..];
    }
    None
}

/// The format name of a native split ZIP (`game.z01`, ..., `game.zip`) in
/// the journal. Builds that don't know it refuse the journal before they
/// read the archive.
pub const SPLIT: &str = "split-zip";

/// The most a split ZIP's central directory may hold, the same bound the
/// engine puts on any archive's saved tail. It is read into memory twice:
/// as stored, and rewritten for the zip crate.
const MAX_INDEX: u64 = 1_000_000_000;

/// A native split ZIP's index, read from the end of the set, with every
/// (disk, offset) pair mapped to an offset in the joined set: part `d`
/// starts at `parts[d].start`, and an offset counts from the start of its
/// segment file, the split marker in `.z01` included.
struct SplitIndex {
    /// Where the central directory starts in the joined set.
    cd_start: u64,
    /// The central directory as stored.
    records: Vec<u8>,
    /// Each record's local header in the joined set, and the record's bytes
    /// in `records`, in the order of the central directory.
    entries: Vec<(u64, Range<usize>)>,
    comment: Vec<u8>,
    zip64: bool,
}

fn damaged(why: &str) -> anyhow::Error {
    Damaged(anyhow!(
        "the index of this split ZIP is damaged: {why}. Nothing was modified"
    ))
    .into()
}

/// The offset in the joined set of `off` bytes into disk `disk`. An offset
/// at the very end of a segment that is not the last is the start of the
/// next one, as a writer that closes a segment right before a header may
/// record it.
fn joined(parts: &[Part], disk: u64, off: u64) -> Option<u64> {
    let disk = usize::try_from(disk).ok()?;
    let p = parts.get(disk)?;
    let last = disk + 1 == parts.len();
    (off < p.len || (!last && off == p.len)).then_some(p.start + off)
}

/// A central directory record's sizes, local header offset and disk. The
/// ZIP64 extra field lists only the fixed fields that are saturated, in
/// this order.
struct RecordValues {
    uncompressed: u64,
    compressed: u64,
    offset: u64,
    disk: u64,
}

fn record_values(record: &[u8]) -> Option<RecordValues> {
    let name_len = u16_at(record, 28)? as usize;
    let extra_len = u16_at(record, 30)? as usize;
    let mut extra = record.get(46 + name_len..46 + name_len + extra_len)?;
    let mut v = RecordValues {
        uncompressed: u32_at(record, 24)?.into(),
        compressed: u32_at(record, 20)?.into(),
        offset: u32_at(record, 42)?.into(),
        disk: u16_at(record, 34)?.into(),
    };
    let saturated = [
        v.uncompressed == 0xFFFF_FFFF,
        v.compressed == 0xFFFF_FFFF,
        v.offset == 0xFFFF_FFFF,
        v.disk == 0xFFFF,
    ];
    if !saturated.contains(&true) {
        return Some(v);
    }
    while extra.len() >= 4 {
        let id = u16_at(extra, 0)?;
        let len = u16_at(extra, 2)? as usize;
        let body = extra.get(4..4 + len)?;
        if id == 0x0001 {
            let mut at = 0;
            for (i, field) in [&mut v.uncompressed, &mut v.compressed, &mut v.offset]
                .into_iter()
                .enumerate()
            {
                if saturated[i] {
                    *field = u64_at(body, at)?;
                    at += 8;
                }
            }
            if saturated[3] {
                v.disk = u32_at(body, at)?.into();
            }
            return Some(v);
        }
        extra = &extra[4 + len..];
    }
    None
}

/// The end record is authoritative: the set must have exactly the parts it
/// names, `this + 1`, no fewer and no more.
fn check_part_count(parts: &[Part], this: u64) -> Result<()> {
    let n = parts.len() - 1;
    let first = &parts[0].path;
    if this > n as u64 {
        let missing = storage::split_zip_segment(first, n).unwrap_or_else(|| first.clone());
        bail!(
            "{} is missing: this split ZIP has {} parts. Put the \
             missing parts next to {} and try again. Nothing was modified",
            missing.display(),
            this + 1,
            first.display()
        );
    }
    if this < n as u64 {
        bail!(
            "{} does not belong to this split ZIP, whose index says it has {} {}. Move it away \
             and try again. Nothing was modified",
            parts[this as usize].path.display(),
            this + 1,
            if this == 0 { "part" } else { "parts" }
        );
    }
    Ok(())
}

/// Reads a split set's end records and central directory through `read`,
/// which serves `len` bytes at an offset in the joined set, from `lowest`
/// up. Everything a later step relies on is checked here: the number of
/// segments the end record names, that every (disk, offset) lies inside
/// its segment, and that the central directory runs without a gap up to
/// the end records.
fn parse_split(
    read: &mut dyn FnMut(u64, usize) -> Result<Vec<u8>>,
    lowest: u64,
    parts: &[Part],
) -> Result<SplitIndex> {
    let n = parts.len() - 1;
    let total = parts[n].start + parts[n].len;
    let want = (total - lowest).min(END_SEARCH);
    let t = read(total - want, want as usize)?;
    let at = find_eocd(&t).ok_or_else(|| damaged("the last part has no end record"))?;
    let eocd = total - want + at as u64;
    let short = |i: usize| u64::from(u16_at(&t, at + i).unwrap_or(0));
    let word = |i: usize| u64::from(u32_at(&t, at + i).unwrap_or(0));
    let comment = t[at + 22..at + 22 + short(20) as usize].to_vec();
    let (mut this, mut cd_disk) = (short(4), short(6));
    let (mut on_this, mut count) = (short(8), short(10));
    let (mut cd_size, mut cd_off) = (word(12), word(16));
    let mut end_start = eocd;
    // Checked first when the end record holds the count itself, so a
    // missing part is named as such and not as damage it causes further on.
    if this != 0xFFFF {
        check_part_count(parts, this)?;
    }

    let locator = (at >= 20)
        .then(|| &t[at - 20..at])
        .filter(|l| u32_at(l, 0) == Some(ZIP64_LOCATOR));
    if let Some(l) = locator {
        let field = |i| u64_at(l, i).unwrap_or(0);
        let disk = u64::from(u32_at(l, 4).unwrap_or(0));
        let disks = u64::from(u32_at(l, 16).unwrap_or(0));
        let record = joined(parts, disk, field(8))
            .ok_or_else(|| damaged("the ZIP64 end record is outside the set"))?;
        let loc = eocd - 20;
        if record.checked_add(56).is_none_or(|e| e > loc) || record < lowest {
            return Err(damaged("the ZIP64 end record overlaps its locator"));
        }
        let r = read(record, 56)?;
        let size = u64_at(&r, 4).unwrap_or(0);
        if u32_at(&r, 0) != Some(ZIP64_EOCD)
            || record.checked_add(12).and_then(|e| e.checked_add(size)) != Some(loc)
        {
            return Err(damaged(
                "there is no ZIP64 end record where its locator points",
            ));
        }
        // A saturated field is in the ZIP64 record; one that is not must
        // agree with it.
        let take = |short: u64, long: u64, full: u64| -> Result<u64> {
            if short == full || short == long {
                Ok(long)
            } else {
                Err(damaged("the end record and the ZIP64 end record disagree"))
            }
        };
        this = take(this, u64::from(u32_at(&r, 16).unwrap_or(0)), 0xFFFF)?;
        cd_disk = take(cd_disk, u64::from(u32_at(&r, 20).unwrap_or(0)), 0xFFFF)?;
        on_this = take(on_this, u64_at(&r, 24).unwrap_or(0), 0xFFFF)?;
        count = take(count, u64_at(&r, 32).unwrap_or(0), 0xFFFF)?;
        cd_size = take(cd_size, u64_at(&r, 40).unwrap_or(0), 0xFFFF_FFFF)?;
        cd_off = take(cd_off, u64_at(&r, 48).unwrap_or(0), 0xFFFF_FFFF)?;
        // Writers disagree on this count; only a wrong one that says
        // several parts is refused.
        if disks > 1 && disks != this + 1 {
            return Err(damaged("the ZIP64 locator counts another number of parts"));
        }
        end_start = record;
    }

    check_part_count(parts, this)?;
    if on_this > count {
        return Err(damaged(
            "the last part holds more entries than the whole archive",
        ));
    }
    let cd_start = joined(parts, cd_disk, cd_off)
        .ok_or_else(|| damaged("the central directory is outside the set"))?;
    if cd_start.checked_add(cd_size) != Some(end_start) || cd_start < lowest {
        return Err(damaged(
            "the central directory does not end where the end records begin",
        ));
    }
    if cd_size > MAX_INDEX {
        return Err(Unsupported(anyhow!(
            "the index of this split ZIP is larger than HatchZip keeps in memory. Nothing was \
             modified"
        ))
        .into());
    }
    let records = read(cd_start, cd_size as usize)?;

    let mut entries = Vec::new();
    let mut at = 0;
    while at < records.len() {
        let r = &records[at..];
        if r.len() < 46 || u32_at(r, 0) != Some(CDFH) {
            return Err(damaged("a record is not where the previous one ends"));
        }
        let field = |i| u16_at(r, i).map_or(0, usize::from);
        let len = 46 + field(28) + field(30) + field(32);
        let record = r
            .get(..len)
            .ok_or_else(|| damaged("a record runs past the central directory"))?;
        if field(8) & 0x2000 != 0 {
            return Err(Unsupported(anyhow!(
                "this split ZIP's central directory is encrypted, which HatchZip does not \
                 read. Nothing was modified"
            ))
            .into());
        }
        let v =
            record_values(record).ok_or_else(|| damaged("a record's ZIP64 field is missing"))?;
        let header = joined(parts, v.disk, v.offset)
            .ok_or_else(|| damaged("an entry is recorded past the end of its part"))?;
        entries.push((header, at..at + len));
        at += len;
    }
    if entries.len() as u64 != count {
        return Err(damaged(
            "the central directory holds another number of entries than the end record says",
        ));
    }
    Ok(SplitIndex {
        cd_start,
        records,
        entries,
        comment,
        zip64: locator.is_some(),
    })
}

/// The index the zip crate is given for a split set: every record on disk 0
/// with its local header's offset in the joined set, and end records for a
/// ZIP that is one file. Records are rebuilt, not patched: the fixed
/// fields, a fresh ZIP64 field holding exactly the values that need it,
/// then every other extra field as stored. The archive comment is left
/// out. The crate looks for the end record from the end, and a comment
/// holding something like one could make it read another index than the
/// one checked here. The result depends on nothing but the stored index
/// and the part lengths, so a resume rebuilds it byte for byte.
fn normalize(index: &SplitIndex) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(index.records.len() + 1024);
    for (header, range) in &index.entries {
        let record = &index.records[range.clone()];
        let v =
            record_values(record).ok_or_else(|| damaged("a record's ZIP64 field is missing"))?;
        let field = |i| u16_at(record, i).map_or(0, usize::from);
        let (name_len, extra_len) = (field(28), field(30));
        let mut fixed = record[..46].to_vec();
        let mut zip64 = Vec::new();
        for (at, value) in [(24, v.uncompressed), (20, v.compressed), (42, *header)] {
            let short = if value >= 0xFFFF_FFFF {
                zip64.extend(value.to_le_bytes());
                0xFFFF_FFFF
            } else {
                value as u32
            };
            fixed[at..at + 4].copy_from_slice(&short.to_le_bytes());
        }
        fixed[34..36].copy_from_slice(&0u16.to_le_bytes());
        let mut extra = Vec::new();
        if !zip64.is_empty() {
            extra.extend(1u16.to_le_bytes());
            extra.extend((zip64.len() as u16).to_le_bytes());
            extra.extend(zip64);
        }
        let mut rest = &record[46 + name_len..46 + name_len + extra_len];
        while rest.len() >= 4 {
            let id = u16_at(rest, 0).unwrap_or(0);
            let len = 4 + u16_at(rest, 2).map_or(0, usize::from);
            let Some(field) = rest.get(..len) else {
                break;
            };
            if id != 0x0001 {
                extra.extend_from_slice(field);
            }
            rest = &rest[len..];
        }
        // Whatever does not parse as a field stays as it was.
        extra.extend_from_slice(rest);
        let extra_len = u16::try_from(extra.len())
            .map_err(|_| damaged("a record's extra fields are too long"))?;
        fixed[30..32].copy_from_slice(&extra_len.to_le_bytes());
        out.extend(fixed);
        out.extend_from_slice(&record[46..46 + name_len]);
        out.extend(extra);
        out.extend_from_slice(&record[46 + name_len + field(30)..]);
    }
    let cd_size = out.len() as u64;
    let count = index.entries.len() as u64;
    end_records(&mut out, false, 0, count, cd_size, index.cd_start, &[]);
    Ok(out)
}

fn from_tail(tail: &Tail, at: u64, len: usize) -> Result<Vec<u8>> {
    at.checked_sub(tail.start)
        .and_then(|o| usize::try_from(o).ok())
        .and_then(|o| tail.bytes.get(o..o.checked_add(len)?))
        .map(<[u8]>::to_vec)
        .ok_or_else(|| damaged("the saved index does not hold it"))
}

/// The first segment starts with the split marker, the marker of a split
/// that needed only one segment (APPNOTE 8.5.4), or straight with an entry.
fn check_split_start(storage: &Storage) -> Result<()> {
    let head = read_at(&mut storage.reader(None), 0, 4)?;
    if !matches!(&head[..], b"PK\x07\x08" | b"PK00" | b"PK\x03\x04") {
        return Err(Damaged(anyhow!(
            "{} does not start like the first part of a split ZIP. Nothing was modified",
            storage.primary().display()
        ))
        .into());
    }
    Ok(())
}

/// Every entry's local header is where the index says and agrees with it,
/// and so does the CRC-32 after an entry whose sizes follow its data, which
/// for an entry that spans segments is in a later one. A segment from
/// another copy of the archive, or renamed into the wrong place, fails here,
/// before anything is touched, rather than as a CRC error once the segments
/// before it are freed.
fn check_local_headers(storage: &Storage, index: &SplitIndex) -> Result<()> {
    let mut r = storage.reader(None);
    for (header, range) in &index.entries {
        let record = &index.records[range.clone()];
        let name = &record[46..46 + u16_at(record, 28).map_or(0, usize::from)];
        let wrong = || {
            Damaged(anyhow!(
                "'{}' is not where the index of this split ZIP says, or differs from it, so a \
                 part is damaged, in the wrong place or from another copy of the archive. \
                 Nothing was modified",
                String::from_utf8_lossy(name)
            ))
        };
        let local = read_at(&mut r, *header, 30 + name.len()).map_err(|_| wrong())?;
        let flags = u16_at(&local, 6).unwrap_or(0);
        if u32_at(&local, 0) != Some(LOCAL)
            || u16_at(&local, 8) != u16_at(record, 10)
            || flags & 1 != u16_at(record, 8).unwrap_or(0) & 1
            || &local[30..] != name
        {
            return Err(wrong().into());
        }
        let crc = u32_at(record, 16);
        let found = if flags & 0x0008 == 0 {
            u32_at(&local, 14)
        } else {
            let v = record_values(record).ok_or_else(wrong)?;
            let data_end = (header + 30 + name.len() as u64)
                .checked_add(u64::from(u16_at(&local, 28).unwrap_or(0)))
                .and_then(|d| d.checked_add(v.compressed))
                .ok_or_else(wrong)?;
            let d = read_at(&mut r, data_end, 8).map_err(|_| wrong())?;
            if u32_at(&d, 0) == Some(DESCRIPTOR) {
                u32_at(&d, 4)
            } else {
                u32_at(&d, 0)
            }
        };
        if found != crc {
            return Err(wrong().into());
        }
    }
    Ok(())
}

/// Opens a native split ZIP (`game.z01`, ..., `game.zip`). The zip crate
/// refuses a set, or reads its offsets, which count from the start of each
/// segment, as offsets in one file. So HatchZip reads the index itself and
/// gives the crate the same index rewritten for the joined set, as if it
/// were one file; decoding, the CRC checks and streaming stay the crate's
/// and the engine's, exactly as for a ZIP. `saved` is the tail a resumed
/// run saved, the index as stored, which is rewritten the same way.
pub fn open_split(
    storage: &Arc<Storage>,
    saved: Option<Arc<Tail>>,
    password: Option<&str>,
) -> Result<Opened> {
    let parts = storage.parts();
    if let Some(p) = parts.iter().find(|p| p.len == 0) {
        return Err(Damaged(anyhow!(
            "{} is empty, so this split ZIP is damaged or incomplete. Nothing was modified",
            p.path.display()
        ))
        .into());
    }
    let index = match &saved {
        Some(t) => parse_split(
            &mut |at: u64, len: usize| from_tail(t, at, len),
            t.start,
            parts,
        )?,
        None => {
            check_split_start(storage)?;
            let mut r = storage.reader(None);
            parse_split(
                &mut |at: u64, len: usize| read_at(&mut r, at, len),
                0,
                parts,
            )?
        }
    };
    let normalized = normalize(&index)?;
    // On resume, what lies between the last entry's data and the index (a
    // data descriptor) is served from the saved tail too.
    let tail = Arc::new(match &saved {
        Some(t) => {
            let mut bytes = index
                .cd_start
                .checked_sub(t.start)
                .and_then(|n| t.bytes.get(..usize::try_from(n).ok()?))
                .ok_or_else(|| damaged("the saved index starts inside the central directory"))?
                .to_vec();
            bytes.extend(normalized);
            Tail {
                start: t.start,
                bytes,
            }
        }
        None => Tail {
            start: index.cd_start,
            bytes: normalized,
        },
    });
    let mut zip = ZipArchive::with_config(
        Config {
            archive_offset: ArchiveOffset::Known(0),
        },
        storage.reader(Some(tail.clone())),
    )
    .map_err(|e| damaged(&e.to_string()))?;

    let tail_start = match &saved {
        Some(t) => t.start,
        None => {
            let end = fresh_checks(&mut zip, storage.reader(Some(tail.clone())))?;
            if end > index.cd_start {
                return Err(damaged("an entry's data runs into the central directory"));
            }
            check_local_headers(storage, &index)?;
            end
        }
    };
    // What the crate reads must be the index checked above.
    let meta = zip.metadata();
    let same = meta.len() == index.entries.len()
        && zip.central_directory_start() == index.cd_start
        && index
            .entries
            .iter()
            .enumerate()
            .all(|(i, (header, _))| meta.entry(i).is_ok_and(|e| e.header_start() == *header));
    if !same {
        return Err(damaged("the zip library reads it differently"));
    }
    listing(SPLIT, storage, zip, tail_start, password)
}

/// [`rebuild_index`] for a native split ZIP. The records of the entries
/// left are copied as stored, since their disk numbers and offsets still
/// point at the same bytes. When the cut falls inside the `.zip`, the new
/// index goes there, after the entries, as for any ZIP. Otherwise the
/// `.zip` is gone and the new index becomes a new `.zip` of its own, the
/// disk after the segment the cut falls in, since every reader opens a set
/// from its `.zip`: an index alone on the last disk is a valid split ZIP.
/// Nothing is renamed, so nothing the journal names changes its meaning.
/// Returns the bytes and whether they are that new segment.
pub fn rebuild_split_index(parts: &[Part], tail: &Tail, keep: u64) -> Result<(Vec<u8>, bool)> {
    let index = parse_split(
        &mut |at: u64, len: usize| from_tail(tail, at, len),
        tail.start,
        parts,
    )
    .map_err(|e| {
        anyhow!("{e:#}. The saved index is damaged; run extract to complete the extraction")
    })?;
    let mut out = Vec::new();
    let mut count = 0u64;
    for (header, range) in &index.entries {
        if *header < keep {
            out.extend_from_slice(&index.records[range.clone()]);
            count += 1;
        }
    }
    let n = parts.len() - 1;
    let k = parts
        .partition_point(|p| p.start < keep)
        .checked_sub(1)
        .ok_or_else(|| anyhow!("nothing is left of this split ZIP to restore"))?;
    let (disk, cd_offset, new_last) = if k == n {
        (n, keep - parts[n].start, false)
    } else {
        (k + 1, 0, true)
    };
    let disk = u32::try_from(disk).map_err(|_| anyhow!("this split ZIP has too many parts"))?;
    let cd_size = out.len() as u64;
    end_records(
        &mut out,
        index.zip64,
        disk,
        count,
        cd_size,
        cd_offset,
        &index.comment,
    );
    Ok((out, new_last))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Seek, SeekFrom, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ::zip::write::{SimpleFileOptions, ZipWriter};

    use super::*;

    /// The name zip 9.5 refuses on every system because of its `c:` folder.
    /// Windows can't create that folder, so it stays refused there.
    #[test]
    fn a_drive_like_folder_inside_a_name_is_accepted_where_it_can_be_made() {
        let name = "wine/dosdevices/c:/x.txt";
        let creatable = crate::platform::name_problem("c:").is_none();
        assert_eq!(
            drive_like_inside(name),
            creatable.then(|| ["wine", "dosdevices", "c:", "x.txt"]
                .iter()
                .collect::<PathBuf>())
        );
    }

    #[test]
    fn the_fallback_still_refuses_roots_drives_and_traversal() {
        for name in [
            "",
            "/etc/passwd",
            "/wine/c:/x",
            "\\etc\\passwd",
            "C:/x",
            "C:\\x",
            "c:x/y",
            "c:",
            "\\\\server\\share\\x",
            "//server/share/x",
            "\\\\?\\C:\\x",
            "../x",
            "a/../../x",
            "a/c:/../../x",
            "a/c:/x\0",
        ] {
            assert_eq!(drive_like_inside(name), None, "{name:?}");
        }
    }

    struct CountingReader<R> {
        inner: R,
        read: Arc<AtomicUsize>,
    }

    impl<R: Read> Read for CountingReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read.fetch_add(n, Ordering::Relaxed);
            Ok(n)
        }
    }

    impl<R: Seek> Seek for CountingReader<R> {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    fn stored_zip(len: usize) -> Vec<u8> {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file(
            "data.bin",
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        )
        .unwrap();
        w.write_all(&vec![0xAB; len]).unwrap();
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn opening_a_zip_does_not_read_the_entry_data() {
        const LEN: usize = 1 << 20;
        let bytes = stored_zip(LEN);
        let read = Arc::new(AtomicUsize::new(0));
        let mut zip = open_archive(|| CountingReader {
            inner: Cursor::new(bytes.clone()),
            read: read.clone(),
        })
        .unwrap();

        let at_open = read.load(Ordering::Relaxed);
        assert!(
            at_open < LEN / 2,
            "opening read {at_open} bytes for a {LEN} byte entry"
        );

        let mut out = Vec::new();
        zip.by_index(0).unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), LEN);
    }

    #[test]
    fn opening_a_zip_with_a_prefix_still_finds_the_archive() {
        let mut bytes = vec![0x90; 8192];
        bytes.extend(stored_zip(4096));
        let read = Arc::new(AtomicUsize::new(0));
        let mut zip = open_archive(|| CountingReader {
            inner: Cursor::new(bytes.clone()),
            read: read.clone(),
        })
        .unwrap();

        let mut out = Vec::new();
        zip.by_index(0).unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out, vec![0xAB; 4096]);
    }

    /// Too many entries for the 16-bit counts, which the end-to-end tests
    /// can't afford to extract one by one.
    #[test]
    fn rebuilds_an_index_with_more_than_65535_entries() {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default();
        for i in 0..70_000 {
            w.start_file(format!("f{i}"), opts).unwrap();
            w.write_all(i.to_string().as_bytes()).unwrap();
        }
        let bytes = w.finish().unwrap().into_inner();
        let keep = {
            let mut z = ZipArchive::new(Cursor::new(bytes.clone())).unwrap();
            z.by_index_raw(68_000).unwrap().header_start()
        };

        let tail = Tail {
            start: 0,
            bytes: bytes.clone(),
        };
        let mut restored = bytes[..keep as usize].to_vec();
        restored.extend(rebuild_index(&tail, keep).unwrap());

        let mut z = ZipArchive::new(Cursor::new(restored)).unwrap();
        assert_eq!(z.len(), 68_000);
        let mut last = String::new();
        z.by_name("f67999")
            .unwrap()
            .read_to_string(&mut last)
            .unwrap();
        assert_eq!(last, "67999");
        assert!(z.by_name("f68000").is_err());
    }

    /// `zip` as a native split ZIP behind the split marker, in segments of
    /// `size` bytes, with every record's offset and disk only in a ZIP64
    /// extra field when `zip64`: the joined bytes and the part layout.
    fn split_set(zip: &[u8], size: usize, zip64: bool) -> (Vec<u8>, Vec<Part>) {
        let eocd = zip.len() - 22;
        let (count, cd_len, cd_at) = (
            u16_at(zip, eocd + 10).unwrap() as usize,
            u32_at(zip, eocd + 12).unwrap() as usize,
            u32_at(zip, eocd + 16).unwrap() as usize,
        );
        let mut joined = b"PK\x07\x08".to_vec();
        joined.extend_from_slice(&zip[..cd_at]);
        let mut records = Vec::new();
        let mut at = cd_at;
        for _ in 0..count {
            let field = |i| u16_at(zip, at + i).unwrap() as usize;
            let len = 46 + field(28) + field(30) + field(32);
            records.push((
                zip[at..at + len].to_vec(),
                u32_at(zip, at + 42).unwrap() as usize + 4,
            ));
            at += len;
        }
        assert_eq!(at, cd_at + cd_len);
        let cd_size: usize = records
            .iter()
            .map(|(r, _)| r.len() + if zip64 { 16 } else { 0 })
            .sum();
        let cd_start = joined.len();
        let total = cd_start + cd_size + 22;
        let starts: Vec<usize> = (0..)
            .map(|i| i * size)
            .take_while(|&p| p + 22 <= total)
            .collect();
        let place = |p: usize| {
            let d = starts.partition_point(|&x| x <= p) - 1;
            (d, p - starts[d])
        };
        for (record, header) in &records {
            let (disk, off) = place(*header);
            let mut r = record.clone();
            if zip64 {
                r[34..36].copy_from_slice(&0xFFFFu16.to_le_bytes());
                r[42..46].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
                let name_end = 46 + u16_at(&r, 28).unwrap() as usize;
                let extra = u16_at(&r, 30).unwrap() + 16;
                r[30..32].copy_from_slice(&extra.to_le_bytes());
                let mut field = vec![1, 0, 12, 0];
                field.extend((off as u64).to_le_bytes());
                field.extend((disk as u32).to_le_bytes());
                r.splice(name_end..name_end, field);
            } else {
                r[34..36].copy_from_slice(&(disk as u16).to_le_bytes());
                r[42..46].copy_from_slice(&(off as u32).to_le_bytes());
            }
            joined.extend(r);
        }
        let (cd_disk, cd_off) = place(cd_start);
        joined.extend(EOCD.to_le_bytes());
        joined.extend(((starts.len() - 1) as u16).to_le_bytes());
        joined.extend((cd_disk as u16).to_le_bytes());
        joined.extend((count as u16).to_le_bytes());
        joined.extend((count as u16).to_le_bytes());
        joined.extend((cd_size as u32).to_le_bytes());
        joined.extend((cd_off as u32).to_le_bytes());
        joined.extend(0u16.to_le_bytes());
        assert_eq!(joined.len(), total);
        let parts = starts
            .iter()
            .enumerate()
            .map(|(i, &a)| Part {
                path: PathBuf::from(format!("game.z{:02}", i + 1)),
                start: a as u64,
                len: (starts.get(i + 1).copied().unwrap_or(total) - a) as u64,
            })
            .collect();
        (joined, parts)
    }

    fn many_files() -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        let mut files = Vec::new();
        for i in 0..30u32 {
            let data: Vec<u8> = (0..(i * 997) as usize)
                .map(|j| (j as u32 * 31 + i) as u8)
                .collect();
            let method = if i % 2 == 0 {
                CompressionMethod::Stored
            } else {
                CompressionMethod::Deflated
            };
            let name = format!("dir/file{i:02}.bin");
            w.start_file(
                &name,
                SimpleFileOptions::default().compression_method(method),
            )
            .unwrap();
            w.write_all(&data).unwrap();
            files.push((name, data));
        }
        (w.finish().unwrap().into_inner(), files)
    }

    fn read_joined(joined: &[u8]) -> impl FnMut(u64, usize) -> Result<Vec<u8>> + '_ {
        move |at, len| {
            let at = at as usize;
            joined
                .get(at..at + len)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| anyhow!("read past the end"))
        }
    }

    /// The zip crate reads a split set's entries through the rewritten
    /// index as if the set were one file, ZIP64 disk fields included, where
    /// the rewritten index is shorter than the stored one.
    #[test]
    fn a_split_index_reads_through_the_zip_crate_as_one_file() {
        let (zip, files) = many_files();
        for zip64 in [false, true] {
            let (joined, parts) = split_set(&zip, 7_000, zip64);
            assert!(parts.len() > 10);
            let index = parse_split(&mut read_joined(&joined), 0, &parts).unwrap();
            assert_eq!(index.entries.len(), files.len());
            let normalized = normalize(&index).unwrap();
            if zip64 {
                assert!(normalized.len() < joined.len() - index.cd_start as usize);
            }
            let mut bytes = joined[..index.cd_start as usize].to_vec();
            bytes.extend(&normalized);
            let mut z = ZipArchive::with_config(
                Config {
                    archive_offset: ArchiveOffset::Known(0),
                },
                Cursor::new(bytes),
            )
            .unwrap();
            assert_eq!(z.central_directory_start(), index.cd_start);
            for (i, (name, data)) in files.iter().enumerate() {
                let mut f = z.by_name(name).unwrap();
                assert_eq!(f.header_start(), index.entries[i].0, "{name}");
                let mut out = Vec::new();
                f.read_to_end(&mut out).unwrap();
                assert_eq!(&out, data, "{name}");
            }
        }
    }

    /// A resume rewrites the index from the saved tail alone, byte for byte
    /// as the fresh open did from the whole set.
    #[test]
    fn a_split_index_rewrites_the_same_from_the_saved_tail() {
        let (zip, _) = many_files();
        let (joined, parts) = split_set(&zip, 5_000, true);
        let fresh = parse_split(&mut read_joined(&joined), 0, &parts).unwrap();
        let lowest = fresh.cd_start - 7;
        let tail = Tail {
            start: lowest,
            bytes: joined[lowest as usize..].to_vec(),
        };
        let resumed = parse_split(
            &mut |at: u64, len: usize| from_tail(&tail, at, len),
            lowest,
            &parts,
        )
        .unwrap();
        assert_eq!(normalize(&fresh).unwrap(), normalize(&resumed).unwrap());
    }

    /// A restore's index names the disk the cut falls in, or the next one
    /// when it is a new `.zip` of its own, and reads back with the same
    /// parser as the set it came from.
    #[test]
    fn a_restored_split_index_reads_back() {
        let (zip, _) = many_files();
        let (joined, parts) = split_set(&zip, 7_000, false);
        let index = parse_split(&mut read_joined(&joined), 0, &parts).unwrap();
        let tail = Tail {
            start: index.cd_start,
            bytes: joined[index.cd_start as usize..].to_vec(),
        };
        let n = parts.len() - 1;
        let in_last = index
            .entries
            .iter()
            .map(|(h, _)| *h)
            .filter(|&h| h > parts[n].start)
            .min();
        let early = index.entries[12].0;
        for keep in [early].into_iter().chain(in_last) {
            let (bytes, new_last) = rebuild_split_index(&parts, &tail, keep).unwrap();
            let k = parts.partition_point(|p| p.start < keep) - 1;
            assert_eq!(new_last, k < n, "{keep}");
            let mut left: Vec<Part> = parts[..=k].to_vec();
            left[k].len = keep - left[k].start;
            let mut restored = joined[..keep as usize].to_vec();
            if new_last {
                left.push(Part {
                    path: PathBuf::from("game.zip"),
                    start: keep,
                    len: bytes.len() as u64,
                });
            } else {
                left[k].len += bytes.len() as u64;
            }
            restored.extend(&bytes);
            let back = parse_split(&mut read_joined(&restored), 0, &left).unwrap();
            let want = index.entries.iter().filter(|(h, _)| *h < keep).count();
            assert_eq!(back.entries.len(), want, "{keep}");
        }
    }
}
