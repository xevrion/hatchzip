//! The Settings window, driven through the callbacks its controls call:
//! opened from the menu and the gear, every control changed, each change
//! saved and mirrored in the main window, Reset, and one window however
//! often it is opened.
use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use hatchzip_gui::args::{TaskArgs, TaskKind};
use hatchzip_gui::backend::Drops;
use hatchzip_gui::settings::{AfterExtracting, Appearance, Destination, Existing, Settings};
use hatchzip_gui::{MainWindow, Palette, Screen, SettingsWindow, main_window, task_window};
use i_slint_backend_testing::{ElementHandle, TestingBackend, TestingBackendOptions};
use slint::language::ColorScheme;
use slint::{ComponentHandle, Model, Timer, TimerMode};

fn wait_until(what: &str, condition: impl Fn() -> bool + 'static) {
    let done = Rc::new(Cell::new(false));
    let finished = done.clone();
    let start = Instant::now();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(10), move || {
        if condition() {
            finished.set(true);
            slint::quit_event_loop().unwrap();
        } else if start.elapsed() > Duration::from_secs(30) {
            slint::quit_event_loop().unwrap();
        }
    });
    while !done.get() && start.elapsed() <= Duration::from_secs(30) {
        slint::run_event_loop().unwrap();
    }
    assert!(done.get(), "timed out waiting for {what}");
}

/// Waits for the background saver to write what `check` looks for.
fn saved(file: &Path, what: &str, check: impl Fn(&Settings) -> bool + 'static) {
    let file = file.to_path_buf();
    wait_until(what, move || {
        file.exists() && check(&Settings::load_from(&file))
    });
}

fn schemes(ui: &MainWindow, settings: &SettingsWindow) -> (ColorScheme, ColorScheme) {
    (
        ui.global::<Palette>().get_color_scheme(),
        settings.global::<Palette>().get_color_scheme(),
    )
}

fn make_zip(dir: &Path) -> PathBuf {
    let archive = dir.join("sample.zip");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    zip.start_file("one.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"hello").unwrap();
    zip.finish().unwrap();
    archive
}

#[test]
fn every_setting_applies_at_once_and_is_saved() {
    let data = tempfile::tempdir().unwrap();
    let linux = std::env::consts::OS == "linux";
    if linux {
        // The right-click menu is plain files there, so the switch can be
        // flipped for real without touching this machine's menus.
        // SAFETY: no other thread exists yet in this one-test binary.
        unsafe { std::env::set_var("XDG_DATA_HOME", data.path()) };
    }
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: false,
        threading: true,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    drop(MainWindow::new().unwrap());
    slint::select_bundled_translation("en").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let archive = make_zip(dir.path());
    let file = dir.path().join("config/settings.json");
    let chosen = dir.path().join("Extracted");
    // A folder picked before, with "Next to the archive" in effect, so the
    // destination can be switched without a folder picker.
    let start = Settings {
        destination_folder: Some(chosen.clone()),
        ..Settings::default()
    };
    let drops = Drops::default();
    let ui = main_window::build(Some(archive), &drops, start, Some(file.clone())).unwrap();
    let w = ui.as_weak();
    wait_until("the archive and plan", move || {
        w.upgrade()
            .is_some_and(|w| w.get_screen() == Screen::Opened && w.get_plan().ready)
    });
    let next_to = dir.path().join("sample").display().to_string();
    assert_eq!(ui.get_output(), next_to);

    // File > Settings… and the gear both open it, and always the same one.
    assert!(main_window::settings_window().is_none());
    ui.invoke_open_settings();
    let settings = main_window::settings_window().expect("Settings opens from the menu");
    assert!(settings.window().is_visible());
    assert_eq!(settings.get_settings_path(), file.display().to_string());
    assert_eq!(
        settings.get_shell_menu_available(),
        ui.get_shell_menu_available()
    );
    settings.set_section(3);
    let gear = ElementHandle::find_by_accessible_label(&ui, "Settings")
        .next()
        .expect("the gear in the header");
    gear.invoke_accessible_default_action();
    let again = main_window::settings_window().unwrap();
    assert_eq!(again.get_section(), 3, "the window already open");
    again.set_section(1);
    assert_eq!(settings.get_section(), 1);
    // Closed and opened again: still that one, as it was left.
    settings.invoke_close();
    assert!(!settings.window().is_visible());
    ui.invoke_open_settings();
    assert!(settings.window().is_visible());
    assert_eq!(main_window::settings_window().unwrap().get_section(), 1);

    // General.
    settings.set_appearance_index(2);
    settings.invoke_choose_appearance(2);
    assert_eq!(
        schemes(&ui, &settings),
        (ColorScheme::Dark, ColorScheme::Dark)
    );
    saved(&file, "dark", |s| s.appearance == Appearance::Dark);
    settings.set_appearance_index(1);
    settings.invoke_choose_appearance(1);
    assert_eq!(
        schemes(&ui, &settings),
        (ColorScheme::Light, ColorScheme::Light)
    );
    saved(&file, "light", |s| s.appearance == Appearance::Light);
    // The About window opened now starts in the chosen scheme.
    ui.invoke_about();
    let about = main_window::about_window().unwrap();
    assert_eq!(
        about.global::<Palette>().get_color_scheme(),
        ColorScheme::Light
    );
    about.hide().unwrap();

    settings.set_after_index(1);
    settings.invoke_choose_after(1);
    saved(&file, "open the folder", |s| {
        s.after_extracting == AfterExtracting::OpenFolder
    });
    settings.set_after_index(0);
    settings.invoke_choose_after(0);
    saved(&file, "do nothing", |s| {
        s.after_extracting == AfterExtracting::Nothing
    });

    assert!(settings.get_close_when_done());
    settings.set_close_when_done(false);
    settings.invoke_close_when_done_toggled();
    saved(&file, "stay open", |s| !s.close_when_done);

    // Extraction, kept in step with the main window's own controls.
    assert!(settings.get_remove_archive());
    settings.set_remove_archive(false);
    settings.invoke_remove_archive_toggled();
    assert!(!ui.get_remove_archive());
    saved(&file, "keep the archive", |s| s.keep_archive);
    let w = ui.as_weak();
    wait_until("the plan for keeping the archive", move || {
        w.upgrade().is_some_and(|w| w.get_plan().keep_archive)
    });
    ui.set_remove_archive(true);
    ui.invoke_remove_archive_toggled();
    assert!(settings.get_remove_archive());
    saved(&file, "remove the archive", |s| !s.keep_archive);

    settings.set_verify_first(true);
    settings.invoke_verify_first_toggled();
    assert!(ui.get_verify_first());
    saved(&file, "verify first", |s| s.verify_first);
    ui.set_verify_first(false);
    ui.invoke_verify_first_toggled();
    assert!(!settings.get_verify_first());
    saved(&file, "no verifying", |s| !s.verify_first);

    assert_eq!(settings.get_keep_free(), "2");
    settings.set_keep_free("0.5".into());
    settings.invoke_keep_free_edited("0.5".into());
    assert!(settings.get_keep_free_valid());
    assert_eq!(ui.get_keep_free(), "0.5");
    saved(&file, "half a gigabyte", |s| s.keep_free == 500_000_000);
    // Something unreadable is refused where it was typed and goes nowhere.
    settings.set_keep_free("lots".into());
    settings.invoke_keep_free_edited("lots".into());
    assert!(!settings.get_keep_free_valid());
    assert!(ui.get_keep_free_valid());
    assert_eq!(ui.get_keep_free(), "0.5");
    ui.set_keep_free("3".into());
    ui.invoke_keep_free_edited("3".into());
    assert_eq!(settings.get_keep_free(), "3");
    assert!(settings.get_keep_free_valid());
    saved(&file, "three gigabytes", |s| s.keep_free == 3_000_000_000);

    assert_eq!(
        settings.get_destination_folder(),
        chosen.display().to_string()
    );
    settings.set_destination_index(1);
    settings.invoke_choose_destination(1);
    assert_eq!(
        ui.get_output(),
        chosen.join("sample").display().to_string(),
        "the open archive follows the new default"
    );
    saved(&file, "always in a folder", |s| {
        s.destination == Destination::Folder
    });
    settings.set_destination_index(0);
    settings.invoke_choose_destination(0);
    assert_eq!(ui.get_output(), next_to);
    saved(&file, "next to the archive", |s| {
        s.destination == Destination::NextToArchive && s.destination_folder.is_some()
    });

    for (index, choice) in [
        (2, Existing::Skip),
        (1, Existing::Replace),
        (0, Existing::Ask),
    ] {
        settings.set_existing_index(index);
        settings.invoke_choose_existing(index);
        saved(&file, "the choice for existing files", move |s| {
            s.existing_files == choice
        });
    }

    // Integration, where the platform has a right-click menu to change.
    if linux {
        settings.set_shell_menu_on(true);
        settings.invoke_shell_menu_toggled();
        let s = settings.as_weak();
        wait_until("the menu to be added", move || {
            s.upgrade().is_some_and(|s| s.get_shell_notice() != "")
        });
        assert_eq!(settings.get_shell_notice(), "Context menu entries added.");
        assert!(!settings.get_shell_notice_bad());
        assert!(ui.get_shell_menu_on(), "the start screen's switch follows");
        assert!(
            data.path()
                .join("kio/servicemenus/hatchzip.desktop")
                .exists()
        );
        settings.set_shell_menu_on(false);
        settings.invoke_shell_menu_toggled();
        let s = settings.as_weak();
        wait_until("the menu to be removed", move || {
            s.upgrade()
                .is_some_and(|s| s.get_shell_notice() == "Context menu entries removed.")
        });
        assert!(!ui.get_shell_menu_on());
    }

    // The language, which the window words itself in at once.
    settings.set_language_index(2);
    settings.invoke_choose_language(2);
    assert_eq!(
        settings.get_language_names().row_data(0).unwrap(),
        "Usar el idioma del sistema"
    );
    saved(&file, "Spanish", |s| s.language.as_deref() == Some("es"));

    // Advanced: Reset puts everything back and into effect.
    settings.invoke_reset();
    assert_eq!(
        schemes(&ui, &settings),
        (ColorScheme::Unknown, ColorScheme::Unknown)
    );
    assert_eq!(settings.get_language_index(), 0);
    assert_eq!(
        settings.get_language_names().row_data(0).unwrap(),
        "Use the system language"
    );
    assert_eq!(settings.get_appearance_index(), 0);
    assert_eq!(settings.get_after_index(), 0);
    assert!(settings.get_close_when_done());
    assert!(settings.get_remove_archive());
    assert!(!settings.get_verify_first());
    assert_eq!(settings.get_keep_free(), "2");
    assert_eq!(settings.get_destination_index(), 0);
    assert_eq!(settings.get_destination_folder(), "");
    assert_eq!(settings.get_existing_index(), 0);
    assert_eq!(ui.get_keep_free(), "2");
    assert!(ui.get_remove_archive());
    assert!(!ui.get_verify_first());
    assert_eq!(ui.get_output(), next_to);
    saved(&file, "the defaults", |s| *s == Settings::default());

    settings.hide().unwrap();
    ui.hide().unwrap();

    // A right-click window reads the saved appearance when it opens.
    let args = TaskArgs {
        kind: TaskKind::Test,
        archives: vec![dir.path().join("sample.zip")],
        keep_archive: false,
    };
    let dark = Settings {
        appearance: Appearance::Dark,
        ..Settings::default()
    };
    let (task, _start) = task_window::build(args, dark).unwrap();
    assert_eq!(
        task.global::<Palette>().get_color_scheme(),
        ColorScheme::Dark
    );
    task.hide().unwrap();
}
