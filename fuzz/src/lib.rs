//! What every fuzz target shares. The parsers read archives from disk, so
//! each input is written to a file under the name the target gives it, then
//! taken through the same read-only path as `hatchzip test`: open, plan,
//! decode every unit, write nothing. Nothing here extracts, since extraction
//! deletes its input.
//!
//! An error is the expected answer to a damaged archive. Only a panic, a
//! crash, a hang or running out of memory is a finding.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use hatchzip_core::archive::storage::Reads;
use hatchzip_core::session::{self, State};

/// Longer inputs are skipped. Every parser is reached far below this, and a
/// compressed tar is decoded whole while it opens, before anything can look
/// at its sizes, so this is what bounds that work: deflate expands at most
/// about 1032 to 1.
pub const MAX_INPUT: usize = 256 * 1024;

/// Archives that declare more output than this are opened and planned but
/// not decoded. A few bytes can declare gigabytes, and decoding them would
/// only measure the decoder's speed until the fuzzer's timeout fires. Every
/// format's reader stops at the size its entry declares, so this bounds the
/// work.
const MAX_DECODED: u64 = 16 * 1024 * 1024;

/// One folder per process, so parallel fuzzing jobs never overwrite each
/// other's inputs.
fn folder() -> &'static PathBuf {
    static FOLDER: OnceLock<PathBuf> = OnceLock::new();
    FOLDER.get_or_init(|| {
        let folder = std::env::temp_dir().join(format!("hatchzip-fuzz-{}", std::process::id()));
        fs::create_dir_all(&folder).expect("creating the folder for fuzz inputs");
        folder
    })
}

/// Writes `data` to a file called `name` and checks it as `hatchzip test`
/// does, without a password. The name matters where the content alone does
/// not decide the format: a tar without the `ustar` magic, or a gzip stream
/// that only its name says holds a tar.
pub fn check(name: &str, data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    let path = folder().join(name);
    fs::write(&path, data).expect("writing the fuzz input");
    let Ok((primary, paths)) = session::locate(&path) else {
        return;
    };
    let reads = Arc::new(Reads::default());
    let Ok(State::Fresh(job, _)) = session::open(
        &primary,
        &paths,
        session::Output::Default,
        None,
        false,
        None,
        &reads,
    ) else {
        return;
    };
    // The space check replays every unit's sizes and offsets, which is
    // where arithmetic on values straight from the archive lives. It is
    // left out for a compressed tar, whose plan first probes whether the
    // folder can punch holes by writing a scratch file.
    if !job.is_stream() {
        let _ = job.space_needed(true, false);
    }
    if job.remaining_size() <= MAX_DECODED {
        let _ = job.verify(&|_: u64| {});
    }
}
