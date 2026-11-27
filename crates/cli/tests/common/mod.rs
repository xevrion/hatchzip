//! Helpers shared by the end to end suites: sample content, running the real
//! binary, and checking what landed on disk.

// Each suite is its own crate and uses a different subset.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub type Files = BTreeMap<String, Vec<u8>>;

/// Every builder also adds this as an explicit directory entry.
pub const EMPTY_DIR: &str = "game/saves";

pub fn noise(len: usize, seed: u64) -> Vec<u8> {
    // xorshift, so the data doesn't compress and tests stay deterministic.
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

pub fn sample_files() -> Files {
    let mut files = Files::new();
    files.insert("game/data/big.pak".into(), noise(3_000_000, 1));
    files.insert("game/data/small.pak".into(), noise(70_000, 2));
    files.insert(
        "game/config/settings.ini".into(),
        b"volume=7\n".repeat(20_000),
    );
    files.insert("game/empty.txt".into(), Vec::new());
    files.insert(
        "game/naïve résumé/日本語.txt".into(),
        "unicode names survive\n".into(),
    );
    for i in 0..40 {
        files.insert(
            format!("game/many/file{i:02}.txt"),
            format!("file number {i}\n").into(),
        );
    }
    files
}

/// Where runs note the folders chosen for them, instead of the profile of
/// whoever runs the tests. Records are per archive path, and every test has
/// its own folder, so the suites can share it.
pub fn state_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("chosen-outputs")
}

/// Puts a link at `path` that points nowhere. Windows can only make one with
/// Developer Mode on or as admin, so tests step aside when this returns
/// false.
pub fn dangling_link(target: &Path, path: &Path) -> bool {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, path).is_ok();
    #[cfg(windows)]
    return std::os::windows::fs::symlink_file(target, path).is_ok();
    #[cfg(not(any(unix, windows)))]
    return false;
}

pub fn hatchzip(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hatchzip"));
    cmd.args(args);
    cmd.env("HATCHZIP_STATE_DIR", state_dir());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

pub fn extract(archive: &Path, out: &Path, env: &[(&str, &str)]) -> Output {
    let (a, o) = (archive.to_str().unwrap(), out.to_str().unwrap());
    hatchzip(&["extract", a, "-o", o, "--keep-free", "0", "--yes"], env)
}

pub fn assert_ok(out: &Output) {
    assert!(
        out.status.success(),
        "hatchzip failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

pub fn assert_json_listing(input: &Path, expected_archive_path: &Path) {
    let run = hatchzip(&["list", input.to_str().unwrap(), "--json"], &[]);
    assert_ok(&run);

    let document: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON: {e}\nstdout:\n{}",
            String::from_utf8_lossy(&run.stdout)
        )
    });

    assert_eq!(document["version"], 1);

    let archives = document["archives"].as_array().unwrap();
    assert_eq!(archives.len(), 1);

    let result = &archives[0];

    assert_eq!(
        result["archive_path"],
        expected_archive_path.to_string_lossy().to_string()
    );
    assert_eq!(result["state"], "fresh");
    assert!(result["format"].as_str().is_some());
    assert!(result["archive_size_bytes"].as_u64().is_some());
    assert!(
        result["remaining_uncompressed_size_bytes"]
            .as_u64()
            .is_some()
    );
    assert!(result["error"].is_null());

    let entries = result["entries"].as_array().unwrap();

    assert_eq!(result["entry_count"].as_u64(), Some(entries.len() as u64));

    for entry in entries {
        assert!(entry["path"].as_str().is_some());
        assert!(entry["kind"].as_str().is_some());
        assert!(entry["size_bytes"].as_u64().is_some());
        assert!(entry["encrypted"].is_boolean());
        assert!(entry.get("mode").is_some());
        assert!(entry.get("modified_at").is_some());
        assert!(entry.get("hard_link_target").is_some());
    }
}

pub fn assert_tree(out: &Path, files: &Files) {
    for (name, data) in files {
        let got = fs::read(out.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(got == *data, "{name} differs from the archive");
    }
    assert!(out.join(EMPTY_DIR).is_dir(), "{EMPTY_DIR} is missing");
    let mut stray = Vec::new();
    for entry in walk(out) {
        let rel = entry
            .strip_prefix(out)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if entry.is_file() && !files.contains_key(&rel) {
            stray.push(rel);
        }
    }
    assert!(stray.is_empty(), "unexpected files left behind: {stray:?}");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut all = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() && !p.is_symlink() {
            all.extend(walk(&p));
        }
        all.push(p);
    }
    all
}

pub fn side_files(archive: &Path) -> [PathBuf; 2] {
    let s = archive.as_os_str().to_string_lossy();
    [
        PathBuf::from(format!("{s}.hatchzip")),
        PathBuf::from(format!("{s}.hatchzip-tail")),
    ]
}

/// The saved folder entries, written only when the archive has some.
pub fn folders_file(archive: &Path) -> PathBuf {
    PathBuf::from(format!("{}.hatchzip-folders", archive.display()))
}

pub fn assert_consumed(archive: &Path) {
    assert!(!archive.exists(), "the archive should be gone");
    for p in side_files(archive)
        .into_iter()
        .chain([folders_file(archive)])
    {
        assert!(!p.exists(), "{} should be cleaned up", p.display());
    }
}

/// Kills a run at both crash points after `after` commits, resumes it, and
/// checks the result. `build` makes a fresh archive in the given folder and
/// returns the path to pass to `extract`.
pub fn crash_and_resume(build: impl Fn(&Path) -> PathBuf, files: &Files, afters: &[u32]) {
    for point in ["after-extract", "after-journal"] {
        for after in afters {
            let tmp = tempfile::TempDir::new().unwrap();
            let archive = build(tmp.path());
            let out = tmp.path().join("out");
            let after = after.to_string();

            let crashed = extract(
                &archive,
                &out,
                &[
                    ("HATCHZIP_BATCH_ENTRIES", "1"),
                    ("HATCHZIP_CRASH_AT", point),
                    ("HATCHZIP_CRASH_AFTER", &after),
                ],
            );
            assert!(
                !crashed.status.success(),
                "{point}#{after} should have crashed"
            );
            for p in side_files(&archive) {
                assert!(p.exists(), "{point}#{after}: {} is missing", p.display());
            }

            assert_ok(&extract(&archive, &out, &[]));
            assert_tree(&out, files);
            assert_consumed(&archive);
        }
    }
}

/// A ZIP of `files` and the `EMPTY_DIR` entry at `<dir>/<name>`, cycling
/// through stored, deflate and bzip2.
pub fn zip_of(dir: &Path, name: &str, files: &Files) -> PathBuf {
    use std::io::Write;
    use zip::CompressionMethod;
    use zip::write::{SimpleFileOptions, ZipWriter};

    let methods = [
        CompressionMethod::Stored,
        CompressionMethod::Deflated,
        CompressionMethod::Bzip2,
    ];
    let mut w = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    w.add_directory(format!("{EMPTY_DIR}/"), SimpleFileOptions::default())
        .unwrap();
    for (i, (n, data)) in files.iter().enumerate() {
        let opts = SimpleFileOptions::default().compression_method(methods[i % methods.len()]);
        w.start_file(n, opts).unwrap();
        w.write_all(data).unwrap();
    }
    let path = dir.join(name);
    fs::write(&path, w.finish().unwrap().into_inner()).unwrap();
    path
}

/// A 7z at `<dir>/<name>`. Solid ones get three blocks, so there is
/// something to commit between them.
pub fn sevenz_of(dir: &Path, name: &str, files: &Files, solid: bool) -> PathBuf {
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter, SourceReader};

    let path = dir.join(name);
    let mut w = ArchiveWriter::create(&path).unwrap();
    w.push_archive_entry::<&[u8]>(ArchiveEntry::new_directory(EMPTY_DIR), None)
        .unwrap();
    let all: Vec<_> = files.iter().collect();
    if solid {
        for chunk in all.chunks(all.len().div_ceil(3)) {
            let entries = chunk
                .iter()
                .map(|(n, _)| ArchiveEntry::new_file(n))
                .collect();
            let readers = chunk
                .iter()
                .map(|(_, d)| SourceReader::new(std::io::Cursor::new(d.to_vec())))
                .collect();
            w.push_archive_entries(entries, readers).unwrap();
        }
    } else {
        for (n, data) in all {
            w.push_archive_entry(ArchiveEntry::new_file(n), Some(data.as_slice()))
                .unwrap();
        }
    }
    w.finish().unwrap();
    path
}

pub fn tar_bytes_of(files: &Files) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut dir = tar::Header::new_gnu();
    dir.set_entry_type(tar::EntryType::Directory);
    dir.set_mode(0o755);
    dir.set_size(0);
    b.append_data(&mut dir, EMPTY_DIR, std::io::empty())
        .unwrap();
    for (n, data) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(1_700_000_000);
        b.append_data(&mut h, n, data.as_slice()).unwrap();
    }
    b.into_inner().unwrap()
}

pub fn tar_of(dir: &Path, name: &str, files: &Files) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, tar_bytes_of(files)).unwrap();
    path
}

pub fn targz_of(dir: &Path, name: &str, files: &Files) -> PathBuf {
    use std::io::Write;

    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
    e.write_all(&tar_bytes_of(files)).unwrap();
    let path = dir.join(name);
    fs::write(&path, e.finish().unwrap()).unwrap();
    path
}

/// In two frames, since a single frame can't be extracted in place.
pub fn tarzst_of(dir: &Path, name: &str, files: &Files) -> PathBuf {
    use std::io::Write;

    let tar = tar_bytes_of(files);
    let half = tar.len() / 2 / 512 * 512;
    let mut out = Vec::new();
    for part in [&tar[..half], &tar[half..]] {
        let mut e = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        e.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))
            .unwrap();
        e.write_all(part).unwrap();
        out.extend_from_slice(&e.finish().unwrap());
    }
    let path = dir.join(name);
    fs::write(&path, out).unwrap();
    path
}

/// In blocks small enough to make several of a tiny tree, since a single one
/// can't be extracted in place. A block is never smaller than the dictionary.
pub fn tarxz_of(dir: &Path, name: &str, files: &Files) -> PathBuf {
    use std::io::Write;
    use std::num::NonZeroU64;

    use lzma_rust2::{XzOptions, XzWriter};

    let mut opts = XzOptions::with_preset(0);
    opts.lzma_options.dict_size = 4096;
    opts.set_block_size(NonZeroU64::new(4096));
    let mut w = XzWriter::new(Vec::new(), opts).unwrap();
    w.write_all(&tar_bytes_of(files)).unwrap();
    let path = dir.join(name);
    fs::write(&path, w.finish().unwrap()).unwrap();
    path
}

/// Replaces `archive` with `parts` volumes next to it and returns the first.
pub fn split_up(archive: &Path, parts: usize) -> PathBuf {
    let bytes = fs::read(archive).unwrap();
    fs::remove_file(archive).unwrap();
    split(&bytes, archive, parts).remove(0)
}

/// Every volume of the archive `first` names, as it is now.
pub fn snapshot(first: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let name = first.to_string_lossy();
    if name.ends_with(".z01") || name.ends_with(".Z01") {
        return split_zip_snapshot(first);
    }
    let Some(base) = name.strip_suffix(".001") else {
        return vec![(first.to_path_buf(), fs::read(first).unwrap())];
    };
    (1..)
        .map(|i| PathBuf::from(format!("{base}.{i:03}")))
        .take_while(|p| p.exists())
        .map(|p| {
            let bytes = fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect()
}

/// Every volume is byte for byte what `before` recorded.
pub fn assert_unchanged(before: &[(PathBuf, Vec<u8>)]) {
    for (p, bytes) in before {
        let now = fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        assert!(now == *bytes, "{} changed", p.display());
    }
}

pub fn assert_no_side_files(archive: &Path) {
    for p in side_files(archive)
        .into_iter()
        .chain([folders_file(archive)])
    {
        assert!(!p.exists(), "{} should be cleaned up", p.display());
    }
}

/// Cuts `bytes` into `parts` volumes named `<base>.001`, `<base>.002`, ...
pub fn split(bytes: &[u8], base: &Path, parts: usize) -> Vec<PathBuf> {
    let size = bytes.len().div_ceil(parts);
    bytes
        .chunks(size)
        .enumerate()
        .map(|(i, chunk)| {
            let mut name = base.as_os_str().to_owned();
            name.push(format!(".{:03}", i + 1));
            let p = PathBuf::from(name);
            fs::write(&p, chunk).unwrap();
            p
        })
        .collect()
}

/// How [`split_zip_segments`] cuts a ZIP into a native split ZIP.
#[derive(Clone, Copy, Debug)]
pub struct Split {
    /// Bytes per segment; the last one takes what is left, and always holds
    /// the whole end record.
    pub size: usize,
    /// What the first segment starts with: the split marker `PK\x07\x08`
    /// that `zip -s` writes, `PK00`, or nothing.
    pub marker: &'static [u8],
    /// Every record's offset and disk only in its ZIP64 extra field, and
    /// ZIP64 end records with every 16- and 32-bit value saturated.
    pub zip64: bool,
    /// A new segment at every local header and at the central directory,
    /// instead of every `size` bytes, so every cut lands on a boundary.
    pub at_headers: bool,
    /// `GAME.Z01`, ..., `GAME.ZIP`.
    pub upper: bool,
}

impl Default for Split {
    fn default() -> Self {
        Self {
            size: 200_000,
            marker: b"PK\x07\x08",
            zip64: false,
            at_headers: false,
            upper: false,
        }
    }
}

/// The segments of a native split ZIP holding what `zip`, a ZIP that is one
/// file without a comment or ZIP64 records, holds: `.z01`, `.z02`, ..., and
/// the `.zip` last, as `zip -s` writes them. Every offset in the index
/// becomes a disk and an offset from the start of that segment file, the
/// marker included, and entries, local headers and the central directory
/// run across segment boundaries wherever the cuts fall.
pub fn split_zip_segments(zip: &[u8], s: &Split) -> Vec<Vec<u8>> {
    let u16_at = |b: &[u8], i: usize| usize::from(u16::from_le_bytes([b[i], b[i + 1]]));
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as usize;
    let eocd = zip.len() - 22;
    assert_eq!(u32_at(zip, eocd), 0x0605_4b50, "a ZIP without a comment");
    let (count, cd_len, cd_at) = (
        u16_at(zip, eocd + 10),
        u32_at(zip, eocd + 12),
        u32_at(zip, eocd + 16),
    );
    let shift = s.marker.len();
    let mut joined = s.marker.to_vec();
    joined.extend_from_slice(&zip[..cd_at]);
    let mut records = Vec::new();
    let mut at = cd_at;
    for _ in 0..count {
        let len = 46 + u16_at(zip, at + 28) + u16_at(zip, at + 30) + u16_at(zip, at + 32);
        records.push((zip[at..at + len].to_vec(), u32_at(zip, at + 42) + shift));
        at += len;
    }
    assert_eq!(at, cd_at + cd_len);
    let grow = if s.zip64 { 16 } else { 0 };
    let cd_size: usize = records.iter().map(|(r, _)| r.len() + grow).sum();
    let cd_start = joined.len();
    let total = cd_start + cd_size + if s.zip64 { 56 + 20 } else { 0 } + 22;
    let starts: Vec<usize> = if s.at_headers {
        let mut v: Vec<usize> = records.iter().map(|(_, h)| *h).collect();
        v.push(cd_start);
        v.sort_unstable();
        v[0] = 0;
        v.dedup();
        v
    } else {
        (0..)
            .map(|i| i * s.size)
            .take_while(|&p| p + 22 <= total)
            .collect()
    };
    let place = |p: usize| {
        let d = starts.partition_point(|&x| x <= p) - 1;
        (d, p - starts[d])
    };
    for (record, header) in &records {
        let (disk, off) = place(*header);
        let mut r = record.clone();
        if s.zip64 {
            r[34..36].copy_from_slice(&0xFFFFu16.to_le_bytes());
            r[42..46].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            let name_end = 46 + u16_at(&r, 28);
            let extra = u16_at(&r, 30) + 16;
            r[30..32].copy_from_slice(&(extra as u16).to_le_bytes());
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
    let last = starts.len() - 1;
    if s.zip64 {
        let (rec_disk, rec_off) = place(joined.len());
        joined.extend(0x0606_4b50u32.to_le_bytes());
        joined.extend(44u64.to_le_bytes());
        joined.extend(45u16.to_le_bytes());
        joined.extend(45u16.to_le_bytes());
        joined.extend((last as u32).to_le_bytes());
        joined.extend((cd_disk as u32).to_le_bytes());
        joined.extend((count as u64).to_le_bytes());
        joined.extend((count as u64).to_le_bytes());
        joined.extend((cd_size as u64).to_le_bytes());
        joined.extend((cd_off as u64).to_le_bytes());
        joined.extend(0x0706_4b50u32.to_le_bytes());
        joined.extend((rec_disk as u32).to_le_bytes());
        joined.extend((rec_off as u64).to_le_bytes());
        joined.extend((last as u32 + 1).to_le_bytes());
        joined.extend(0x0605_4b50u32.to_le_bytes());
        for _ in 0..4 {
            joined.extend(0xFFFFu16.to_le_bytes());
        }
        joined.extend(0xFFFF_FFFFu32.to_le_bytes());
        joined.extend(0xFFFF_FFFFu32.to_le_bytes());
    } else {
        joined.extend(0x0605_4b50u32.to_le_bytes());
        joined.extend((last as u16).to_le_bytes());
        joined.extend((cd_disk as u16).to_le_bytes());
        joined.extend((count as u16).to_le_bytes());
        joined.extend((count as u16).to_le_bytes());
        joined.extend((cd_size as u32).to_le_bytes());
        joined.extend((cd_off as u32).to_le_bytes());
    }
    joined.extend(0u16.to_le_bytes());
    assert_eq!(joined.len(), total);
    starts
        .iter()
        .enumerate()
        .map(|(i, &a)| joined[a..starts.get(i + 1).copied().unwrap_or(total)].to_vec())
        .collect()
}

/// The name of segment `i` of `n` of the split ZIP `<dir>/<stem>`.
pub fn split_zip_name(dir: &Path, stem: &str, i: usize, n: usize, upper: bool) -> PathBuf {
    let ext = if i + 1 == n {
        "zip".to_string()
    } else {
        format!("z{:02}", i + 1)
    };
    let ext = if upper { ext.to_uppercase() } else { ext };
    dir.join(format!("{stem}.{ext}"))
}

/// Writes `segments` as the split ZIP `<dir>/<stem>.z01`, ..., `.zip`, and
/// returns the `.z01`.
pub fn write_split_zip(dir: &Path, stem: &str, segments: &[Vec<u8>], upper: bool) -> PathBuf {
    let n = segments.len();
    assert!(n > 1, "a split ZIP needs two segments or more");
    for (i, seg) in segments.iter().enumerate() {
        fs::write(split_zip_name(dir, stem, i, n, upper), seg).unwrap();
    }
    split_zip_name(dir, stem, 0, n, upper)
}

/// A native split ZIP of `files` and `EMPTY_DIR`, as [`zip_of`] makes it,
/// at `<dir>/game.z01`, ..., `game.zip`. Returns the `.z01`.
pub fn split_zip_of(dir: &Path, files: &Files, s: &Split) -> PathBuf {
    let whole = zip_of(dir, "whole.zip", files);
    let bytes = fs::read(&whole).unwrap();
    fs::remove_file(&whole).unwrap();
    write_split_zip(dir, "game", &split_zip_segments(&bytes, s), s.upper)
}

/// Every segment of the split ZIP whose `.z01` is `first` that is there now:
/// `.z01` upwards until one is missing, then the `.zip`.
pub fn split_zip_parts(first: &Path) -> Vec<PathBuf> {
    let name = first.file_name().unwrap().to_str().unwrap();
    let upper = name.ends_with(".Z01");
    let stem = &name[..name.len() - 4];
    let dir = first.parent().unwrap();
    let ext = |e: String| if upper { e.to_uppercase() } else { e };
    let mut parts: Vec<PathBuf> = (1..)
        .map(|i| dir.join(format!("{stem}.{}", ext(format!("z{i:02}")))))
        .take_while(|p| p.exists())
        .collect();
    let last = dir.join(format!("{stem}.{}", ext("zip".into())));
    if last.exists() {
        parts.push(last);
    }
    parts
}

/// Every segment of the split ZIP `first` starts, as it is now.
pub fn split_zip_snapshot(first: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    split_zip_parts(first)
        .into_iter()
        .map(|p| {
            let bytes = fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect()
}
