//! The progress files lie next to the archive, where whoever supplied the
//! archive could have put their own. Whatever a journal says, a resume may
//! only touch the archive's own volumes and the folder the person chose.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::*;
use tempfile::TempDir;

fn files() -> Files {
    let mut files = Files::new();
    files.insert("one.txt".into(), noise(20_000, 1));
    files.insert("sub/two.txt".into(), noise(20_000, 2));
    files.insert("three.txt".into(), noise(20_000, 3));
    files
}

fn crash() -> [(&'static str, &'static str); 2] {
    [
        ("HATCHZIP_BATCH_ENTRIES", "1"),
        ("HATCHZIP_CRASH_AT", "after-journal"),
    ]
}

/// Extracts into the default folder, next to the archive.
fn extract_here(archive: &Path, env: &[(&str, &str)]) -> Output {
    let a = archive.to_str().unwrap();
    hatchzip(&["extract", a, "--keep-free", "0", "--yes"], env)
}

/// A tar of `files()` in `dir`, crashed after its first batch.
fn half_extracted(dir: &Path) -> PathBuf {
    let archive = tar_of(dir, "game.tar", &files());
    assert!(!extract_here(&archive, &crash()).status.success());
    assert!(side_files(&archive)[0].exists());
    archive
}

fn edit_journal(archive: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = &side_files(archive)[0];
    let mut journal: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    edit(&mut journal);
    fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
}

fn assert_refused(run: &Output, says: &str) {
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(!run.status.success(), "should have been refused");
    assert!(stderr.contains(says), "{stderr}");
}

/// The journal of a crashed run resumes into the default folder with no
/// options, as before.
#[test]
fn an_ordinary_crash_resumes() {
    let tmp = TempDir::new().unwrap();
    let archive = half_extracted(tmp.path());
    assert_ok(&extract_here(&archive, &[]));
    let out = tmp.path().join("game");
    for (name, data) in files() {
        assert_eq!(fs::read(out.join(&name)).unwrap(), data, "{name}");
    }
    assert_consumed(&archive);
}

/// A journal that names some other file as the archive must not get it
/// truncated or deleted.
#[test]
fn a_journal_for_another_file_is_refused() {
    let tmp = TempDir::new().unwrap();
    let victim = tmp.path().join("victim.txt");
    fs::write(&victim, vec![b'v'; 100]).unwrap();
    let archive = tar_of(tmp.path(), "game.tar", &files());
    let before = fs::read(&archive).unwrap();
    let [journal, tail] = side_files(&archive);
    fs::write(&tail, b"").unwrap();
    fs::write(
        &journal,
        serde_json::json!({
            "version": 3, "format": "tar", "parts": [[victim, 100]], "tail_start": 100,
            "tail_crc": 0, "output": tmp.path().join("game"), "total_size": 0,
            "committed_len": 10, "links": []
        })
        .to_string(),
    )
    .unwrap();

    for run in [extract_here(&archive, &[]), restore(&archive, None)] {
        assert_refused(&run, "does not belong to");
    }
    assert_eq!(fs::read(&victim).unwrap(), vec![b'v'; 100]);
    assert_eq!(fs::read(&archive).unwrap(), before);
}

/// A journal that sends the output to a folder this computer has no record
/// of is not followed, even when the person names that folder.
#[test]
fn a_journal_output_elsewhere_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = half_extracted(tmp.path());
    let before = fs::read(&archive).unwrap();
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    edit_journal(&archive, |j| j["output"] = serde_json::json!(elsewhere));

    assert_refused(&extract_here(&archive, &[]), "no record of starting it");
    assert_refused(&restore(&archive, None), "no record of starting it");
    assert_refused(
        &extract(&archive, &elsewhere, &[]),
        "no record of starting it",
    );
    assert_refused(
        &restore(&archive, Some(elsewhere.as_path())),
        "no record of starting it",
    );
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    assert_eq!(fs::read(&archive).unwrap(), before);
    // Looking changes nothing, so it needs no folder.
    assert_ok(&hatchzip(&["check", archive.to_str().unwrap()], &[]));
}

/// Makes the journal look like one a release before 0.1.6 wrote: those had
/// no folder list, and recorded no default folder.
fn as_before_records(archive: &Path) {
    edit_journal(archive, |j| {
        let j = j.as_object_mut().unwrap();
        j.remove("folders");
        j.remove("folders_left");
        j.insert("version".into(), serde_json::json!(3));
    });
}

/// A planted journal can claim an old format just as easily, so one without
/// a record is refused whatever its format, the folder named or not, and
/// the refusal offers no command that would follow it.
#[test]
fn a_journal_in_an_old_format_without_its_record_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = half_extracted(tmp.path());
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    edit_journal(&archive, |j| j["output"] = serde_json::json!(elsewhere));
    as_before_records(&archive);
    let before = fs::read(&archive).unwrap();

    let run = extract_here(&archive, &[]);
    assert_refused(&run, "no record of starting it");
    assert_refused(&run, "If you did not start this extraction, delete");
    assert!(!String::from_utf8_lossy(&run.stderr).contains(" -o "));
    assert_refused(
        &extract(&archive, &elsewhere, &[]),
        "no record of starting it",
    );
    assert_refused(
        &restore(&archive, Some(elsewhere.as_path())),
        "no record of starting it",
    );
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert!(side_files(&archive)[0].exists());
}

/// Where a run notes the folder of the extraction it starts.
fn record(archive: &Path) -> PathBuf {
    let hash = crc32fast::hash(archive.as_os_str().as_encoded_bytes());
    state_dir().join(format!("{hash:08x}.json"))
}

/// A journal this computer has no record of, such as one that came with
/// the archive, is not used even for the default folder, named or not. A
/// resume deletes what it finds at the paths of the entries left, and there
/// those may be the person's own files.
#[test]
fn a_journal_for_the_default_folder_without_its_record_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = tar_of(tmp.path(), "game.tar", &files());
    // Crashed under another profile, as on another computer or by a
    // release that did not record the default folder.
    let elsewhere = tmp.path().join("state");
    let env = [
        crash().as_slice(),
        &[("HATCHZIP_STATE_DIR", elsewhere.to_str().unwrap())],
    ]
    .concat();
    assert!(!extract_here(&archive, &env).status.success());
    assert!(!record(&archive).exists());
    // Extraction runs from the end, so `one.txt` is not extracted yet.
    let out = tmp.path().join("game");
    assert!(!out.join("one.txt").exists());
    fs::write(out.join("one.txt"), b"a file of the person's").unwrap();
    let before = fs::read(&archive).unwrap();

    let run = extract_here(&archive, &[]);
    assert_refused(&run, "no record of starting it");
    assert_refused(&run, "Nothing was modified");
    assert_refused(&run, "If you did not start this extraction, delete");
    assert_refused(&restore(&archive, None), "no record of starting it");
    assert_refused(&extract(&archive, &out, &[]), "no record of starting it");
    assert_refused(
        &restore(&archive, Some(out.as_path())),
        "no record of starting it",
    );
    assert_eq!(
        fs::read(out.join("one.txt")).unwrap(),
        b"a file of the person's"
    );
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert!(side_files(&archive)[0].exists());
}

/// Restore removes the record with the progress files, so it can't vouch
/// for a journal that turns up next to an archive at that path later.
#[test]
fn restore_removes_the_record() {
    let tmp = TempDir::new().unwrap();
    let archive = half_extracted(tmp.path());
    assert!(record(&archive).exists());
    assert_ok(&restore(&archive, None));
    assert!(!record(&archive).exists());
}

/// A run the person started with `-o` resumes without it on the same
/// computer, as the app resumes one, and the record goes when the run ends.
#[test]
fn a_chosen_output_resumes_without_naming_it() {
    let tmp = TempDir::new().unwrap();
    let archive = tar_of(tmp.path(), "game.tar", &files());
    let out = tmp.path().join("chosen");
    assert!(!extract(&archive, &out, &crash()).status.success());
    let record = record(&archive);
    assert!(record.exists());

    assert_ok(&extract_here(&archive, &[]));
    assert_tree_without_dir(&out);
    assert_consumed(&archive);
    assert!(!record.exists(), "the record should be gone");
}

/// Elsewhere, such as on another computer, the journal is not followed,
/// and the refusal does not hand out a command that would follow it.
#[test]
fn a_chosen_output_without_its_record_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = tar_of(tmp.path(), "game.tar", &files());
    let out = tmp.path().join("chosen");
    let elsewhere = tmp.path().join("state");
    let crashed = hatchzip(
        &[
            "extract",
            archive.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--yes",
        ],
        &[
            crash().as_slice(),
            &[("HATCHZIP_STATE_DIR", elsewhere.to_str().unwrap())],
        ]
        .concat(),
    );
    assert!(!crashed.status.success());

    let run = extract_here(&archive, &[]);
    assert_refused(&run, "no record of starting it");
    assert!(!String::from_utf8_lossy(&run.stderr).contains(" -o "));
    assert_refused(&extract(&archive, &out, &[]), "no record of starting it");
    assert!(side_files(&archive)[0].exists());
}

/// A resume cuts what the last run committed off the archive only once the
/// journal, its saved index and the folder all check out. Here the saved
/// index matches its journal but is not the archive's, so the archive can't
/// be read, and it must stay exactly as it was.
#[test]
fn a_resume_that_fails_to_open_leaves_the_archive_as_it_was() {
    let tmp = TempDir::new().unwrap();
    let archive = zip_of(tmp.path(), "game.zip", &files());
    // Stopped after committing a batch, before cutting it off.
    assert!(!extract_here(&archive, &crash()).status.success());
    let [journal, tail] = side_files(&archive);
    let j: serde_json::Value = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    let committed = j["committed_len"].as_u64().unwrap();
    let before = fs::read(&archive).unwrap();
    assert!(before.len() as u64 > committed, "a cut should be pending");

    let zeros = vec![0u8; fs::metadata(&tail).unwrap().len() as usize];
    fs::write(&tail, &zeros).unwrap();
    edit_journal(&archive, |j| {
        j["tail_crc"] = serde_json::json!(crc32fast::hash(&zeros));
    });
    assert!(!extract_here(&archive, &[]).status.success());
    assert_eq!(fs::read(&archive).unwrap(), before);
}

/// A volume that is a link would have the file it points to cut off.
#[cfg(unix)]
#[test]
fn a_linked_volume_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = tar_of(tmp.path(), "game.tar", &files());
    let first = split_up(&archive, 2);
    let second = first.with_extension("002");
    let victim = tmp.path().join("victim.bin");
    fs::rename(&second, &victim).unwrap();
    std::os::unix::fs::symlink(&victim, &second).unwrap();
    let before = fs::read(&victim).unwrap();

    assert_refused(&extract_here(&first, &[]), "is a link");
    assert_eq!(fs::read(&victim).unwrap(), before);
}

fn assert_tree_without_dir(out: &Path) {
    for (name, data) in files() {
        assert_eq!(fs::read(out.join(&name)).unwrap(), data, "{name}");
    }
}

#[test]
fn a_journal_link_outside_the_output_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = half_extracted(tmp.path());
    let outside = tmp.path().join("outside");
    for (path, target, hard) in [
        (outside.to_str().unwrap(), "one.txt", false),
        ("../escaped", "one.txt", true),
        ("fine", outside.to_str().unwrap(), true),
    ] {
        let original = fs::read(&side_files(&archive)[0]).unwrap();
        edit_journal(&archive, |j| {
            j["links"] = serde_json::json!([{"path": path, "target": target, "hard": hard}]);
        });
        assert_refused(
            &extract_here(&archive, &[]),
            "a link outside the output folder",
        );
        fs::write(&side_files(&archive)[0], original).unwrap();
    }
    assert!(!outside.exists());
    assert!(!tmp.path().join("escaped").exists());
}

/// A link someone else put in the output folder is not followed by the
/// files a resume deletes and writes again.
#[cfg(unix)]
#[test]
fn a_link_on_the_way_in_the_output_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = half_extracted(tmp.path());
    let out = tmp.path().join("game");
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("two.txt"), b"precious").unwrap();
    let _ = fs::remove_dir_all(out.join("sub"));
    std::os::unix::fs::symlink(&outside, out.join("sub")).unwrap();

    assert_refused(
        &extract_here(&archive, &[]),
        "would be extracted through it",
    );
    assert_eq!(fs::read(outside.join("two.txt")).unwrap(), b"precious");
}

/// A link where the default output folder goes would send every file
/// wherever it points.
#[cfg(unix)]
#[test]
fn a_link_at_the_default_output_is_refused() {
    let tmp = TempDir::new().unwrap();
    let archive = tar_of(tmp.path(), "game.tar", &files());
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, tmp.path().join("game")).unwrap();

    assert_refused(&extract_here(&archive, &[]), "is a link");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    assert!(archive.exists());
}

fn restore(archive: &Path, out: Option<&Path>) -> Output {
    let mut args = vec!["restore", archive.to_str().unwrap()];
    if let Some(o) = out {
        args.extend(["-o", o.to_str().unwrap()]);
    }
    hatchzip(&args, &[])
}
