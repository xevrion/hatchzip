//! `--replace`: extracting over files already in the output folder. They
//! are moved aside before anything is extracted, deleted once the run is
//! complete, and put back by `restore`. Without the option nothing changes.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::*;
use hatchzip_core::session;
use tempfile::TempDir;

const ASIDE: &str = ".hatchzip-replaced";

fn files() -> Files {
    let mut files = Files::new();
    files.insert("game/data/a.pak".into(), noise(20_000, 1));
    files.insert("game/data/b.pak".into(), noise(30_000, 2));
    for i in 0..6 {
        files.insert(format!("game/many/f{i}"), format!("file {i}\n").into());
    }
    // Last in the archive, so the first one a run extracts and commits.
    files.insert("game/readme.txt".into(), b"new readme\n".to_vec());
    files
}

/// What the output folder held before the run: three files the archive
/// replaces and one it doesn't touch.
fn old() -> Files {
    let mut old = Files::new();
    old.insert("game/data/a.pak".into(), noise(5_000, 10));
    old.insert("game/many/f3".into(), b"old f3\n".to_vec());
    old.insert("game/readme.txt".into(), b"old readme\n".to_vec());
    old.insert("mine.txt".into(), b"not in the archive\n".to_vec());
    old
}

fn put(out: &Path, files: &Files) {
    for (name, data) in files {
        let p = out.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }
}

fn setup() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let archive = zip_of(tmp.path(), "patch.zip", &files());
    let out = tmp.path().join("out");
    put(&out, &old());
    (tmp, archive, out)
}

fn run(args: &[&str], archive: &Path, out: &Path, env: &[(&str, &str)]) -> Output {
    let (a, o) = (archive.to_str().unwrap(), out.to_str().unwrap());
    let mut all = vec![args[0], a, "-o", o];
    all.extend_from_slice(&args[1..]);
    hatchzip(&all, env)
}

fn replace(archive: &Path, out: &Path, env: &[(&str, &str)]) -> Output {
    run(
        &["extract", "--replace", "--keep-free", "0", "--yes"],
        archive,
        out,
        env,
    )
}

fn stderr(run: &Output) -> String {
    String::from_utf8_lossy(&run.stderr).replace('\\', "/")
}

/// The archive's files, and the person's file it doesn't name.
fn expected() -> Files {
    let mut all = files();
    all.insert("mine.txt".into(), old()["mine.txt"].clone());
    all
}

fn assert_refused_untouched(run: &Output, archive: &Path, before: &[u8], out: &Path, says: &str) {
    assert!(!run.status.success(), "should be refused");
    assert!(stderr(run).contains(says), "{}", stderr(run));
    assert_eq!(fs::read(archive).unwrap(), before);
    assert_no_side_files(archive);
    assert!(!out.join(ASIDE).exists());
}

#[test]
fn replaces_existing_files_and_deletes_them_when_complete() {
    let (_tmp, archive, out) = setup();
    let run = replace(&archive, &out, &[]);
    assert_ok(&run);
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("3 existing files will be replaced"),
        "{}",
        String::from_utf8_lossy(&run.stdout)
    );
    assert_tree(&out, &expected());
    assert!(!out.join(ASIDE).exists());
    assert_consumed(&archive);
}

#[test]
fn without_replace_nothing_is_modified() {
    let (_tmp, archive, out) = setup();
    let before = fs::read(&archive).unwrap();
    let run = extract(&archive, &out, &[]);
    assert_refused_untouched(
        &run,
        &archive,
        &before,
        &out,
        "and 2 other files already exist. Use --replace to replace existing files, or choose \
         another destination",
    );
    for (name, data) in old() {
        assert_eq!(fs::read(out.join(&name)).unwrap(), data, "{name}");
    }
    assert!(!out.join("game/data/b.pak").exists());

    // `check` says so, and changes nothing either.
    let a = archive.to_str().unwrap();
    let o = out.to_str().unwrap();
    let check = hatchzip(&["check", a, "-o", o, "--keep-free", "0"], &[]);
    assert_ok(&check);
    let said = String::from_utf8_lossy(&check.stdout);
    assert!(
        said.contains("3 files already exist in the destination"),
        "{said}"
    );
    assert_eq!(fs::read(&archive).unwrap(), before);
}

fn skip(archive: &Path, out: &Path, env: &[(&str, &str)]) -> Output {
    run(
        &["extract", "--skip-existing", "--keep-free", "0", "--yes"],
        archive,
        out,
        env,
    )
}

/// The archive's files where nothing was in the way, and the person's own
/// files everywhere else.
fn expected_skipping() -> Files {
    let mut all = files();
    all.extend(old());
    all
}

#[test]
fn skipping_keeps_existing_files_and_extracts_the_rest() {
    let (_tmp, archive, out) = setup();
    let run = skip(&archive, &out, &[]);
    assert_ok(&run);
    assert!(
        String::from_utf8_lossy(&run.stdout)
            .contains("3 existing files will be kept instead of the archive's copies"),
        "{}",
        String::from_utf8_lossy(&run.stdout)
    );
    assert_tree(&out, &expected_skipping());
    assert!(!out.join(ASIDE).exists());
    assert_consumed(&archive);
}

/// A crash anywhere, including while the kept files go back, never loses
/// one: a resume finishes putting them back.
#[test]
fn a_crashed_skipping_run_resumes() {
    let points = [
        ("set-aside", 1),
        ("after-journal", 2),
        ("settle", 0),
        ("settle", 1),
    ];
    for (point, after) in points {
        let (_tmp, archive, out) = setup();
        let after_s = after.to_string();
        let crashed = skip(
            &archive,
            &out,
            &[
                ("HATCHZIP_BATCH_ENTRIES", "1"),
                ("HATCHZIP_CRASH_AT", point),
                ("HATCHZIP_CRASH_AFTER", &after_s),
            ],
        );
        assert!(!crashed.status.success(), "{point}#{after} should crash");
        // The journal remembers the choice; the option is not needed again.
        assert_ok(&extract(&archive, &out, &[]));
        assert_tree(&out, &expected_skipping());
        assert!(!out.join(ASIDE).exists(), "{point}#{after}");
        assert_no_side_files(&archive);
    }
}

#[test]
fn a_crashed_run_resumes() {
    let points = [
        ("set-aside", 0),
        ("set-aside", 2),
        ("after-extract", 0),
        ("after-extract", 3),
        ("after-journal", 0),
        ("after-journal", 3),
    ];
    for (point, after) in points {
        let (_tmp, archive, out) = setup();
        let after_s = after.to_string();
        let crashed = replace(
            &archive,
            &out,
            &[
                ("HATCHZIP_BATCH_ENTRIES", "1"),
                ("HATCHZIP_CRASH_AT", point),
                ("HATCHZIP_CRASH_AFTER", &after_s),
            ],
        );
        assert!(!crashed.status.success(), "{point}#{after} should crash");
        assert!(out.join(ASIDE).is_dir(), "{point}#{after}");

        // The journal decides; the option is not needed again.
        let resumed = if after == 0 {
            replace(&archive, &out, &[])
        } else {
            extract(&archive, &out, &[])
        };
        assert_ok(&resumed);
        assert_tree(&out, &expected());
        assert!(!out.join(ASIDE).exists(), "{point}#{after}");
        assert_consumed(&archive);
    }
}

fn restore(archive: &Path, out: &Path) -> Output {
    run(&["restore"], archive, out, &[])
}

/// Every path the run had not committed gets its file back, byte for byte,
/// and the archive holds what is not extracted.
#[test]
fn restore_puts_the_replaced_files_back() {
    for (point, after) in [("set-aside", 1), ("after-extract", 0)] {
        let (_tmp, archive, out) = setup();
        let before = fs::read(&archive).unwrap();
        let crashed = replace(
            &archive,
            &out,
            &[
                ("HATCHZIP_BATCH_ENTRIES", "1"),
                ("HATCHZIP_CRASH_AT", point),
                ("HATCHZIP_CRASH_AFTER", &after.to_string()),
            ],
        );
        assert!(!crashed.status.success(), "{point}#{after} should crash");

        assert_ok(&restore(&archive, &out));
        for (name, data) in old() {
            assert_eq!(fs::read(out.join(&name)).unwrap(), data, "{point}: {name}");
        }
        assert!(!out.join(ASIDE).exists(), "{point}");
        assert!(!out.join("game/data/b.pak").exists(), "{point}");
        // Nothing was committed, so nothing was cut off.
        assert_eq!(fs::read(&archive).unwrap(), before, "{point}");
        assert_no_side_files(&archive);
    }
}

/// A file extracted in place is the only copy of its entry once committed,
/// so the file it replaced stays aside and restore says where.
#[test]
fn restore_keeps_aside_what_a_committed_file_replaced() {
    let (_tmp, archive, out) = setup();
    let crashed = replace(
        &archive,
        &out,
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
        ],
    );
    assert!(!crashed.status.success());

    let restored = restore(&archive, &out);
    assert_ok(&restored);
    let said = String::from_utf8_lossy(&restored.stdout).replace('\\', "/");
    assert!(
        said.contains(
            &format!("1 replaced file remains in {}", out.join(ASIDE).display()).replace('\\', "/")
        ),
        "{said}"
    );
    assert_eq!(
        fs::read(out.join("game/readme.txt")).unwrap(),
        files()["game/readme.txt"]
    );
    assert_eq!(
        fs::read(out.join(ASIDE).join("game/readme.txt")).unwrap(),
        old()["game/readme.txt"]
    );
    for name in ["game/data/a.pak", "game/many/f3", "mine.txt"] {
        assert_eq!(fs::read(out.join(name)).unwrap(), old()[name], "{name}");
    }
    assert_no_side_files(&archive);

    // The folder left behind is not taken over by the next run.
    let again = replace(&archive, &out, &[]);
    assert!(!again.status.success());
    assert!(stderr(&again).contains("already exists, so existing files cannot be moved there"));
}

/// A kept archive still holds every entry, so restore puts every replaced
/// file back, over what was extracted in its place.
#[test]
fn restoring_a_kept_archive_puts_every_file_back() {
    let (_tmp, archive, out) = setup();
    let before = fs::read(&archive).unwrap();
    let crashed = run(
        &["extract", "--replace", "--keep-archive", "--keep-free", "0"],
        &archive,
        &out,
        &[
            ("HATCHZIP_BATCH_ENTRIES", "1"),
            ("HATCHZIP_CRASH_AT", "after-journal"),
            ("HATCHZIP_CRASH_AFTER", "2"),
        ],
    );
    assert!(!crashed.status.success());

    assert_ok(&restore(&archive, &out));
    for (name, data) in old() {
        assert_eq!(fs::read(out.join(&name)).unwrap(), data, "{name}");
    }
    assert!(!out.join(ASIDE).exists());
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert_no_side_files(&archive);
}

#[test]
fn a_folder_and_a_file_never_replace_each_other() {
    let (_tmp, archive, out) = setup();
    let before = fs::read(&archive).unwrap();
    fs::create_dir_all(out.join("game/data/b.pak")).unwrap();
    let run = replace(&archive, &out, &[]);
    assert_refused_untouched(
        &run,
        &archive,
        &before,
        &out,
        "is a folder, but the archive has a file",
    );
    assert!(out.join("game/data/b.pak").is_dir());

    let (_tmp, archive, out) = setup();
    let before = fs::read(&archive).unwrap();
    fs::remove_dir_all(out.join("game/many")).unwrap();
    fs::write(out.join("game/many"), "a file, not a folder").unwrap();
    let run = replace(&archive, &out, &[]);
    assert_refused_untouched(&run, &archive, &before, &out, "requires a folder");
    assert_eq!(
        fs::read(out.join("game/many")).unwrap(),
        b"a file, not a folder"
    );
    assert_eq!(
        fs::read(out.join("game/readme.txt")).unwrap(),
        old()["game/readme.txt"]
    );
}

/// The folder of replaced files takes after the output, which can ignore case
/// where the folder they are in now doesn't. So two names it would make one
/// are refused before the first moves, while the second isn't left to fail
/// halfway. Pairs meet below `sub`, for `sub` to keep them apart: only the
/// output is told to ignore case.
#[test]
fn names_the_folder_of_replaced_files_would_make_one_are_refused() {
    let tmp = TempDir::new().unwrap();
    if !session::case_sensitive(tmp.path()) {
        return;
    }
    for (first, second) in [
        ("sub/Makefile", "sub/makefile"),
        // One is a file where the other needs a folder.
        ("sub/Game", "sub/game/inner.txt"),
        // Only folders differ, so a restore would put one file in the wrong one.
        ("sub/Game/a.txt", "sub/game/b.txt"),
    ] {
        eprintln!("{first} and {second}");
        let tmp = TempDir::new().unwrap();
        let files = Files::from([
            (first.to_string(), b"new first\n".to_vec()),
            (second.to_string(), b"new second\n".to_vec()),
        ]);
        let archive = zip_of(tmp.path(), "case.zip", &files);
        let out = tmp.path().join("out");
        let old = Files::from([
            (first.to_string(), b"old first\n".to_vec()),
            (second.to_string(), b"old second\n".to_vec()),
        ]);
        put(&out, &old);
        let before = fs::read(&archive).unwrap();

        let at = [("HATCHZIP_IGNORE_CASE_AT", out.to_str().unwrap())];
        let run = replace(&archive, &out, &at);
        assert_refused_untouched(&run, &archive, &before, &out, "differ only in case");
        for (name, data) in &old {
            assert_eq!(&fs::read(out.join(name)).unwrap(), data, "{name}");
        }

        assert_ok(&replace(&archive, &out, &[]));
        assert_tree(&out, &files);
    }
}

/// With one of the two in the output, there is only one name to move aside.
#[test]
fn one_name_of_a_case_pair_can_be_replaced_where_the_output_ignores_case() {
    let tmp = TempDir::new().unwrap();
    if !session::case_sensitive(tmp.path()) {
        return;
    }
    let files = Files::from([
        ("sub/Makefile".to_string(), b"upper\n".to_vec()),
        ("sub/makefile".to_string(), b"lower\n".to_vec()),
    ]);
    let archive = zip_of(tmp.path(), "case.zip", &files);
    let out = tmp.path().join("out");
    put(
        &out,
        &Files::from([("sub/Makefile".to_string(), b"old\n".to_vec())]),
    );
    let at = [("HATCHZIP_IGNORE_CASE_AT", out.to_str().unwrap())];
    assert_ok(&replace(&archive, &out, &at));
    assert_tree(&out, &files);
}

#[test]
fn an_archive_naming_the_folder_of_replaced_files_is_refused() {
    for name in [".hatchzip-replaced/x.txt", ".HatchZip-Replaced/x.txt"] {
        let tmp = TempDir::new().unwrap();
        let mut files = files();
        files.insert(name.into(), b"inside".to_vec());
        let archive = zip_of(tmp.path(), "patch.zip", &files);
        let before = fs::read(&archive).unwrap();
        let out = tmp.path().join("out");
        put(&out, &old());
        let run = replace(&archive, &out, &[]);
        assert_refused_untouched(
            &run,
            &archive,
            &before,
            &out,
            "uses the name .hatchzip-replaced",
        );
        assert_eq!(
            fs::read(out.join("game/readme.txt")).unwrap(),
            old()["game/readme.txt"]
        );
    }
}

/// A link at a replaced path is moved like a file; what it points to is
/// never read or written. A link on the way to one is refused.
#[cfg(unix)]
#[test]
fn links_are_moved_not_followed() {
    use std::os::unix::fs::symlink;

    let (tmp, archive, out) = setup();
    let outside = tmp.path().join("outside.pak");
    fs::write(&outside, "outside the output").unwrap();
    symlink(&outside, out.join("game/data/b.pak")).unwrap();
    assert_ok(&replace(&archive, &out, &[]));
    assert_tree(&out, &expected());
    assert!(!out.join(ASIDE).exists());
    assert!(
        !fs::symlink_metadata(out.join("game/data/b.pak"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(fs::read(&outside).unwrap(), b"outside the output");

    let (tmp, archive, out) = setup();
    let before = fs::read(&archive).unwrap();
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("f3"), "elsewhere").unwrap();
    fs::remove_dir_all(out.join("game/many")).unwrap();
    symlink(&elsewhere, out.join("game/many")).unwrap();
    let run = replace(&archive, &out, &[]);
    assert_refused_untouched(&run, &archive, &before, &out, "is a link");
    assert_eq!(fs::read(elsewhere.join("f3")).unwrap(), b"elsewhere");
    assert_eq!(
        fs::read(out.join("game/readme.txt")).unwrap(),
        old()["game/readme.txt"]
    );
}
