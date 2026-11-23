use std::path::Path;
use std::time::SystemTime;

use hatchzip_core::archive::{Item, Kind};
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

pub const VERSION: u8 = 1;

#[derive(Serialize)]
pub struct JsonDocument<T> {
    pub version: u8,
    pub archives: Vec<T>,
}

impl<T> JsonDocument<T> {
    pub fn new(archives: Vec<T>) -> Self {
        Self {
            version: VERSION,
            archives,
        }
    }
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveState {
    Fresh,
    Resuming,
}

#[derive(Serialize)]
pub struct ArchiveMetadata {
    pub archive_path: String,
    pub state: Option<ArchiveState>,
    pub format: Option<String>,
    pub archive_size_bytes: Option<u64>,
    pub entry_count: Option<usize>,
    pub remaining_uncompressed_size_bytes: Option<u64>,
}

#[derive(Serialize)]
pub struct ListArchive {
    #[serde(flatten)]
    pub archive: ArchiveMetadata,
    pub entries: Option<Vec<ListEntry>>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct ListEntry {
    pub path: String,
    pub kind: EntryKind,
    pub size_bytes: u64,
    pub mode: Option<u32>,
    pub modified_at: Option<String>,
    pub encrypted: bool,
    pub hard_link_target: Option<String>,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    HardLink,
}

impl ListEntry {
    pub fn from_item(item: &Item, encrypted: bool) -> Self {
        let (kind, hard_link_target) = match &item.kind {
            Kind::File => (EntryKind::File, None),
            Kind::Dir => (EntryKind::Directory, None),
            Kind::Symlink => (EntryKind::Symlink, None),
            Kind::HardLink(target) => (EntryKind::HardLink, Some(member_path(target))),
        };

        Self {
            path: member_path(&item.path),
            kind,
            size_bytes: item.size,
            mode: item.mode,
            modified_at: item
                .mtime
                .as_ref()
                .and_then(|time| format_modified_at(*time)),
            encrypted,
            hard_link_target,
        }
    }
}

#[derive(Serialize)]
pub struct CheckArchive {
    #[serde(flatten)]
    pub archive: ArchiveMetadata,
    pub space_plan: Option<CheckSpacePlan>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct CheckSpacePlan {
    pub needed_bytes: u64,
    pub available_bytes: u64,
    pub reserved_bytes: u64,
    pub fits_in_place: Option<bool>,
    pub fits_with_archive_kept: Option<bool>,
    pub in_place_refusal_reason: Option<String>,
}

pub fn archive_path(path: &Path) -> String {
    hatchzip_core::archive::storage::first_volume(path)
        .to_string_lossy()
        .into_owned()
}

pub fn member_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn format_modified_at(value: SystemTime) -> Option<String> {
    OffsetDateTime::from(value)
        .to_offset(UtcOffset::UTC)
        .format(&Rfc3339)
        .ok()
}
