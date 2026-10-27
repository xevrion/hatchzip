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
        data: tmp.path().to_path_buf(),
    };
    // A path with a space and a quote, the classic way to break Exec lines.
    let gui = Path::new("/opt/Hatch Zip/it's/hatchzip-gui");

    assert!(!is_registered(&dirs));
    register(&dirs, gui).unwrap();
    assert!(is_registered(&dirs));

    let kde = tmp.path().join("kio/servicemenus/hatchzip.desktop");
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

    let script = tmp.path().join("nautilus/scripts/HatchZip - Extract here");
    let text = std::fs::read_to_string(&script).unwrap();
    assert!(text.contains("'/opt/Hatch Zip/it'\\''s/hatchzip-gui' --extract-here"));
    assert!(std::fs::metadata(&script).unwrap().permissions().mode() & 0o100 != 0);
    let keep = tmp
        .path()
        .join("nautilus/scripts/HatchZip - Extract here (keep archive)");
    assert!(
        std::fs::read_to_string(&keep)
            .unwrap()
            .contains("'/opt/Hatch Zip/it'\\''s/hatchzip-gui' --keep-archive --extract-here")
    );

    let nemo = tmp.path().join("nemo/actions/hatchzip-test.nemo_action");
    assert!(
        std::fs::read_to_string(&nemo)
            .unwrap()
            .contains("Extensions=zip;7z;tar;tgz;gz;001;")
    );
    let nemo = tmp
        .path()
        .join("nemo/actions/hatchzip-extract-here-keep.nemo_action");
    let text = std::fs::read_to_string(&nemo).unwrap();
    assert!(text.contains("Name=Extract here (keep archive) with HatchZip"));
    assert!(text.contains("--keep-archive --extract-here %F\nSelection=notnone\n"));

    // Running it twice is harmless, and so is unregistering twice.
    register(&dirs, gui).unwrap();
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
    // No DLL next to that exe, so no command handler either.
    let clsid = format!(
        r"Software\Classes\CLSID\{}",
        hatchzip_shell::explorer::clsid_string()
    );
    assert!(hkcu.open_subkey(&clsid).is_err());
    let extract_to = format!(r"{base}\.zip\shell\HatchZip\shell\04extract-to");
    let item = hkcu.open_subkey(&extract_to).unwrap();
    assert!(
        item.get_value::<String, _>("ExplorerCommandHandler")
            .is_err()
    );

    // With the DLL next to the exe, the "Extract to" entry, and only that
    // one, names the handler. Both files are empty: nothing loads them here.
    let tmp = tempfile::TempDir::new().unwrap();
    let installed = tmp.path().join("hatchzip-gui.exe");
    std::fs::write(&installed, b"").unwrap();
    let dll = tmp.path().join(hatchzip_shell::explorer::HANDLER_DLL);
    std::fs::write(&dll, b"").unwrap();
    register(&dirs, &installed).unwrap();
    let server = hkcu
        .open_subkey(format!(r"{clsid}\InprocServer32"))
        .unwrap();
    assert_eq!(
        server.get_value::<String, _>("").unwrap(),
        dll.to_string_lossy()
    );
    assert_eq!(
        server.get_value::<String, _>("ThreadingModel").unwrap(),
        "Apartment"
    );
    for ext in hatchzip_shell::EXTENSIONS {
        for (i, a) in ACTIONS.iter().enumerate() {
            let item = hkcu
                .open_subkey(format!(
                    r"{base}\.{ext}\shell\HatchZip\shell\{:02}{}",
                    i + 1,
                    a.id
                ))
                .unwrap();
            let handler = item.get_value::<String, _>("ExplorerCommandHandler");
            if a.id == "extract-to" {
                assert_eq!(handler.unwrap(), hatchzip_shell::explorer::clsid_string());
            } else {
                assert!(handler.is_err(), "{}", a.id);
            }
        }
    }

    // Registering again from where there is no DLL drops the handler.
    register(&dirs, gui).unwrap();
    assert!(hkcu.open_subkey(&clsid).is_err());
    register(&dirs, &installed).unwrap();

    unregister(&dirs).unwrap();
    unregister(&dirs).unwrap();
    assert!(!is_registered(&dirs));
    for ext in hatchzip_shell::EXTENSIONS {
        assert!(
            hkcu.open_subkey(format!(r"{base}\.{ext}\shell\HatchZip"))
                .is_err()
        );
    }
    assert!(hkcu.open_subkey(&clsid).is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_says_it_is_not_supported_yet() {
    let dirs = Dirs::current().unwrap();
    assert!(register(&dirs, Path::new("/Applications/HatchZip.app")).is_err());
    assert!(!is_registered(&dirs));
}
