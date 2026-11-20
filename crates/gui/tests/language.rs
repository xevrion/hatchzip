//! The language in the Settings window, through the same callbacks its
//! controls call.
use hatchzip_gui::{Screen, SettingsWindow, backend::Drops, main_window, settings::Settings};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, Model, Timer, TimerMode};
use std::cell::Cell;
use std::io::Write;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

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

/// Picks entry `index` of the language list, as a click on it does.
fn choose(settings: &SettingsWindow, index: i32) {
    settings.set_language_index(index);
    settings.invoke_choose_language(index);
}

fn saved_language(file: &Path) -> Option<String> {
    Settings::load_from(file).language
}

#[test]
fn choosing_a_language_switches_the_window_and_is_saved() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: false,
        threading: true,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("sample.zip");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    zip.start_file("one.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"hello").unwrap();
    zip.finish().unwrap();
    let settings_file = dir.path().join("config/settings.json");
    let drops = Drops::default();
    let ui = main_window::build(
        Some(archive),
        &drops,
        Settings::default(),
        Some(settings_file.clone()),
    )
    .unwrap();
    let weak = ui.as_weak();
    wait_until("the archive and plan", move || {
        weak.upgrade()
            .is_some_and(|w| w.get_screen() == Screen::Opened && w.get_plan().ready)
    });

    ui.invoke_open_settings();
    let settings = main_window::settings_window().expect("Settings opens from the menu");
    let names: Vec<String> = settings
        .get_language_names()
        .iter()
        .map(|name| name.to_string())
        .collect();
    assert_eq!(
        names,
        [
            "Use the system language",
            "English",
            "Español",
            "हिन्दी",
            "简体中文"
        ]
    );
    assert_eq!(settings.get_language_index(), 0);
    let english_label = ui.get_shell_menu_label();
    assert!(ui.get_formats().contains("Supported formats"));
    assert!(ui.get_summary().contains("1 entry"), "{}", ui.get_summary());

    choose(&settings, 4);
    assert!(
        ui.get_formats().contains("支持的格式"),
        "{}",
        ui.get_formats()
    );
    assert!(
        ui.get_summary().contains("1 个条目"),
        "{}",
        ui.get_summary()
    );
    // Set once from Rust at startup, so it has to be worded again.
    assert!(
        ui.get_shell_menu_label().contains("右键菜单"),
        "{}",
        ui.get_shell_menu_label()
    );
    assert_eq!(settings.get_language_index(), 4);
    // The first entry is worded again; the names stay in their own scripts
    // whatever is showing.
    assert_eq!(
        settings.get_language_names().row_data(0).unwrap(),
        "使用系统语言"
    );
    assert_eq!(
        settings.get_language_names().row_data(1).unwrap(),
        "English"
    );
    let file = settings_file.clone();
    wait_until("Chinese to be saved", move || {
        saved_language(&file).as_deref() == Some("zh_CN")
    });

    choose(&settings, 1);
    assert!(ui.get_formats().starts_with("Or drag an archive"));
    assert!(ui.get_summary().contains("1 entry"), "{}", ui.get_summary());
    assert_eq!(ui.get_shell_menu_label(), english_label);
    assert_eq!(settings.get_language_index(), 1);
    let file = settings_file.clone();
    wait_until("English to be saved", move || {
        saved_language(&file).as_deref() == Some("en")
    });

    // Tests never follow the system, so here the system option is English;
    // what matters is that the choice is forgotten.
    choose(&settings, 0);
    assert!(ui.get_formats().starts_with("Or drag an archive"));
    assert_eq!(settings.get_language_index(), 0);
    let file = settings_file.clone();
    wait_until("the choice to be forgotten", move || {
        saved_language(&file).is_none() && file.exists()
    });
    settings.hide().unwrap();
    ui.hide().unwrap();
}
