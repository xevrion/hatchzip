//! Progress record that makes an interrupted extraction resumable.
//!
//! ZIP and 7z keep their index at the end of the archive, which is the first
//! thing truncation destroys. So before the first byte is reclaimed the tail
//! of the archive is copied to a side file, and from then on the archive is
//! parsed through a reader that serves the saved tail at its old offsets.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::archive::storage::{self, Storage, Tail};
pub use crate::archive::targz::{StreamPoint, TarAt};
use crate::fsutil;

/// Version 3 added `resume`. A version 2 journal can't hold one, so it reads
/// as is. Older HatchZip builds refuse version 3, which matters: they would
/// delete a half-extracted file whose archive bytes are already freed.
const VERSION: u32 = 3;
/// An extraction that keeps the archive writes version 4, and only it does.
/// Builds before `keep` existed refuse it, which matters just as much: they
/// would resume it in place and truncate the archive the user kept. Every
/// other journal stays at version 3, so an older build can still finish it.
const KEEP_VERSION: u32 = 4;
/// An extraction that replaces existing files writes version 5, kept
/// archive or not. Builds before `replace` existed refuse it: they would
/// delete the person's files at the paths of entries left to do, and a
/// restore would not put the replaced ones back.
const REPLACE_VERSION: u32 = 5;
/// One that keeps some existing files, or both copies of them, writes
/// version 6. A build before that would finish it by deleting the files the
/// person chose to keep.
const CHOOSE_VERSION: u32 = 6;
const READABLE: [u32; 5] = [2, 3, 4, 5, 6];

#[derive(Serialize, Deserialize, Clone)]
pub struct Link {
    pub path: PathBuf,
    pub target: PathBuf,
    pub hard: bool,
}

/// Where extraction of a large file can pick up again after a crash. Its
/// output up to `written` is flushed to disk, and the archive bytes before
/// the one holding `bit` may already be freed, so this is the only way back.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Resume {
    /// The unit, by the offset it starts at.
    pub unit: u64,
    /// Bytes of output in `<name>.hatchzip-part`, flushed. Anything the file
    /// holds past this was written after the point and is thrown away.
    pub written: u64,
    /// Where decoding restarts, in bits from the start of the archive. Only
    /// deflate streams ever stop between two bytes.
    pub bit: u64,
    /// CRC-32 of the first `written` bytes of output. Unused for a
    /// compressed tar, whose checksums are per gzip member, in `stream`.
    pub crc: u32,
    /// For a compressed tar, where one stream holds every entry: the
    /// decoder's and the tar parser's state at `bit`. `written` is then how
    /// much of the entry being extracted is in its part file, if the point
    /// lies inside a file. Older builds don't know the format and refuse the
    /// journal before reading this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<Box<StreamPoint>>,
}

#[derive(Serialize, Deserialize)]
pub struct Journal {
    pub version: u32,
    pub format: String,
    /// Every part of the archive with its original length.
    pub parts: Vec<(PathBuf, u64)>,
    pub tail_start: u64,
    pub tail_crc: u32,
    pub output: PathBuf,
    pub total_size: u64,
    /// The archive is truncated to this once the latest batch is committed.
    /// Every unit starting at or after it is done.
    pub committed_len: u64,
    /// Links are made last, so nothing extracted can be written through one
    /// and every hard link's target already exists.
    pub links: Vec<Link>,
    /// The large file being extracted while its archive bytes are freed, if
    /// one is in progress. It always belongs to the highest uncommitted unit.
    #[serde(default)]
    pub resume: Option<Resume>,
    /// Every unit was extracted and the final commit was journaled: only the
    /// archive's removal and the links are left. A compressed stream lands
    /// here with its archive punched up to its last resume point, so it no
    /// longer parses from its start and must not be read again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
    /// Set while `restore` turns the archive back into a normal one: the
    /// bytes it overwrites below `committed_len`, so that an interrupted
    /// restore can be undone by putting them back and cutting off whatever it
    /// appended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restoring: Option<Vec<(u64, Vec<u8>)>>,
    /// The extraction keeps the archive: nothing is truncated, punched or
    /// deleted, and `committed_len` only records progress. A resume keeps
    /// it too, whatever it is asked.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep: bool,
    /// The extraction replaces files that were in the output folder before
    /// it started; see [`crate::replace`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace: Option<Replace>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Replace {
    /// Every file at the path of an entry left to do is in the replaced
    /// files folder. Until then nothing is extracted, so whatever sits at
    /// those paths is still the person's.
    pub moved: bool,
    /// Existing files, relative to the output, that go back over what was
    /// extracted at their paths once the extraction completes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keep: Vec<PathBuf>,
    /// Existing files that go back to their paths once the extraction
    /// completes, with the extracted copy renamed beside them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub both: Vec<PathBuf>,
}

impl Replace {
    fn chose(&self) -> bool {
        !self.keep.is_empty() || !self.both.is_empty()
    }
}

pub struct Paths {
    /// The first volume of the archive the side files belong to.
    pub archive: PathBuf,
    pub journal: PathBuf,
    pub tail: PathBuf,
}

impl Paths {
    pub fn for_archive(archive: &Path) -> Self {
        let with = |suffix: &str| {
            let mut s = archive.as_os_str().to_owned();
            s.push(suffix);
            PathBuf::from(s)
        };
        Self {
            archive: archive.to_path_buf(),
            journal: with(".hatchzip"),
            tail: with(".hatchzip-tail"),
        }
    }

    pub fn exists(&self) -> bool {
        self.journal.exists()
    }

    pub fn remove(&self) -> Result<()> {
        crate::chosen::forget(&self.archive);
        for p in [&self.tail, &self.journal] {
            match fs::remove_file(p) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("removing {}", p.display()));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

pub struct Plan {
    pub format: &'static str,
    pub tail_start: u64,
    pub output: PathBuf,
    pub total_size: u64,
    pub keep: bool,
    /// Existing files are moved aside before the first entry is extracted,
    /// and what to do with each once it completes.
    pub replace: Option<Replace>,
}

fn version_for(keep: bool, replace: Option<&Replace>) -> u32 {
    if replace.is_some_and(Replace::chose) {
        CHOOSE_VERSION
    } else if replace.is_some() {
        REPLACE_VERSION
    } else if keep {
        KEEP_VERSION
    } else {
        VERSION
    }
}

impl Journal {
    pub fn create(paths: &Paths, storage: &Storage, plan: Plan) -> Result<Self> {
        let mut tail = Vec::new();
        let mut r = storage.reader(None);
        r.seek(SeekFrom::Start(plan.tail_start))?;
        r.read_to_end(&mut tail)?;
        fsutil::write_atomic(&paths.tail, &tail)?;
        let journal = Self {
            version: version_for(plan.keep, plan.replace.as_ref()),
            format: plan.format.to_string(),
            parts: storage.record(),
            tail_start: plan.tail_start,
            tail_crc: crc32fast::hash(&tail),
            output: plan.output,
            total_size: plan.total_size,
            committed_len: storage.original_len(),
            links: Vec::new(),
            resume: None,
            done: false,
            restoring: None,
            keep: plan.keep,
            replace: plan.replace,
        };
        journal.save(paths)?;
        Ok(journal)
    }

    pub fn load(paths: &Paths) -> Result<(Self, Arc<Tail>)> {
        let bytes = fs::read(&paths.journal)
            .with_context(|| format!("reading {}", paths.journal.display()))?;
        // The version is checked before the rest is parsed, so a newer layout
        // gets the version message rather than a confusing parse error.
        #[derive(Deserialize)]
        struct Versioned {
            version: u32,
        }
        let version = serde_json::from_slice::<Versioned>(&bytes)
            .with_context(|| format!("{} is damaged", paths.journal.display()))?
            .version;
        if !READABLE.contains(&version) {
            let which = if version > VERSION {
                "a newer"
            } else {
                "an older"
            };
            bail!(
                "{} was written by {which} HatchZip version (journal format {version}); \
                 complete the extraction with that version",
                paths.journal.display()
            );
        }
        let mut journal: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is damaged", paths.journal.display()))?;
        let replaces = version == REPLACE_VERSION || version == CHOOSE_VERSION;
        if journal.replace.is_some() != replaces
            || journal.replace.as_ref().is_some_and(Replace::chose) != (version == CHOOSE_VERSION)
            || (!replaces && journal.keep != (version == KEEP_VERSION))
        {
            bail!("{} is damaged", paths.journal.display());
        }
        // Each is joined to the output folder and renamed, so each must stay
        // inside it.
        let inside = |p: &PathBuf| {
            p.components().next().is_some()
                && p.components().all(|c| matches!(c, Component::Normal(_)))
        };
        if let Some(r) = &journal.replace
            && !r.keep.iter().chain(&r.both).all(inside)
        {
            bail!("{} is damaged", paths.journal.display());
        }
        journal.version = version_for(journal.keep, journal.replace.as_ref());
        journal.check_bound(paths)?;
        let tail =
            fs::read(&paths.tail).with_context(|| format!("reading {}", paths.tail.display()))?;
        let len: u64 = journal.parts.iter().map(|(_, l)| l).sum();
        if crc32fast::hash(&tail) != journal.tail_crc
            || journal.tail_start + tail.len() as u64 != len
        {
            bail!("{} does not match its journal", paths.tail.display());
        }
        let tail = Arc::new(Tail {
            start: journal.tail_start,
            bytes: tail,
        });
        Ok((journal, tail))
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        fsutil::write_atomic(&paths.journal, &serde_json::to_vec(self)?)
    }

    /// The journal is a file next to the archive, so anyone who could put
    /// the archive there could have written it too. Whatever it names may
    /// only be what an extraction of this archive touches: the archive's own
    /// volumes, and links inside the output folder. The output folder itself
    /// is checked by [`crate::session::check_output`].
    fn check_bound(&self, paths: &Paths) -> Result<()> {
        let foreign = || {
            anyhow!(
                "{} does not belong to {}, so it was not used and nothing was modified. If the \
                 archive was moved during an extraction, move it back to resume; otherwise \
                 delete {} and {}",
                paths.journal.display(),
                paths.archive.display(),
                paths.journal.display(),
                paths.tail.display()
            )
        };
        if self.parts.is_empty() {
            return Err(foreign());
        }
        for (i, (part, _)) in self.parts.iter().enumerate() {
            if !storage::volume_path(&paths.archive, i).is_some_and(|v| fsutil::same_path(&v, part))
            {
                return Err(foreign());
            }
        }
        // Restore only ever saves bytes it overwrites inside the archive.
        let len: u64 = self.parts.iter().map(|(_, l)| l).sum();
        let within = |(at, bytes): &(u64, Vec<u8>)| {
            at.checked_add(bytes.len() as u64)
                .is_some_and(|end| end <= len)
        };
        if self
            .restoring
            .as_ref()
            .is_some_and(|r| !r.iter().all(within))
        {
            bail!(
                "{} is damaged: it records bytes outside the archive. Nothing was modified",
                paths.journal.display()
            );
        }
        let inside = |p: &Path| {
            p.components().next().is_some()
                && p.components().all(|c| matches!(c, Component::Normal(_)))
        };
        for link in &self.links {
            if !inside(&link.path) || (link.hard && !inside(&link.target)) {
                bail!(
                    "{} is damaged: it records a link outside the output folder. Nothing was \
                     modified",
                    paths.journal.display()
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, version: u32) -> Paths {
        write_with(dir, version, "")
    }

    fn write_with(dir: &Path, version: u32, extra: &str) -> Paths {
        let paths = Paths::for_archive(&dir.join("game.zip"));
        let tail = b"saved tail";
        fs::write(&paths.tail, tail).unwrap();
        let json = format!(
            r#"{{"version":{version},"format":"zip","parts":[[{},{}]],"tail_start":0,
                "tail_crc":{},"output":"out","total_size":0,"committed_len":0,"links":[]{extra}}}"#,
            serde_json::to_string(&dir.join("game.zip")).unwrap(),
            tail.len(),
            crc32fast::hash(tail)
        );
        fs::write(&paths.journal, json).unwrap();
        paths
    }

    /// Only an extraction that keeps the archive writes version 4, so older
    /// builds, which would truncate it, refuse it; in-place runs stay at 3.
    #[test]
    fn keeping_the_archive_is_version_4_and_only_that_is() {
        let dir =
            std::env::temp_dir().join(format!("hatchzip-journal-keep-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        let (journal, _) = Journal::load(&write_with(&dir, 4, r#","keep":true"#)).unwrap();
        assert!(journal.keep);
        assert_eq!(journal.version, KEEP_VERSION);
        let saved = serde_json::to_string(&journal).unwrap();
        assert!(saved.contains(r#""version":4"#) && saved.contains(r#""keep":true"#));

        let (journal, _) = Journal::load(&write(&dir, 3)).unwrap();
        assert!(!journal.keep);
        assert!(
            !serde_json::to_string(&journal)
                .unwrap()
                .contains(r#""keep":"#)
        );

        // A mismatch means someone edited the file.
        assert!(Journal::load(&write(&dir, 4)).is_err());
        assert!(Journal::load(&write_with(&dir, 3, r#","keep":true"#)).is_err());

        fs::remove_dir_all(&dir).unwrap();
    }

    /// Replacing files is version 5 whether the archive is kept or not, and
    /// no other journal may say it replaces anything.
    #[test]
    fn replacing_files_is_version_5_and_only_that_is() {
        let dir =
            std::env::temp_dir().join(format!("hatchzip-journal-replace-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        let moving = r#","replace":{"moved":false}"#;
        let (journal, _) = Journal::load(&write_with(&dir, 5, moving)).unwrap();
        assert_eq!(journal.replace, Some(replace(false)));
        assert!(!journal.keep);
        let saved = serde_json::to_string(&journal).unwrap();
        assert!(saved.contains(r#""version":5"#), "{saved}");
        assert!(!saved.contains("both"), "{saved}");

        let kept = r#","keep":true,"replace":{"moved":true}"#;
        let (journal, _) = Journal::load(&write_with(&dir, 5, kept)).unwrap();
        assert!(journal.keep && journal.replace == Some(replace(true)));
        assert_eq!(journal.version, REPLACE_VERSION);

        assert!(Journal::load(&write(&dir, 5)).is_err());
        assert!(Journal::load(&write_with(&dir, 3, moving)).is_err());
        assert!(Journal::load(&write_with(&dir, 4, &format!(r#","keep":true{moving}"#))).is_err());

        fs::remove_dir_all(&dir).unwrap();
    }

    fn replace(moved: bool) -> Replace {
        Replace {
            moved,
            keep: Vec::new(),
            both: Vec::new(),
        }
    }

    /// Keeping some existing files is version 6, which an older build
    /// refuses rather than deleting the files kept.
    #[test]
    fn choosing_per_file_is_version_6_and_only_that_is() {
        let dir =
            std::env::temp_dir().join(format!("hatchzip-journal-choose-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        let chose = r#","replace":{"moved":false,"keep":["a.txt"],"both":["b/c.txt"]}"#;
        let (journal, _) = Journal::load(&write_with(&dir, 6, chose)).unwrap();
        let r = journal.replace.as_ref().unwrap();
        assert_eq!(r.keep, [PathBuf::from("a.txt")]);
        assert_eq!(r.both, [PathBuf::from("b/c.txt")]);
        assert_eq!(journal.version, CHOOSE_VERSION);

        assert!(Journal::load(&write_with(&dir, 5, chose)).is_err());
        assert!(Journal::load(&write_with(&dir, 6, r#","replace":{"moved":false}"#)).is_err());

        fs::remove_dir_all(&dir).unwrap();
    }

    /// A run started by the previous release resumes; one started by a
    /// release that knows more is refused before anything is touched.
    #[test]
    fn reads_version_2_and_refuses_newer_journals() {
        let dir = std::env::temp_dir().join(format!("hatchzip-journal-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        let (journal, _) = Journal::load(&write(&dir, 2)).unwrap();
        assert_eq!(journal.resume, None);
        assert_eq!(journal.version, VERSION);

        let err = Journal::load(&write(&dir, CHOOSE_VERSION + 1))
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("newer HatchZip"), "{err:#}");
        let err = Journal::load(&write(&dir, 1)).err().unwrap();
        assert!(format!("{err:#}").contains("older HatchZip"), "{err:#}");

        fs::remove_dir_all(&dir).unwrap();
    }
}
