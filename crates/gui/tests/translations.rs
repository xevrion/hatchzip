//! Exercises the bundled catalog through the same Slint component as the app.
use hatchzip_gui::{MainWindow, Screen, backend::Drops, main_window, settings::Settings};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, Timer, TimerMode};
use std::cell::Cell;
use std::io::Write;
use std::rc::Rc;
use std::time::{Duration, Instant};

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
    assert_eq!(ui.invoke_entry_count("35".into(), false), "35 प्रविष्टियाँ");
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
    assert_eq!(ui.invoke_checking_space(), "डिस्क की जगह जाँची जा रही है…");
    assert_eq!(
        ui.invoke_paused_message(),
        "प्रगति सहेज ली गई है। एक्सट्रैक्शन अभी या बाद में फिर शुरू किया जा सकता है।"
    );
    assert_eq!(ui.invoke_menu_added(), "संदर्भ मेन्यू प्रविष्टियाँ जोड़ दी गई हैं।");
    assert_eq!(ui.invoke_menu_removed(), "संदर्भ मेन्यू प्रविष्टियाँ हटा दी गई हैं।");
    assert!(
        ui.invoke_restore_result(
            false,
            ui.invoke_restore_held(false, "2".into()),
            "out".into()
        )
        .contains("आर्काइव पुनर्स्थापित हुआ")
    );
    assert_eq!(
        ui.invoke_task_test_result(false, "2".into()),
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
        let image = ui.window().take_snapshot().unwrap();
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut encoder = png::Encoder::new(file, image.width(), image.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(image.as_bytes())
            .unwrap();
        ui.hide().unwrap();
    }
    // Opening a real archive builds the summary on the UI thread, after the
    // worker has loaded it. It must use the same bundled language.
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("sample.zip");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    zip.start_file("one.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"hello").unwrap();
    zip.finish().unwrap();
    let drops = Drops::default();
    let window = main_window::build(Some(archive), &drops, Settings::default(), None).unwrap();
    let finished = Rc::new(Cell::new(false));
    let done = finished.clone();
    let weak = window.as_weak();
    let started = Instant::now();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(10), move || {
        if weak
            .upgrade()
            .is_some_and(|w| w.get_screen() == Screen::Opened)
        {
            done.set(true);
            slint::quit_event_loop().unwrap();
        } else if started.elapsed() > Duration::from_secs(30) {
            slint::quit_event_loop().unwrap();
        }
    });
    while !finished.get() && started.elapsed() <= Duration::from_secs(30) {
        slint::run_event_loop().unwrap();
    }
    assert!(finished.get(), "archive did not open");
    assert!(
        window.get_summary().contains("1 प्रविष्टि"),
        "{}",
        window.get_summary()
    );
    if let Some(path) = std::env::var_os("HATCHZIP_TRANSLATION_SHOT") {
        let image = window.window().take_snapshot().unwrap();
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
    timer.stop();
    window.invoke_test();
    let tested = Rc::new(Cell::new(false));
    let done = tested.clone();
    let weak = window.as_weak();
    let started = Instant::now();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(10), move || {
        if weak
            .upgrade()
            .is_some_and(|w| w.get_task().phase == hatchzip_gui::Phase::Done)
        {
            done.set(true);
            slint::quit_event_loop().unwrap();
        } else if started.elapsed() > Duration::from_secs(30) {
            slint::quit_event_loop().unwrap();
        }
    });
    while !tested.get() && started.elapsed() <= Duration::from_secs(30) {
        slint::run_event_loop().unwrap();
    }
    assert!(tested.get(), "archive test did not finish");
    assert_eq!(window.get_task().heading, "जाँच पूरी हुई");
    assert_eq!(window.get_task().message, "जाँच सफल: 1 प्रविष्टि सत्यापित हुई।");
    timer.stop();
    window.hide().unwrap();
    // GB is deliberately absent from the catalog and falls back to English.
    assert_eq!(ui.get_storage_unit(), "GB");
    slint::select_bundled_translation("en").unwrap();
    assert_eq!(ui.invoke_open_archive_title(), "Open archive");
    assert_eq!(ui.invoke_reading_archive(37), "Reading archive… 37%");
    assert_eq!(
        ui.invoke_restore_result(
            false,
            ui.invoke_restore_held(false, "2".into()),
            "out".into()
        ),
        "Archive restored. It contains the 2 entries not yet extracted. Extracted files remain in out."
    );
    assert_eq!(
        ui.invoke_task_test_result(false, "2".into()),
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

    assert_eq!(ui.invoke_entry_count("35".into(), false), "35 entries");
    assert!(ui.get_formats().starts_with("Or drag an archive"));
}
