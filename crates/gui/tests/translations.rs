//! Exercises the bundled catalog through the same Slint component as the app.
use hatchzip_gui::{MainWindow, Screen, backend::Drops, main_window, settings::Settings};
use i_slint_backend_testing::{ElementHandle, TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, Timer, TimerMode};
use std::cell::Cell;
use std::io::Write;
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

fn snapshot(window: &slint::Window, path: impl AsRef<std::path::Path>) {
    let image = window.take_snapshot().unwrap();
    let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut encoder = png::Encoder::new(file, image.width(), image.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(image.as_bytes())
        .unwrap();
}

#[test]
fn bundled_hindi_and_english_fallback() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: false,
        threading: true,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let ui = MainWindow::new().unwrap();
    slint::select_bundled_translation("hi").unwrap();
    assert_eq!(ui.invoke_open_archive_title(), "आर्काइव खोलें");
    assert_eq!(ui.invoke_reading_archive(37), "आर्काइव पढ़ा जा रहा है… 37%");
    assert_eq!(ui.invoke_entry_count("35".into(), 35), "35 प्रविष्टियाँ");
    assert_eq!(
        ui.invoke_task_heading(
            hatchzip_gui::Phase::Working,
            false,
            false,
            "game.zip".into()
        ),
        "game.zip एक्सट्रैक्ट हो रहा है"
    );
    assert!(ui.get_formats().contains("समर्थित फ़ॉर्मैट"));
    for rar in [false, true] {
        ui.set_supports_rar(rar);
        assert!(ui.get_formats().contains("tar.gz, tar.bz2, tar.xz"));
        assert_eq!(ui.get_formats().contains("RAR"), rar);
    }
    assert!(
        ui.invoke_keep_archive_hint("Reason".into())
            .contains("\"एक्सट्रैक्शन के दौरान आर्काइव हटाएँ\"")
    );
    assert_eq!(
        ui.invoke_remaining_summary("3.2 GB".into(), "120 प्रविष्टियाँ".into()),
        "3.2 GB शेष (120 प्रविष्टियाँ)"
    );
    assert_eq!(
        ui.invoke_uncompressed_summary("3.2 GB".into()),
        "असंपीड़ित आकार: 3.2 GB"
    );
    for (n, number, entry, hour) in [
        (0, "0", "प्रविष्टियाँ", "घंटे"),
        (1, "1", "प्रविष्टि", "घंटा"),
        (2, "2", "प्रविष्टियाँ", "घंटे"),
        (1200, "1,200", "प्रविष्टियाँ", "घंटे"),
    ] {
        assert_eq!(
            ui.invoke_entry_count(number.into(), n),
            format!("{number} {entry}")
        );
        assert_eq!(
            ui.invoke_time_hours(n, number.into()),
            format!("{number} {hour}")
        );
        let expected = if n == 1 {
            "जाँच सफल: 1 प्रविष्टि सत्यापित हुई।".to_string()
        } else {
            format!("जाँच सफल: {number} प्रविष्टियाँ सत्यापित हुईं।")
        };
        assert_eq!(ui.invoke_task_test_result(n, number.into()), expected);
    }
    assert_eq!(
        ui.invoke_restore_result(false, ui.invoke_restore_held(1, "1".into()), "out".into()),
        "आर्काइव पुनर्स्थापित हुआ। अभी एक्सट्रैक्ट होनी बाकी: 1 प्रविष्टि। एक्सट्रैक्ट की गई फ़ाइलें out में बनी रहेंगी।"
    );
    assert!(
        ui.invoke_restore_aside(1, "1".into(), "out".into())
            .contains("out")
    );
    assert_eq!(ui.invoke_checking_space(), "डिस्क की जगह जाँची जा रही है…");
    assert_eq!(
        ui.invoke_paused_message(),
        "प्रगति सहेज ली गई है। एक्सट्रैक्शन अभी या बाद में फिर शुरू किया जा सकता है।"
    );
    assert_eq!(ui.invoke_menu_added(), "संदर्भ मेन्यू प्रविष्टियाँ जोड़ दी गई हैं।");
    assert_eq!(ui.invoke_menu_removed(), "संदर्भ मेन्यू प्रविष्टियाँ हटा दी गई हैं।");
    assert!(
        ui.invoke_restore_result(false, ui.invoke_restore_held(2, "2".into()), "out".into())
            .contains("आर्काइव पुनर्स्थापित हुआ")
    );
    assert_eq!(
        ui.invoke_task_test_result(2, "2".into()),
        "जाँच सफल: 2 प्रविष्टियाँ सत्यापित हुईं।"
    );
    assert_eq!(
        ui.invoke_task_amounts("5 MB".into(), "10 MB".into()),
        "10 MB में से 5 MB"
    );
    assert_eq!(ui.invoke_task_destination("folder".into()), "गंतव्य: folder");
    assert_eq!(
        ui.invoke_replace_question("file".into(), "folder".into()),
        "'file' पहले से folder में मौजूद है।"
    );
    assert_eq!(
        ui.invoke_replace_remaining(2, "2".into()),
        "अगली 2 फ़ाइलों के लिए भी यही करें"
    );
    assert!(
        ui.invoke_replace_instructions(true)
            .contains("आर्काइव की प्रति")
    );
    if let Some(path) = std::env::var_os("HATCHZIP_TRANSLATION_EMPTY_SHOT") {
        ui.window().set_size(slint::LogicalSize::new(640.0, 440.0));
        ui.show().unwrap();
        snapshot(ui.window(), path);
        ui.hide().unwrap();
    }
    // Opening a real archive builds the summary on the UI thread, after the
    // worker has loaded it. It must use the same bundled language.
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    let archive = dir.path().join("sample.zip");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    zip.start_file("root/sub/one.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"hello").unwrap();
    zip.finish().unwrap();
    let drops = Drops::default();
    let window = main_window::build(Some(archive), &drops, Settings::default(), None).unwrap();
    let weak = window.as_weak();
    wait_until("the archive and plan", move || {
        weak.upgrade()
            .is_some_and(|w| w.get_screen() == Screen::Opened && w.get_plan().ready)
    });
    assert!(
        window.get_summary().contains("1 प्रविष्टि"),
        "{}",
        window.get_summary()
    );
    let folder = ElementHandle::find_by_accessible_label(&window, "root/sub")
        .next()
        .expect("implicit folder is visible");
    assert_eq!(folder.accessible_description().unwrap(), "फ़ोल्डर");
    folder.invoke_accessible_expand_action();
    let file = ElementHandle::find_by_accessible_label(&window, "root/sub/one.txt")
        .next()
        .expect("expanded file is visible");
    assert_eq!(file.accessible_description().unwrap(), "फ़ाइल");
    if let Some(path) = std::env::var_os("HATCHZIP_TRANSLATION_SHOT") {
        snapshot(window.window(), path);
    }
    window.invoke_test();
    let weak = window.as_weak();
    wait_until("the archive test", move || {
        weak.upgrade()
            .is_some_and(|w| w.get_task().phase == hatchzip_gui::Phase::Done)
    });
    assert_eq!(window.get_task().heading, "जाँच पूरी हुई");
    assert_eq!(window.get_task().message, "जाँच सफल: 1 प्रविष्टि सत्यापित हुई।");
    window.hide().unwrap();
    // A single-block xz archive requires keeping it. The plan's hint must
    // name the same Hindi checkbox shown by the main window.
    let single = dir.path().join("single.tar.xz");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../cli/tests/fixtures/tarxz/single.tar.xz"),
        &single,
    )
    .unwrap();
    let window = main_window::build(
        Some(single),
        &drops,
        Settings {
            keep_free: 0,
            ..Settings::default()
        },
        None,
    )
    .unwrap();
    let weak = window.as_weak();
    wait_until("the single-block plan", move || {
        weak.upgrade()
            .is_some_and(|w| w.get_screen() == Screen::Opened && w.get_plan().ready)
    });
    assert!(!window.get_plan().fits);
    assert!(
        window
            .get_plan()
            .refusal
            .contains("\"एक्सट्रैक्शन के दौरान आर्काइव हटाएँ\""),
        "{}",
        window.get_plan().refusal
    );
    assert!(
        window.get_plan().refusal.ends_with('।'),
        "{}",
        window.get_plan().refusal
    );
    window.hide().unwrap();
    if let Some(dir) = std::env::var_os("HATCHZIP_TRANSLATION_SHOTS") {
        for language in ["en", "hi"] {
            slint::select_bundled_translation(language).unwrap();
            let preview = main_window::build(
                Some(dir_path.join("sample.zip")),
                &drops,
                Settings::default(),
                None,
            )
            .unwrap();
            let weak = preview.as_weak();
            wait_until("the preview plan", move || {
                weak.upgrade()
                    .is_some_and(|w| w.get_screen() == Screen::Opened && w.get_plan().ready)
            });
            for (tag, width, height) in [("minimum", 640.0, 440.0), ("normal", 980.0, 660.0)] {
                preview
                    .window()
                    .set_size(slint::LogicalSize::new(width, height));
                snapshot(
                    preview.window(),
                    std::path::Path::new(&dir).join(format!("{language}-{tag}.png")),
                );
            }
            preview.hide().unwrap();
        }
    }
    // GB is deliberately absent from the catalog and falls back to English.
    assert_eq!(ui.get_storage_unit(), "GB");
    slint::select_bundled_translation("en").unwrap();
    assert_eq!(ui.invoke_open_archive_title(), "Open archive");
    assert_eq!(ui.invoke_reading_archive(37), "Reading archive… 37%");
    assert_eq!(
        ui.invoke_restore_result(false, ui.invoke_restore_held(2, "2".into()), "out".into()),
        "Archive restored. It contains the 2 entries not yet extracted. Extracted files remain in out."
    );
    assert_eq!(
        ui.invoke_task_test_result(2, "2".into()),
        "Test passed: 2 entries verified."
    );
    assert_eq!(
        ui.invoke_task_amounts("5 MB".into(), "10 MB".into()),
        "5 MB of 10 MB"
    );
    assert_eq!(
        ui.invoke_replace_question("file".into(), "folder".into()),
        "'file' already exists in folder."
    );
    assert_eq!(
        ui.invoke_replace_remaining(1, "1".into()),
        "Do the same for the next file"
    );
    assert_eq!(
        ui.invoke_replace_remaining(1200, "1,200".into()),
        "Do the same for the next 1,200 files"
    );
    assert!(
        ui.invoke_replace_instructions(true)
            .contains("the archive's copy is not kept")
    );

    for (n, number) in [(0, "0"), (1, "1"), (2, "2"), (1200, "1,200")] {
        let unit = if n == 1 { "entry" } else { "entries" };
        assert_eq!(
            ui.invoke_entry_count(number.into(), n),
            format!("{number} {unit}")
        );
        assert_eq!(
            ui.invoke_restore_held(n, number.into()),
            format!("the {number} {unit}")
        );
    }
    assert_eq!(ui.invoke_time_minutes(0, "0".into()), "0 minutes");
    assert_eq!(ui.invoke_time_minutes(1, "1".into()), "1 minute");
    assert_eq!(ui.invoke_time_seconds(2, "2".into()), "2 seconds");
    assert_eq!(
        ui.invoke_restore_aside(1, "1".into(), "out".into()),
        "1 replaced file remains in out, since the file extracted at its path is no longer in the archive."
    );
    assert_eq!(
        ui.invoke_restore_aside(2, "2".into(), "out".into()),
        "2 replaced files remain in out, since the files extracted at their paths are no longer in the archive."
    );
    assert_eq!(
        ui.invoke_keep_archive_hint("Reason".into()),
        "Reason. Turn off \"Remove the archive during extraction\" to extract it and keep the archive."
    );
    assert_eq!(ui.invoke_replace_remaining(0, "0".into()), "");
    assert_eq!(ui.invoke_entry_count("35".into(), 35), "35 entries");
    assert!(ui.get_formats().starts_with("Or drag an archive"));
}
