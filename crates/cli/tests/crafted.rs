//! Archives put together by hand, with layouts no ordinary tool writes,
//! aimed at what in-place extraction relies on. Each one is either extracted
//! whole or refused before anything is touched.

mod common;

use std::fs;
use std::path::Path;

use common::*;
use tempfile::TempDir;

/// 7z's variable-length numbers, for the values these tests need.
fn number(out: &mut Vec<u8>, n: u64) {
    if n < 0x80 {
        out.push(n as u8);
    } else if n < 0x4000 {
        out.extend([0x80 | (n >> 8) as u8, n as u8]);
    } else {
        out.push(0xFF);
        out.extend(n.to_le_bytes());
    }
}

/// A 7z header for blocks of one packed stream each, with a Copy coder.
/// `packs` are the packed stream sizes, which may list more streams than
/// there are blocks; `blocks` the files of each block with their sizes;
/// `dirs` folders, which have no data.
fn header(packs: &[u64], blocks: &[&[(&str, u64)]], dirs: &[&str]) -> Vec<u8> {
    header_with_methods(packs, blocks, dirs, &vec![COPY; blocks.len()])
}

const COPY: &[u8] = &[0x00];

/// The same, with the method ID of each block's single coder, so a test can
/// name a coder the decoders don't have.
fn header_with_methods(
    packs: &[u64],
    blocks: &[&[(&str, u64)]],
    dirs: &[&str],
    methods: &[&[u8]],
) -> Vec<u8> {
    assert_eq!(methods.len(), blocks.len());
    // Header, main streams info, pack info at position 0.
    let mut h = vec![0x01, 0x04, 0x06, 0x00];
    number(&mut h, packs.len() as u64);
    h.push(0x09);
    for &p in packs {
        number(&mut h, p);
    }
    h.push(0x00);

    // Unpack info: one coder per block, then each block's size.
    h.extend([0x07, 0x0B]);
    number(&mut h, blocks.len() as u64);
    h.push(0x00);
    for method in methods {
        h.push(0x01);
        h.push(method.len() as u8);
        h.extend(*method);
    }
    h.push(0x0C);
    for files in blocks {
        number(&mut h, files.iter().map(|f| f.1).sum());
    }
    h.push(0x00);

    // Sub-streams: how many files each block holds, and the size of all
    // but its last one.
    h.extend([0x08, 0x0D]);
    for files in blocks {
        number(&mut h, files.len() as u64);
    }
    h.push(0x09);
    for files in blocks {
        for f in files.iter().take(files.len().saturating_sub(1)) {
            number(&mut h, f.1);
        }
    }
    h.extend([0x00, 0x00]);

    // Files: those of the blocks in order, then the folders.
    let names: Vec<&str> = blocks
        .iter()
        .flat_map(|b| b.iter().map(|f| f.0))
        .chain(dirs.iter().copied())
        .collect();
    h.push(0x05);
    number(&mut h, names.len() as u64);
    if !dirs.is_empty() {
        let streams = names.len() - dirs.len();
        let mut bits = vec![0u8; names.len().div_ceil(8)];
        for i in streams..names.len() {
            bits[i / 8] |= 0x80 >> (i % 8);
        }
        h.push(0x0E);
        number(&mut h, bits.len() as u64);
        h.extend(bits);
    }
    let mut utf16 = vec![0x00];
    for n in &names {
        for c in n.encode_utf16().chain([0]) {
            utf16.extend(c.to_le_bytes());
        }
    }
    h.push(0x11);
    number(&mut h, utf16.len() as u64);
    h.extend(utf16);
    h.extend([0x00, 0x00]);
    h
}

/// A whole 7z: the start header, then `body`, whose header is the `len`
/// bytes found `at` bytes in.
fn sevenz(body: &[u8], at: usize, len: usize) -> Vec<u8> {
    let mut start = Vec::with_capacity(20);
    start.extend((at as u64).to_le_bytes());
    start.extend((len as u64).to_le_bytes());
    start.extend(crc32fast::hash(&body[at..at + len]).to_le_bytes());
    let mut out = b"7z\xBC\xAF\x27\x1C\x00\x04".to_vec();
    out.extend(crc32fast::hash(&start).to_le_bytes());
    out.extend(start);
    out.extend(body);
    out
}

/// The packed streams in order, with the header right after them, as 7-Zip
/// lays an archive out.
fn sevenz_with_header_last(packs: &[&[u8]], header: &[u8]) -> Vec<u8> {
    let mut body = packs.concat();
    let at = body.len();
    body.extend(header);
    sevenz(&body, at, header.len())
}

fn refused(archive: &Path, out: &Path, says: &str) {
    refused_env(archive, out, says, &[]);
}

fn refused_env(
    archive: &Path,
    out: &Path,
    says: &str,
    env: &[(&str, &str)],
) -> std::process::Output {
    let before = fs::read(archive).unwrap();
    let run = extract(archive, out, env);
    assert!(!run.status.success(), "the archive should be refused");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains(says), "{stderr}");
    assert_eq!(fs::read(archive).unwrap(), before);
    assert_no_side_files(archive);
    run
}

/// A block with no packed bytes starts where the next block does. Committed
/// on its own, it would truncate that block's data before it was extracted;
/// its empty file is made with the folders instead.
#[test]
fn a_7z_block_without_packed_bytes_does_not_cut_off_the_next() {
    let tmp = TempDir::new().unwrap();
    let data = noise(3000, 1);
    let last = noise(500, 2);
    let h = header(
        &[0, 3000, 500],
        &[
            &[("empty.txt", 0)],
            &[("data.bin", 3000)],
            &[("last.bin", 500)],
        ],
        &[EMPTY_DIR],
    );
    let archive = tmp.path().join("shared.7z");
    fs::write(&archive, sevenz_with_header_last(&[&data, &last], &h)).unwrap();
    let out = tmp.path().join("out");

    assert_ok(&extract(&archive, &out, &[("HATCHZIP_BATCH_ENTRIES", "1")]));
    let mut files = Files::new();
    files.insert("empty.txt".into(), Vec::new());
    files.insert("data.bin".into(), data);
    files.insert("last.bin".into(), last);
    assert_tree(&out, &files);
    assert_consumed(&archive);
}

/// The same block claiming data it has no bytes for is damage.
#[test]
fn a_7z_block_without_packed_bytes_but_with_data_is_refused() {
    let tmp = TempDir::new().unwrap();
    let data = noise(3000, 1);
    let h = header(
        &[0, 3000],
        &[&[("ghost.bin", 10)], &[("data.bin", 3000)]],
        &[],
    );
    let archive = tmp.path().join("ghost.7z");
    fs::write(&archive, sevenz_with_header_last(&[&data], &h)).unwrap();
    refused(&archive, &tmp.path().join("out"), "header is inconsistent");
}

/// A last packed stream no block uses would be truncated with the block
/// before it, and could hold the header.
#[test]
fn a_7z_with_packed_data_no_block_uses_is_refused() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (noise(1000, 1), noise(1000, 2));
    let h = header(
        &[1000, 1000, 300],
        &[&[("a.bin", 1000)], &[("b.bin", 1000)]],
        &[],
    );
    let archive = tmp.path().join("unused.7z");
    fs::write(&archive, sevenz_with_header_last(&[&a, &b, &[0; 300]], &h)).unwrap();
    refused(&archive, &tmp.path().join("out"), "no block uses");
}

/// The header doubles as the data of the last block. Committing that block
/// would truncate the header, which every resume and restore reads again.
#[test]
fn a_7z_whose_header_is_inside_a_block_is_refused() {
    let tmp = TempDir::new().unwrap();
    let victim = noise(1000, 1);
    let h = header(
        &[1000, 400],
        &[&[("victim.bin", 1000)], &[("carrier.bin", 400)]],
        &[],
    );
    let mut carrier = h.clone();
    carrier.resize(400, 0);
    let archive = tmp.path().join("inside.7z");
    let body = [victim, carrier].concat();
    fs::write(&archive, sevenz(&body, 1000, h.len())).unwrap();
    refused(
        &archive,
        &tmp.path().join("out"),
        "lies among its packed data",
    );
}

/// The same with an encoded header, whose packed data is the inside of a
/// block while the encoded header itself sits at the end.
#[test]
fn a_7z_whose_encoded_header_data_is_inside_a_block_is_refused() {
    let tmp = TempDir::new().unwrap();
    let victim = noise(1000, 1);
    let h = header(
        &[1000, 400],
        &[&[("victim.bin", 1000)], &[("carrier.bin", 400)]],
        &[],
    );
    let mut carrier = h.clone();
    carrier.resize(400, 0);
    // Encoded header, pack info: the raw header, stored where the carrier's
    // data begins. Unpack info: one Copy coder of the same size.
    let mut encoded = vec![0x17, 0x06];
    number(&mut encoded, 1000);
    encoded.extend([0x01, 0x09]);
    number(&mut encoded, h.len() as u64);
    encoded.extend([0x00, 0x07, 0x0B, 0x01, 0x00, 0x01, 0x01, 0x00, 0x0C]);
    number(&mut encoded, h.len() as u64);
    encoded.extend([0x00, 0x00]);
    let archive = tmp.path().join("encoded.7z");
    let body = [victim, carrier, encoded.clone()].concat();
    fs::write(&archive, sevenz(&body, 1400, encoded.len())).unwrap();
    refused(
        &archive,
        &tmp.path().join("out"),
        "lies among its packed data",
    );
}

/// Two files of 2^63 bytes each add up to zero in 64 bits, so the space
/// check would count nothing for them.
#[test]
fn a_7z_whose_sizes_overflow_is_refused() {
    let tmp = TempDir::new().unwrap();
    let half = 1u64 << 63;
    let h = header(&[16, 16], &[&[("a.bin", half)], &[("b.bin", half)]], &[]);
    let archive = tmp.path().join("huge.7z");
    fs::write(&archive, sevenz_with_header_last(&[&[1; 16], &[2; 16]], &h)).unwrap();
    refused(
        &archive,
        &tmp.path().join("out"),
        "more than any disk can hold",
    );
    let check = hatchzip(&["check", archive.to_str().unwrap()], &[]);
    assert_eq!(check.status.code(), Some(4), "reported as damaged");
}

/// The last block's coder takes no input, so it has no packed stream, and
/// its start would be looked up one past the last stream. sevenz-rust2
/// allows that for a block holding no files.
#[test]
fn a_7z_block_without_packed_streams_is_refused() {
    let tmp = TempDir::new().unwrap();
    let mut h = header(&[16], &[&[("a.bin", 16)], &[]], &[]);
    // The second block's one coder becomes Copy with zero inputs and one
    // output: after its coder count, flags with the complex bit, the method
    // id, then the stream counts.
    let at = h.windows(2).position(|w| w == [0x07, 0x0B]).unwrap() + 8;
    h.splice(at..at + 2, [0x11, 0x00, 0x00, 0x01]);
    let archive = tmp.path().join("noinput.7z");
    fs::write(&archive, sevenz_with_header_last(&[&[1; 16]], &h)).unwrap();
    refused(&archive, &tmp.path().join("out"), "header is inconsistent");
}

/// A coder this build has no decoder for is found while decoding, which is
/// too late: the block behind it was already extracted, committed, and its
/// bytes freed. Refused up front, the archive stays whole.
#[test]
fn a_7z_with_an_unsupported_method_is_refused_before_it_is_touched() {
    const LZS: &[u8] = &[0x04, 0xF7, 0x11, 0x05];
    let tmp = TempDir::new().unwrap();
    let (first, second) = (noise(16, 1), noise(16, 2));
    let h = header_with_methods(
        &[16, 16],
        &[&[("alpha.bin", 16)], &[("beta.bin", 16)]],
        &[],
        &[LZS, COPY],
    );
    let archive = tmp.path().join("lzs.7z");
    fs::write(&archive, sevenz_with_header_last(&[&first, &second], &h)).unwrap();

    // One unit per batch, so the Copy block would be committed on its own
    // before the LZS one is reached.
    let run = refused_env(
        &archive,
        &tmp.path().join("out"),
        "uses the LZS compression method",
        &[("HATCHZIP_BATCH_ENTRIES", "1")],
    );
    assert_eq!(run.status.code(), Some(1), "unsupported, not damaged");
}

/// A tar entry whose data runs past the end of the file, the last thing in
/// the archive, as a cut-off download leaves it.
fn tar_cut_off(kind: tar::EntryType) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(4);
    h.set_mode(0o644);
    b.append_data(&mut h, "first.txt", &b"data"[..]).unwrap();
    let mut bytes = b.into_inner().unwrap();
    bytes.truncate(1024);
    let mut last = tar::Header::new_gnu();
    last.set_entry_type(kind);
    last.set_path("last").unwrap();
    last.set_mode(0o755);
    last.set_size(1_000_000);
    last.set_cksum();
    bytes.extend(last.as_bytes());
    bytes
}

/// Its end lies past the end of the file, and nothing past the end can be
/// truncated or saved; a folder there was dropped and never made.
#[test]
fn a_tar_entry_past_the_end_of_the_file_is_refused() {
    for kind in [tar::EntryType::Directory, tar::EntryType::Regular] {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("cut.tar");
        fs::write(&archive, tar_cut_off(kind)).unwrap();
        refused(
            &archive,
            &tmp.path().join("out"),
            "past the end of the file",
        );
    }
}

/// Everything after the last entry is read into memory and kept there for
/// the whole run, so there is a limit to it, lowered here to 4 KB.
#[test]
fn too_much_after_the_last_entry_is_refused() {
    let tmp = TempDir::new().unwrap();
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(4);
    h.set_mode(0o644);
    b.append_data(&mut h, "first.txt", &b"data"[..]).unwrap();
    let tar = b.into_inner().unwrap();
    let limit = [("HATCHZIP_MAX_TAIL", "4096")];

    let fits = tmp.path().join("fits.tar");
    fs::write(&fits, &tar).unwrap();
    assert_ok(&hatchzip(&["check", fits.to_str().unwrap()], &limit));

    let archive = tmp.path().join("junk.tar");
    fs::write(&archive, [tar, noise(5000, 1)].concat()).unwrap();
    let before = fs::read(&archive).unwrap();
    let run = extract(&archive, &tmp.path().join("out"), &limit);
    assert!(!run.status.success());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("after its last entry"), "{stderr}");
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert_no_side_files(&archive);
}

/// An archive named so that only dots are left once the format is taken
/// off gets a folder of its own, not the folder above (`...zip`) or its own
/// folder (`..zip`).
#[test]
fn a_name_of_dots_never_extracts_outside_its_own_folder() {
    for name in ["...zip", "..zip"] {
        let tmp = TempDir::new().unwrap();
        let downloads = tmp.path().join("downloads");
        fs::create_dir(&downloads).unwrap();
        let files = Files::from([("escaped.txt".to_string(), b"x".to_vec())]);
        let archive = zip_of(&downloads, name, &files);
        let a = archive.to_str().unwrap();
        assert_ok(&hatchzip(&["extract", a, "--keep-free", "0", "--yes"], &[]));
        assert_eq!(
            fs::read(downloads.join(format!("{name}-extracted/escaped.txt"))).unwrap(),
            b"x",
            "{name}"
        );
        assert!(!tmp.path().join("escaped.txt").exists(), "{name}");
        assert!(!downloads.join("escaped.txt").exists(), "{name}");
    }
}

/// Entries can't land on the archive's own files, which the output folder
/// holds when it is the archive's folder: a volume would be lost before it
/// was read, and a progress file would be trusted by the next resume.
#[test]
fn entries_on_the_archives_own_files_are_refused() {
    for entry in ["a.zip.hatchzip", "A.ZIP.HATCHZIP-TAIL", "a.zip"] {
        let tmp = TempDir::new().unwrap();
        let files = Files::from([(entry.to_string(), b"planted".to_vec())]);
        let archive = zip_of(tmp.path(), "a.zip", &files);
        refused(&archive, tmp.path(), "one of the archive's own files");
    }
}
