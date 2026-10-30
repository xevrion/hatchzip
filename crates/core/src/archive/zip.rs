//! ZIP compresses every entry on its own, so each entry is a unit.

use std::io::{Read, Seek};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ::zip::extra_fields::ExtraField;
use ::zip::read::{ArchiveOffset, Config, ZipArchiveMetadata};
use ::zip::result::ZipError;
use ::zip::{CompressionMethod, ZipArchive};
use anyhow::{Context, Result, anyhow, bail};

use super::storage::{Reader, Storage, Tail};
use super::{
    Codec, Format, Item, Kind, Opened, PasswordNeeded, Sink, Stream, Unit, Worker, WrongPassword,
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
    let resuming = tail.clone();
    let mut zip = open_archive(move || storage.reader(tail.clone()))?;

    let tail_start = match resuming {
        Some(t) => t.start,
        None => {
            if zip.has_overlapping_entries()? {
                bail!(
                    "entries in this archive overlap, which in-place extraction \
                     cannot handle safely"
                );
            }
            if let Some(name) = duplicate_name(storage, zip.central_directory_start())? {
                bail!("'{name}' appears more than once in the archive; extraction refused");
            }
            let mut end = 0;
            for i in 0..zip.len() {
                let f = zip.by_index_raw(i)?;
                end = end.max(f.data_start().unwrap_or(f.header_start()) + f.compressed_size());
            }
            end
        }
    };

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
        // where no file or link can be made.
        let path = e
            .enclosed_name()
            .filter(|p| e.is_dir() || !p.as_os_str().is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "'{name}' would be written outside the destination folder; extraction refused"
                )
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
        name: "zip",
        format: Box::new(Zip {
            storage: storage.clone(),
            meta,
            password: password.map(str::to_owned),
        }),
        units,
        tail_start,
    })
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

/// A name two records of the central directory at `dir_start` share. The
/// zip crate keys entries by name, so it keeps only the last of them, and
/// the engine's own duplicate check never sees the others. Tools that read
/// the first copy would show something other than what lands on disk.
fn duplicate_name(storage: &Storage, dir_start: u64) -> Result<Option<String>> {
    use std::io::{BufReader, Read, Seek, SeekFrom};

    let mut r = BufReader::with_capacity(1 << 20, storage.reader(None));
    r.seek(SeekFrom::Start(dir_start))?;
    let mut seen = std::collections::HashSet::new();
    let mut fixed = [0u8; 46];
    loop {
        // The records end where the end records begin, or with the file.
        match r.read_exact(&mut fixed) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e).context("reading the ZIP central directory"),
        }
        if u32_at(&fixed, 0) != Some(CDFH) {
            return Ok(None);
        }
        let len = |at| u16_at(&fixed, at).map_or(0, usize::from);
        let mut name = vec![0; len(28)];
        r.read_exact(&mut name)
            .context("reading the ZIP central directory")?;
        r.seek_relative((len(30) + len(32)) as i64)?;
        if !seen.insert(name.clone()) {
            return Ok(Some(String::from_utf8_lossy(&name).into_owned()));
        }
    }
}

const CDFH: u32 = 0x0201_4b50;
const EOCD: u32 = 0x0605_4b50;
const ZIP64_EOCD: u32 = 0x0606_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;

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

    // The comment length must fit what is left, so a signature inside the
    // comment is not mistaken for the record.
    let eocd = (0..t.len().saturating_sub(21))
        .rev()
        .find(|&i| {
            u32_at(t, i) == Some(EOCD)
                && u16_at(t, i + 20).is_some_and(|n| i + 22 + n as usize <= t.len())
        })
        .ok_or_else(damaged)?;
    let comment_len = u16_at(t, eocd + 20).ok_or_else(damaged)? as usize;
    let comment = &t[eocd + 22..eocd + 22 + comment_len];

    let zip64 = eocd >= 20 && u32_at(t, eocd - 20) == Some(ZIP64_LOCATOR);
    let (cd_end, cd_size, cd_offset) = if zip64 {
        let locator = eocd - 20;
        let record = (0..locator.saturating_sub(55))
            .rev()
            .find(|&i| {
                u32_at(t, i) == Some(ZIP64_EOCD)
                    && u64_at(t, i + 4).is_some_and(|n| i as u64 + 12 + n == locator as u64)
            })
            .ok_or_else(damaged)?;
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
    let needs_zip64 =
        zip64 || count >= 0xFFFF || cd_size >= 0xFFFF_FFFF || cd_offset >= 0xFFFF_FFFF;
    if needs_zip64 {
        let record_offset = cd_offset + cd_size;
        out.extend(ZIP64_EOCD.to_le_bytes());
        // Size of the rest of the record, without extensible data.
        out.extend(44u64.to_le_bytes());
        // Made by and needed: 4.5, the first version with ZIP64.
        out.extend(45u16.to_le_bytes());
        out.extend(45u16.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(count.to_le_bytes());
        out.extend(count.to_le_bytes());
        out.extend(cd_size.to_le_bytes());
        out.extend(cd_offset.to_le_bytes());
        out.extend(ZIP64_LOCATOR.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(record_offset.to_le_bytes());
        out.extend(1u32.to_le_bytes());
    }
    // Saturated values tell readers to take them from the ZIP64 record.
    let count16 = count.min(0xFFFF) as u16;
    out.extend(EOCD.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(count16.to_le_bytes());
    out.extend(count16.to_le_bytes());
    out.extend((cd_size.min(0xFFFF_FFFF) as u32).to_le_bytes());
    out.extend((cd_offset.min(0xFFFF_FFFF) as u32).to_le_bytes());
    out.extend((comment.len() as u16).to_le_bytes());
    out.extend_from_slice(comment);
    Ok(out)
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

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Seek, SeekFrom, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ::zip::write::{SimpleFileOptions, ZipWriter};

    use super::*;

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
}
