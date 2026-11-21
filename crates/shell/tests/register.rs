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
    let places = register(&dirs, gui).unwrap();
    assert!(is_registered(&dirs));
    assert!(
        places
            .iter()
            .any(|p| p.starts_with("Thunar: ") && p.contains("thunar -q"))
    );

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
    assert!(thunar.contains("<patterns>*.zip;*.ZIP;*.7z;*.7Z;*.tar;*.TAR;"));
    assert!(thunar.contains(";*.XZ;*.zst;*.ZST;*.tzst;*.TZST;*.001"));
    assert_eq!(
        thunar.matches("*.001").count(),
        ACTIONS.len(),
        "no upper-case 001"
    );

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
    assert!(
        !uca.exists(),
        "the copy goes, so Thunar reads the defaults again"
    );
    assert!(!dirs.config.join("hatchzip/thunar-uca-created").exists());

    // Thunar rewrites the copy when it loads it, in its own layout and with
    // ids and empty elements of its own. Nothing the user did, so it goes too.
    register(&dirs, gui).unwrap();
    let text = std::fs::read_to_string(&uca).unwrap();
    let rewritten = text
        .replacen(
            "<!DOCTYPE actions [\n<!ELEMENT actions (action)+>\n]>\n",
            "",
            1,
        )
        .replacen(
            "  <action>\n    <name>Open Terminal Here</name>\n    \
             <unique-id>terminal</unique-id>\n  </action>",
            "<action>\n\t<icon></icon>\n\t<name>Open Terminal Here</name>\n\t\
             <submenu></submenu>\n\t<unique-id>1700000000-1</unique-id>\n</action>",
            1,
        );
    assert_ne!(rewritten, text);
    std::fs::write(&uca, &rewritten).unwrap();
    unregister(&dirs).unwrap();
    assert!(!uca.exists(), "Thunar's rewrite of an untouched copy goes");
    assert!(!dirs.config.join("hatchzip/thunar-uca-created").exists());

    // Once the user adds an action to the copy, it is theirs to keep.
    register(&dirs, gui).unwrap();
    let text = std::fs::read_to_string(&uca).unwrap();
    let added = text.replacen(
        "<actions>\n",
        "<actions>\n<action><unique-id>3-3</unique-id></action>\n",
        1,
    );
    std::fs::write(&uca, &added).unwrap();
    unregister(&dirs).unwrap();
    assert_eq!(
        std::fs::read_to_string(&uca).unwrap(),
        defaults.replacen(
            "<actions>\n",
            "<actions>\n<action><unique-id>3-3</unique-id></action>\n",
            1
        )
    );

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

    // Something that is not an actions file is never overwritten, and only
    // the Thunar menu is skipped.
    std::fs::write(&uca, "not xml").unwrap();
    std::fs::remove_dir_all(&dirs.data).unwrap();
    let places = register(&dirs, gui).unwrap();
    assert!(places.iter().any(|p| p.starts_with("Thunar: skipped, ")));
    assert_eq!(std::fs::read_to_string(&uca).unwrap(), "not xml");
    assert!(dirs.data.join("kio/servicemenus/hatchzip.desktop").exists());
}

/// A Thunar file HatchZip can't read or edit skips only Thunar, and says
/// which file it was.
#[cfg(target_os = "linux")]
#[test]
fn thunar_skips_what_it_cannot_edit() {
    use std::os::unix::fs::PermissionsExt;

    use hatchzip_shell::{ACTIONS, unregister};

    let tmp = tempfile::TempDir::new().unwrap();
    let system = tmp.path().join("xdg");
    std::fs::create_dir_all(system.join("Thunar")).unwrap();
    let dirs = Dirs {
        data: tmp.path().join("data"),
        config: tmp.path().join("config"),
        system_config: vec![system.clone()],
    };
    std::fs::create_dir_all(dirs.config.join("Thunar")).unwrap();
    let uca = dirs.config.join("Thunar/uca.xml");
    let gui = Path::new("/usr/bin/hatchzip-gui");
    let skipped = |places: Vec<String>| {
        places
            .into_iter()
            .find(|p| p.starts_with("Thunar: skipped, "))
            .unwrap_or_default()
    };

    // The user's file in Latin-1, which Thunar's parser refuses too.
    let latin1 = b"<actions><action><name>Caf\xe9</name></action></actions>\n";
    std::fs::write(&uca, latin1).unwrap();
    let line = skipped(register(&dirs, gui).unwrap());
    assert!(line.contains(&uca.display().to_string()), "{line}");
    assert_eq!(std::fs::read(&uca).unwrap(), latin1);
    assert!(dirs.data.join("kio/servicemenus/hatchzip.desktop").exists());
    // Unregister skips it too, and still removes the other menus.
    let line = skipped(unregister(&dirs).unwrap());
    assert!(line.contains(&uca.display().to_string()), "{line}");
    assert_eq!(std::fs::read(&uca).unwrap(), latin1);
    assert!(!dirs.data.join("kio/servicemenus/hatchzip.desktop").exists());
    std::fs::remove_file(&uca).unwrap();

    // A system file that can't be copied names the system file.
    let system_uca = system.join("Thunar/uca.xml");
    std::fs::write(&system_uca, "<actions><action></actions>").unwrap();
    let line = skipped(register(&dirs, gui).unwrap());
    assert!(line.contains(&system_uca.display().to_string()), "{line}");
    assert!(!uca.exists());
    unregister(&dirs).unwrap();

    // One that can't be read at all is reported, not passed over. Root reads
    // it anyway, so this only shows as a normal user.
    std::fs::write(&system_uca, "<actions/>").unwrap();
    std::fs::set_permissions(&system_uca, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&system_uca).is_err() {
        let line = skipped(register(&dirs, gui).unwrap());
        assert!(line.contains(&system_uca.display().to_string()), "{line}");
        assert!(!uca.exists());
    }
    std::fs::remove_file(&system_uca).unwrap();

    // Only XML's own five entities exist in Thunar's file.
    let nbsp = "<actions><action><name>A&nbsp;B</name></action></actions>\n";
    std::fs::write(&uca, nbsp).unwrap();
    assert!(!skipped(register(&dirs, gui).unwrap()).is_empty());
    assert_eq!(std::fs::read_to_string(&uca).unwrap(), nbsp);

    // A symlinked file stays a link, its target keeps its permissions, and
    // a Thunar-only registration still counts as registered.
    unregister(&dirs).unwrap();
    std::fs::remove_file(&uca).unwrap();
    let target = tmp.path().join("dotfiles/uca.xml");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "<actions/>\n").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&target, &uca).unwrap();
    assert!(skipped(register(&dirs, gui).unwrap()).is_empty());
    assert!(
        std::fs::symlink_metadata(&uca)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let text = std::fs::read_to_string(&target).unwrap();
    assert_eq!(text.matches("<action>").count(), ACTIONS.len());
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::remove_dir_all(&dirs.data).unwrap();
    assert!(is_registered(&dirs));
    unregister(&dirs).unwrap();
    assert!(!is_registered(&dirs));
    assert!(
        std::fs::symlink_metadata(&uca)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        !std::fs::read_to_string(&target)
            .unwrap()
            .contains("hatchzip-")
    );
}

/// Files that would trip a plain text search: a comment naming the end tag, an
/// unclosed action, a spaced empty root, and an empty file the user keeps on
/// purpose to hide the system's actions.
#[cfg(target_os = "linux")]
#[test]
fn thunar_reads_the_file_as_xml_not_text() {
    use hatchzip_shell::{ACTIONS, unregister};

    let tmp = tempfile::TempDir::new().unwrap();
    let system = tmp.path().join("xdg");
    std::fs::create_dir_all(system.join("Thunar")).unwrap();
    std::fs::write(
        system.join("Thunar/uca.xml"),
        "<actions><action><name>Default</name></action></actions>\n",
    )
    .unwrap();
    let dirs = Dirs {
        data: tmp.path().join("data"),
        config: tmp.path().join("config"),
        system_config: vec![system],
    };
    std::fs::create_dir_all(dirs.config.join("Thunar")).unwrap();
    let uca = dirs.config.join("Thunar/uca.xml");
    let gui = Path::new("/usr/bin/hatchzip-gui");

    // The real end tag is the one outside the comment.
    let commented = "<actions></actions><!-- Mentions </actions> here. -->\n";
    std::fs::write(&uca, commented).unwrap();
    register(&dirs, gui).unwrap();
    let text = std::fs::read_to_string(&uca).unwrap();
    assert!(text.ends_with("</actions><!-- Mentions </actions> here. -->\n"));
    assert_eq!(text.matches("<action>").count(), ACTIONS.len());
    unregister(&dirs).unwrap();
    let left = std::fs::read_to_string(&uca).unwrap();
    assert!(!left.contains("hatchzip-"));
    assert!(left.ends_with("</actions><!-- Mentions </actions> here. -->\n"));

    // A broken file is left alone, and only Thunar is skipped.
    let broken = "<actions><action><name>Incomplete</name></actions>";
    std::fs::write(&uca, broken).unwrap();
    let places = register(&dirs, gui).unwrap();
    assert!(places.iter().any(|p| p.starts_with("Thunar: skipped, ")));
    assert_eq!(std::fs::read_to_string(&uca).unwrap(), broken);

    // Both spellings of an empty root.
    for empty in ["<actions/>\n", "<actions />\n"] {
        std::fs::write(&uca, empty).unwrap();
        register(&dirs, gui).unwrap();
        let text = std::fs::read_to_string(&uca).unwrap();
        assert_eq!(text.matches("<action>").count(), ACTIONS.len(), "{empty}");
        unregister(&dirs).unwrap();
        assert!(uca.exists(), "{empty}");
    }

    // The user's own empty file is theirs, even when it matches HatchZip's.
    let own_empty = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n</actions>\n";
    std::fs::write(&uca, own_empty).unwrap();
    register(&dirs, gui).unwrap();
    unregister(&dirs).unwrap();
    assert_eq!(std::fs::read_to_string(&uca).unwrap(), own_empty);

    // A UTF-8 BOM at the start is kept and doesn't stop the edit.
    let bom = "\u{feff}<actions>\n</actions>\n";
    std::fs::write(&uca, bom).unwrap();
    register(&dirs, gui).unwrap();
    let text = std::fs::read_to_string(&uca).unwrap();
    assert!(text.starts_with('\u{feff}'));
    assert_eq!(text.matches("<action>").count(), ACTIONS.len());
    unregister(&dirs).unwrap();
    assert_eq!(std::fs::read_to_string(&uca).unwrap(), bom);

    // A user action is judged by its own id, not by what a comment says.
    let user = "<actions><action><unique-id>user-preview</unique-id>\
                <!-- <unique-id>hatchzip-test</unique-id> --></action></actions>\n";
    std::fs::write(&uca, user).unwrap();
    register(&dirs, gui).unwrap();
    assert!(
        std::fs::read_to_string(&uca)
            .unwrap()
            .contains("user-preview")
    );
    unregister(&dirs).unwrap();
    let left = std::fs::read_to_string(&uca).unwrap();
    assert!(left.contains("user-preview"));
    assert_eq!(left.matches("<action>").count(), 1);

    // An unescaped `&` is not XML, so the file is refused.
    let amp = "<actions><action><name>Preview & open</name></action></actions>\n";
    std::fs::write(&uca, amp).unwrap();
    let places = register(&dirs, gui).unwrap();
    assert!(places.iter().any(|p| p.starts_with("Thunar: skipped, ")));
    assert_eq!(std::fs::read_to_string(&uca).unwrap(), amp);
    let escaped = "<actions><action><name>Preview &amp; open &#38;</name></action></actions>\n";
    std::fs::write(&uca, escaped).unwrap();
    register(&dirs, gui).unwrap();
    unregister(&dirs).unwrap();
    assert!(
        std::fs::read_to_string(&uca)
            .unwrap()
            .contains("Preview &amp; open &#38;")
    );

    // A mark left after the user deleted HatchZip's file doesn't claim the next one.
    std::fs::remove_file(&uca).unwrap();
    register(&dirs, gui).unwrap();
    std::fs::remove_file(&uca).unwrap();
    unregister(&dirs).unwrap();
    std::fs::write(&uca, own_empty).unwrap();
    register(&dirs, gui).unwrap();
    unregister(&dirs).unwrap();
    assert_eq!(std::fs::read_to_string(&uca).unwrap(), own_empty);

    // The same when the user replaces the file without unregistering first.
    std::fs::remove_file(&uca).unwrap();
    register(&dirs, gui).unwrap();
    std::fs::write(&uca, own_empty).unwrap();
    register(&dirs, gui).unwrap();
    unregister(&dirs).unwrap();
    assert_eq!(std::fs::read_to_string(&uca).unwrap(), own_empty);
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
fn plist_value(file: &Path, key: &str) -> String {
    // plutil is Apple's own parser, so this also checks the escaping.
    let out = std::process::Command::new("/usr/bin/plutil")
        .args(["-extract", key, "raw", "-o", "-"])
        .arg(file)
        .output()
        .unwrap();
    assert!(out.status.success(), "{key} in {}", file.display());
    String::from_utf8(out.stdout)
        .unwrap()
        .trim_end()
        .to_string()
}

#[cfg(target_os = "macos")]
#[test]
fn finder_quick_actions_point_at_the_app_and_go_away_cleanly() {
    use hatchzip_shell::{ACTIONS, unregister};

    let tmp = tempfile::TempDir::new().unwrap();
    let dirs = Dirs {
        data: tmp.path().join("Library/Services"),
    };
    // Someone else's Quick Action, which must survive both directions.
    let foreign = dirs.data.join("Other.workflow/Contents/Info.plist");
    std::fs::create_dir_all(foreign.parent().unwrap()).unwrap();
    std::fs::write(&foreign, "not ours").unwrap();
    let gui = Path::new("/Applications/HatchZip.app/Contents/MacOS/hatchzip-gui");

    assert!(!is_registered(&dirs));
    register(&dirs, gui).unwrap();
    assert!(is_registered(&dirs));

    for a in ACTIONS {
        let contents = dirs
            .data
            .join(format!("hatchzip-{}.workflow/Contents", a.id));
        let info = contents.join("Info.plist");
        let wflow = contents.join("document.wflow");
        let lint = std::process::Command::new("/usr/bin/plutil")
            .arg("-lint")
            .args([&info, &wflow])
            .status()
            .unwrap();
        assert!(lint.success(), "{}", a.id);

        assert_eq!(
            plist_value(&info, "NSServices.0.NSMenuItem.default"),
            format!("HatchZip: {}", a.label)
        );
        assert_eq!(
            plist_value(&info, "NSServices.0.NSMessage"),
            "runWorkflowAsService"
        );
        assert_eq!(
            plist_value(
                &info,
                "NSServices.0.NSRequiredContext.NSApplicationIdentifier"
            ),
            "com.apple.finder"
        );
        assert_eq!(
            plist_value(&info, "NSServices.0.NSSendFileTypes.0"),
            "public.zip-archive"
        );
        let text = std::fs::read_to_string(&info).unwrap();
        for t in [
            "org.7-zip.7-zip-archive",
            "public.tar-archive",
            "org.gnu.gnu-zip-archive",
            "com.facebook.zstandard-archive",
            "io.github.xevrion.hatchzip.split-part",
        ] {
            assert!(text.contains(&format!("<string>{t}</string>")), "{t}");
        }
        assert_eq!(
            text.contains("com.rarlab.rar-archive"),
            cfg!(feature = "rar")
        );

        assert_eq!(
            plist_value(&wflow, "workflowMetaData.workflowTypeIdentifier"),
            "com.apple.Automator.servicesMenu"
        );
        assert_eq!(
            plist_value(&wflow, "actions.0.action.ActionBundlePath"),
            "/System/Library/Automator/Run Shell Script.action"
        );
        assert_eq!(
            plist_value(&wflow, "actions.0.action.ActionParameters.inputMethod"),
            "1"
        );
        assert_eq!(
            plist_value(&wflow, "actions.0.action.ActionParameters.shell"),
            "/bin/sh"
        );
        let script = plist_value(&wflow, "actions.0.action.ActionParameters.COMMAND_STRING");
        // By bundle id, so moving the app doesn't break the entries.
        assert!(
            script.ends_with(&format!(
                "\nexec /usr/bin/open -n -b io.github.xevrion.hatchzip --args {} \"$@\"",
                a.flag
            )),
            "{script}"
        );
    }

    // A bundle half written by an interrupted run is cleaned up.
    std::fs::create_dir_all(dirs.data.join(".hatchzip-test.tmp/Contents")).unwrap();
    register(&dirs, gui).unwrap();
    unregister(&dirs).unwrap();
    unregister(&dirs).unwrap();
    assert!(!is_registered(&dirs));
    let left: Vec<_> = std::fs::read_dir(&dirs.data)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, ["Other.workflow"]);
    assert_eq!(std::fs::read_to_string(&foreign).unwrap(), "not ours");
}

/// An app run from its disk image, or from the copy Gatekeeper makes of a
/// quarantined app that was never moved, is somewhere that goes away.
#[cfg(target_os = "macos")]
#[test]
fn finder_quick_actions_refuse_an_app_that_is_not_installed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dirs = Dirs {
        data: tmp.path().join("Services"),
    };
    for gui in [
        "/Volumes/HatchZip/HatchZip.app/Contents/MacOS/hatchzip-gui",
        "/private/var/folders/x/y/T/AppTranslocation/1234/d/HatchZip.app/Contents/MacOS/hatchzip-gui",
    ] {
        let e = register(&dirs, Path::new(gui)).unwrap_err();
        assert!(e.is::<hatchzip_shell::NotInstalled>(), "{gui}: {e:#}");
        assert!(!dirs.data.exists(), "{gui}");
    }
}

/// The script a Quick Action runs, run as Automator runs it, hands every
/// selected file to one GUI process with names intact. A GUI outside an app
/// bundle, as in a development build, is run directly.
#[cfg(target_os = "macos")]
#[test]
fn finder_quick_action_passes_every_selected_archive_at_once() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::TempDir::new().unwrap();
    let dirs = Dirs {
        data: tmp.path().join("Services"),
    };
    // Stands in for the GUI: writes its arguments, one per line.
    let bin = tmp.path().join("it's a \"dir\"");
    std::fs::create_dir_all(&bin).unwrap();
    let gui = bin.join("hatchzip-gui");
    let log = tmp.path().join("args.txt");
    std::fs::write(
        &gui,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&gui, std::fs::Permissions::from_mode(0o755)).unwrap();
    register(&dirs, &gui).unwrap();

    let wflow = dirs
        .data
        .join("hatchzip-extract-here-keep.workflow/Contents/document.wflow");
    let script = plist_value(&wflow, "actions.0.action.ActionParameters.COMMAND_STRING");
    let run = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            script.as_str(),
            "sh",
            "/Users/sam/My Games/a.zip",
            "/Users/sam/*.7z.001",
        ])
        .status()
        .unwrap();
    assert!(run.success());
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "--keep-archive\n--extract-here\n/Users/sam/My Games/a.zip\n/Users/sam/*.7z.001\n"
    );

    // Nothing selected: nothing runs.
    std::fs::remove_file(&log).unwrap();
    let run = std::process::Command::new("/bin/sh")
        .args(["-c", script.as_str(), "sh"])
        .status()
        .unwrap();
    assert!(run.success());
    assert!(!log.exists());
    hatchzip_shell::unregister(&dirs).unwrap();
}
