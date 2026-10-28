//! An uncompressed tar is a run of header plus data records with nothing
//! shared between them, so every entry is a unit. There is no index; the
//! headers are read in one pass. On resume the same pass over what is left
//! stops at the truncation point, where the zeros read like the end of the
//! archive.

use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ::tar::EntryType;
use anyhow::{Context, Result, anyhow, bail};

use super::storage::{Reader, Storage, Tail};
use super::{Codec, Format, Item, Kind, Opened, Sink, Stream, Unit, Worker, safe_path};
use crate::fsutil;

struct Entry {
    data_start: u64,
    size: u64,
    /// For symlinks: the target, which tar keeps in the header, not the data.
    target: Option<Vec<u8>>,
}

struct Tar {
    storage: Arc<Storage>,
    entries: Vec<Entry>,
}

pub fn open(storage: &Arc<Storage>, tail: Option<Arc<Tail>>) -> Result<Opened> {
    let resuming = tail.as_ref().map(|t| t.start);
    let mut archive = ::tar::Archive::new(storage.reader(tail));
    let mut units = Vec::new();
    let mut entries = Vec::new();
    let mut end = 0;

    for entry in archive
        .entries_with_seek()
        .context("reading the tar headers")?
    {
        let e = entry.context("reading the tar headers")?;
        // The unit starts where the previous entry ended, so any long-name
        // or pax records in between belong to it, and so does an entry that
        // is skipped.
        let start = end;
        end = e
            .size()
            .checked_next_multiple_of(512)
            .and_then(|n| n.checked_add(e.raw_file_position()))
            .ok_or_else(|| anyhow!("reading the tar headers: an entry size is invalid"))?;
        let Some((item, target)) = describe(&e)? else {
            continue;
        };
        units.push(Unit {
            start,
            key: entries.len(),
            items: vec![item],
            encrypted: false,
        });
        entries.push(Entry {
            data_start: e.raw_file_position(),
            size: e.size(),
            target,
        });
    }
    size_links(units.iter_mut().flat_map(|u: &mut Unit| &mut u.items));

    Ok(Opened {
        name: "tar",
        format: Box::new(Tar {
            storage: storage.clone(),
            entries,
        }),
        units,
        tail_start: resuming.unwrap_or(end),
    })
}

/// What a tar entry extracts to, and a symlink's target, which tar keeps in
/// the header rather than the data. `None` for entries that extract to
/// nothing: "./", and pax global headers, which carry defaults such as the
/// commit id `git archive` records. Shared with compressed tar, so both
/// refuse and accept exactly the same entries. A hard link's size is left
/// at 0 for [`size_links`] to fill in.
pub(super) fn describe<R: Read>(
    e: &::tar::Entry<'_, R>,
) -> Result<Option<(Item, Option<Vec<u8>>)>> {
    let header = e.header();
    let name = String::from_utf8_lossy(&e.path_bytes()).into_owned();
    let kind = match header.entry_type() {
        EntryType::Regular | EntryType::Continuous => Kind::File,
        EntryType::Directory => Kind::Dir,
        EntryType::Symlink => Kind::Symlink,
        EntryType::Link => {
            let target = e
                .link_name()?
                .map(|t| t.to_string_lossy().into_owned())
                .unwrap_or_default();
            Kind::HardLink(safe_path(&target).ok_or_else(|| {
                anyhow!(
                    "hard link '{name}' points outside the destination folder; extraction refused"
                )
            })?)
        }
        EntryType::XGlobalHeader => return Ok(None),
        EntryType::GNUSparse => {
            bail!("'{name}' is a sparse file, which is not supported")
        }
        other => bail!("'{name}' is a {other:?} entry, which cannot be created"),
    };
    let Some(path) = safe_path(&name) else {
        // "./" at the top of many tarballs names the output folder itself.
        if kind == Kind::Dir {
            return Ok(None);
        }
        bail!("'{name}' would be written outside the destination folder; extraction refused");
    };
    let target = match kind {
        Kind::Symlink => Some(
            e.link_name_bytes()
                .map(|b| b.into_owned())
                .unwrap_or_default(),
        ),
        _ => None,
    };
    let size = match &kind {
        Kind::File => e.size(),
        Kind::Symlink => target.as_ref().map_or(0, |t| t.len() as u64),
        Kind::HardLink(_) | Kind::Dir => 0,
    };
    // GNU tar writes times before 1970 as negative base-256 numbers, which
    // the tar crate hands back in two's complement.
    let mtime = header
        .mtime()
        .ok()
        .and_then(|s| fsutil::from_epoch_secs(s as i64));
    let mode = header.mode().ok();
    Ok(Some((
        Item {
            path,
            size,
            kind,
            mode,
            mtime,
        },
        target,
    )))
}

/// Gives every hard link the size of the file it points to. Links are
/// copied where the filesystem can't hard link, so the space check counts
/// them at that size, and the file may come after the link in the archive,
/// so this waits for the whole listing.
pub(super) fn size_links<'a>(items: impl IntoIterator<Item = &'a mut Item>) {
    let mut items: Vec<&mut Item> = items.into_iter().collect();
    let files: HashMap<PathBuf, u64> = items
        .iter()
        .filter(|i| i.kind == Kind::File)
        .map(|i| (i.path.clone(), i.size))
        .collect();
    for item in &mut items {
        if let Kind::HardLink(target) = &item.kind {
            item.size = files.get(target).copied().unwrap_or(0);
        }
    }
}

impl Format for Tar {
    fn worker(&self, _threads: u32) -> Result<Box<dyn Worker + '_>> {
        Ok(Box::new(TarWorker {
            fmt: self,
            reader: self.storage.reader(None),
        }))
    }

    fn stream(&self, unit: &Unit) -> Result<Option<Stream>> {
        let entry = &self.entries[unit.key];
        if unit.items[0].kind != Kind::File {
            return Ok(None);
        }
        Ok(Some(Stream {
            data_start: entry.data_start,
            packed: entry.size,
            codec: Codec::Stored,
            crc: None,
        }))
    }
}

struct TarWorker<'a> {
    fmt: &'a Tar,
    reader: Reader,
}

impl Worker for TarWorker<'_> {
    fn extract(&mut self, unit: &Unit, sink: &mut dyn Sink) -> Result<()> {
        let entry = &self.fmt.entries[unit.key];
        let item = &unit.items[0];
        if let Some(target) = &entry.target {
            return sink.item(item, &mut Cursor::new(target));
        }
        if !matches!(item.kind, Kind::File) {
            return sink.item(item, &mut std::io::empty());
        }
        self.reader.seek(SeekFrom::Start(entry.data_start))?;
        sink.item(item, &mut (&mut self.reader).take(entry.size))
    }
}

/// What ends a restored tar. Hard links whose targets are still in the
/// archive get their entries back, since the originals were cut off with the
/// extracted part and the link can't be made before its target exists. Then
/// the two zero blocks that mark the end.
pub fn closing(hard_links: &[(&Path, &Path)]) -> Result<Vec<u8>> {
    let mut b = ::tar::Builder::new(Vec::new());
    for (path, target) in hard_links {
        let mut h = ::tar::Header::new_gnu();
        h.set_entry_type(EntryType::Link);
        h.set_size(0);
        h.set_mode(0o644);
        b.append_link(&mut h, path, target)
            .with_context(|| format!("writing the tar entry for '{}'", path.display()))?;
    }
    Ok(b.into_inner()?)
}
