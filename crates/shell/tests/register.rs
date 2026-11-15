//! Registration writes exactly what each file manager reads, pointing at the
//! right program, and unregistering leaves nothing behind.

use std::path::Path;

use hatchzip_shell::{Dirs, is_registered, register};

#[cfg(target_os = "linux")]
#[test]
fn linux_menus_point_at_the_gui_and_go_away_cleanly() {
    use std::os::unix::fs::PermissionsExt;

    use hatchzip_shell::{ACTIONS, unregister};

    let tmp = tempfile::TempDir::new().unwrap();
    let dirs = Dirs {
        data: tmp.path().join("data"),
        config: tmp.path().join("config"),
        system_config: Vec::new(),
    };
    // A path with a space and a quote, the classic way to break Exec lines.
    let gui = Path::new("/opt/Hatch Zip/it's/hatchzip-gui");

    assert!(!is_registered(&dirs));
    register(&dirs, gui).unwrap();
    assert!(is_registered(&dirs));

    let kde = dirs.data.join("kio/servicemenus/hatchzip.desktop");
    let text = std::fs::read_to_string(&kde).unwrap();
    for a in ACTIONS {
        assert!(
            text.contains(&format!("Exec=\"{}\" {} %F", gui.display(), a.flag)),
            "{}",
            a.id
        );
    }
    assert!(text.contains("Actions=extract-files;extract-here;extract-here-keep;extract-to;test;"));
    assert!(text.contains(
        "Name=Extract here (keep archive)\nIcon=archive-extract\n\
         Exec=\"/opt/Hatch Zip/it's/hatchzip-gui\" --keep-archive --extract-here %F"
    ));
    assert_eq!(
        std::fs::metadata(&kde).unwrap().permissions().mode() & 0o111,
        0o111
    );

    let script = dirs.data.join("nautilus/scripts/HatchZip - Extract here");
    let text = std::fs::read_to_string(&script).unwrap();
    assert!(text.contains("'/opt/Hatch Zip/it'\\''s/hatchzip-gui' --extract-here"));
    assert!(std::fs::metadata(&script).unwrap().permissions().mode() & 0o100 != 0);
    let keep = dirs
        .data
        .join("nautilus/scripts/HatchZip - Extract here (keep archive)");
    assert!(
        std::fs::read_to_string(&keep)
            .unwrap()
            .contains("'/opt/Hatch Zip/it'\\''s/hatchzip-gui' --keep-archive --extract-here")
    );

    let nemo = dirs.data.join("nemo/actions/hatchzip-test.nemo_action");
    assert!(
        std::fs::read_to_string(&nemo)
            .unwrap()
            .contains("Extensions=zip;7z;tar;tgz;gz;tbz2;tbz;bz2;txz;xz;zst;tzst;001;")
    );
    let nemo = dirs
        .data
        .join("nemo/actions/hatchzip-extract-here-keep.nemo_action");
    let text = std::fs::read_to_string(&nemo).unwrap();
    assert!(text.contains("Name=Extract here (keep archive) with HatchZip"));
    assert!(text.contains("--keep-archive --extract-here %F\nSelection=notnone\n"));

    let thunar = std::fs::read_to_string(dirs.config.join("Thunar/uca.xml")).unwrap();
    assert_eq!(thunar.matches("<action>").count(), ACTIONS.len());
    assert!(thunar.contains(
        "<name>Extract here (keep archive)</name>\n    <submenu>HatchZip</submenu>\n    \
         <unique-id>hatchzip-extract-here-keep</unique-id>\n    \
         <command>'/opt/Hatch Zip/it'\\''s/hatchzip-gui' --keep-archive --extract-here %F</command>"
    ));
    assert!(thunar.contains("<patterns>*.zip;*.7z;*.tar;"));

    // Running it twice is harmless, and so is unregistering twice.
    register(&dirs, gui).unwrap();
    let again = std::fs::read_to_string(dirs.config.join("Thunar/uca.xml")).unwrap();
    assert_eq!(again, thunar, "a second register adds nothing");
    unregister(&dirs).unwrap();
    unregister(&dirs).unwrap();
    assert!(!is_registered(&dirs));
    let left: Vec<_> = walk(tmp.path());
    assert!(left.is_empty(), "left behind: {left:?}");
}

/// The GNOME Files script, run as GNOME Files runs it, hands every selected
/// file to one GUI process, names with spaces and stars intact.
#[cfg(target_os = "linux")]
#[test]
fn gnome_files_passes_every_selected_archive_at_once() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::TempDir::new().unwrap();
    let dirs = Dirs {
        data: tmp.path().join("data"),
        config: tmp.path().join("config"),
        system_config: Vec::new(),
    };
    // Stands in for the GUI: writes its arguments, one per line.
    let gui = tmp.path().join("fake gui");
    let log = tmp.path().join("args.txt");
    std::fs::write(
        &gui,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&gui, std::fs::Permissions::from_mode(0o755)).unwrap();
    register(&dirs, &gui).unwrap();

    let script = dirs
        .data
        .join("nautilus/scripts/HatchZip - Extract here (keep archive)");
    let run = std::process::Command::new(&script)
        .current_dir(tmp.path())
        .env(
            "NAUTILUS_SCRIPT_SELECTED_FILE_PATHS",
            "/home/sam/My Games/a.zip\n/home/sam/*.7z.001\n",
        )
        .status()
        .unwrap();
    assert!(run.success());
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "--keep-archive\n--extract-here\n/home/sam/My Games/a.zip\n/home/sam/*.7z.001\n"
    );

    // Nothing selected (a remote folder): nothing runs.
    std::fs::remove_file(&log).unwrap();
    let run = std::process::Command::new(&script)
        .env("NAUTILUS_SCRIPT_SELECTED_FILE_PATHS", "")
        .status()
        .unwrap();
    assert!(run.success());
    assert!(!log.exists());
}

/// Thunar reads only the user's `uca.xml` once there is one, so the system's
/// default actions are copied in, and the user's own actions survive both a
/// register and an unregister untouched.
#[cfg(target_os = "linux")]
#[test]
fn thunar_keeps_every_action_that_is_not_hatchzips() {
    use hatchzip_shell::{ACTIONS, unregister};

    let tmp = tempfile::TempDir::new().unwrap();
    let system = tmp.path().join("xdg");
    std::fs::create_dir_all(system.join("Thunar")).unwrap();
    let defaults = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE actions [\n\
                    <!ELEMENT actions (action)+>\n]>\n<actions>\n  <action>\n    \
                    <name>Open Terminal Here</name>\n    <unique-id>terminal</unique-id>\n  \
                    </action>\n</actions>\n";
    std::fs::write(system.join("Thunar/uca.xml"), defaults).unwrap();
    let dirs = Dirs {
        data: tmp.path().join("data"),
        config: tmp.path().join("config"),
        system_config: vec![tmp.path().join("missing"), system],
    };
    let uca = dirs.config.join("Thunar/uca.xml");
    let gui = Path::new("/usr/bin/hatchzip-gui");

    // No file of the user's yet: the defaults come along.
    register(&dirs, gui).unwrap();
    let text = std::fs::read_to_string(&uca).unwrap();
    assert!(text.contains("<name>Open Terminal Here</name>"));
    assert_eq!(text.matches("<action>").count(), 1 + ACTIONS.len());
    unregister(&dirs).unwrap();
    assert_eq!(std::fs::read_to_string(&uca).unwrap(), defaults);

    // The user's own file, as Thunar writes it, with an action of theirs on
    // either side of an older HatchZip entry.
    let mine = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n<action>\n\
                \t<name>Mine</name>\n\t<unique-id>1-1</unique-id>\n</action>\n<action>\n\
                \t<name>Old</name>\n\t<unique-id>hatchzip-test</unique-id>\n\t\
                <command>/old/hatchzip-gui --test %F</command>\n</action>\n<action>\n\
                \t<name>Also mine</name>\n\t<unique-id>2-2</unique-id>\n</action>\n</actions>\n";
    std::fs::write(&uca, mine).unwrap();
    register(&dirs, gui).unwrap();
    let text = std::fs::read_to_string(&uca).unwrap();
    assert!(!text.contains("/old/hatchzip-gui"));
    assert!(text.contains("<name>Mine</name>") && text.contains("<name>Also mine</name>"));
    assert!(!text.contains("Open Terminal Here"), "the user's file wins");
    assert_eq!(text.matches("<action>").count(), 2 + ACTIONS.len());
    unregister(&dirs).unwrap();
    let left = std::fs::read_to_string(&uca).unwrap();
    assert_eq!(
        left,
        mine.replace(
            "<action>\n\t<name>Old</name>\n\t<unique-id>hatchzip-test</unique-id>\n\t\
             <command>/old/hatchzip-gui --test %F</command>\n</action>\n",
            ""
        )
    );

    // Something that is not an actions file is never overwritten.
    std::fs::write(&uca, "not xml").unwrap();
    std::fs::remove_dir_all(&dirs.data).unwrap();
    assert!(register(&dirs, gui).is_err());
    assert_eq!(std::fs::read_to_string(&uca).unwrap(), "not xml");
    assert!(!dirs.data.exists(), "no other menu is written either");
}

#[cfg(target_os = "linux")]
fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            files.extend(walk(&p));
        } else {
            files.push(p);
        }
    }
    files
}

#[cfg(windows)]
#[test]
fn explorer_menu_points_at_the_gui_and_goes_away_cleanly() {
    use hatchzip_shell::{ACTIONS, unregister};
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let dirs = Dirs::current().unwrap();
    let gui = Path::new(r"C:\Program Files\Hatch Zip\hatchzip-gui.exe");
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let base = r"Software\Classes\SystemFileAssociations";

    register(&dirs, gui).unwrap();
    assert!(is_registered(&dirs));
    for ext in hatchzip_shell::EXTENSIONS {
        let menu = hkcu
            .open_subkey(format!(r"{base}\.{ext}\shell\HatchZip"))
            .unwrap();
        assert_eq!(menu.get_value::<String, _>("MUIVerb").unwrap(), "HatchZip");
        for (i, a) in ACTIONS.iter().enumerate() {
            let cmd = menu
                .open_subkey(format!(r"shell\{:02}{}\command", i + 1, a.id))
                .unwrap();
            let line: String = cmd.get_value("").unwrap();
            assert_eq!(line, format!("\"{}\" {} \"%1\"", gui.display(), a.flag));
        }
    }

    unregister(&dirs).unwrap();
    unregister(&dirs).unwrap();
    assert!(!is_registered(&dirs));
    for ext in hatchzip_shell::EXTENSIONS {
        assert!(
            hkcu.open_subkey(format!(r"{base}\.{ext}\shell\HatchZip"))
                .is_err()
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_says_it_is_not_supported_yet() {
    let dirs = Dirs::current().unwrap();
    assert!(register(&dirs, Path::new("/Applications/HatchZip.app")).is_err());
    assert!(!is_registered(&dirs));
}
