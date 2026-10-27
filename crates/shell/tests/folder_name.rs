//! The Explorer menu works the folder name out by itself, without the core
//! crate. This checks it against what the GUI actually does: find the first
//! volume with `session::locate`, then name the folder with
//! `session::default_output`.

use hatchzip_core::session;
use hatchzip_shell::explorer::folder_name;

#[test]
fn the_menu_names_the_folder_the_gui_extracts_to() {
    let dir = tempfile::TempDir::new().unwrap();
    // `.r00` is a RAR volume only next to the set's first volume.
    std::fs::write(dir.path().join("set.rar"), b"").unwrap();
    std::fs::write(dir.path().join("set.r00"), b"").unwrap();

    for name in [
        "game.zip",
        "Game.ZIP",
        "game.7z",
        "game.tar",
        "game.tar.gz",
        "game.TAR.GZ",
        "game.tgz",
        "game.gz",
        "game.001",
        "game.7z.001",
        "game.7z.002",
        "game.zip.001",
        "game.tar.001",
        "game.7z.0001",
        "game.part1.rar",
        "game.part02.rar",
        "game.PART3.RAR",
        "game.part.rar",
        "game.rar",
        "set.rar",
        "set.r00",
        "set.r05",
        "a.b.c.zip",
        "game.001.zip",
        "My Game (v1.2) & more.7z",
        "\u{30b2}\u{30fc}\u{30e0}.zip",
        "notes.txt",
        ".zip",
        "zip",
    ] {
        let (primary, _) = session::locate(&dir.path().join(name)).unwrap();
        let gui = session::default_output(&primary);
        let gui = gui.file_name().unwrap().to_str().unwrap();
        assert_eq!(folder_name(name), gui, "{name}");
    }
}
