//! The no-space offer end to end: when the reserve is the only thing in the
//! way, the button lowers it for that run and the extraction finishes.
//!
//! The free space is faked with `HATCHZIP_FREE`, a debug-build knob, so the
//! plan stops for the 2 GB reserve on a disk that would fit the extraction a
//! hundred times over. The run itself writes to the real one.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use hatchzip_gui::args::{TaskArgs, TaskKind};
use hatchzip_gui::settings::Settings;
use hatchzip_gui::{MainWindow, Messages, Palette, Phase, task_window};
use i_slint_backend_testing::{ElementHandle, TestingBackend, TestingBackendOptions};
use slint::language::ColorScheme;
use slint::{ComponentHandle, LogicalSize};
use slint::{Timer, TimerMode};
use std::cell::Cell;

fn wait_until(what: &str, done: impl Fn() -> bool + 'static) {
    let ok = Rc::new(Cell::new(false));
    let start = Instant::now();
    let timer = Timer::default();
    let flag = ok.clone();
    timer.start(TimerMode::Repeated, Duration::from_millis(5), move || {
        if done() {
            flag.set(true);
            let _ = slint::quit_event_loop();
        } else if start.elapsed() > Duration::from_secs(30) {
            let _ = slint::quit_event_loop();
        }
    });
    while !ok.get() && start.elapsed() <= Duration::from_secs(30) {
        slint::run_event_loop().unwrap();
    }
    assert!(ok.get(), "timed out waiting for {what}");
}

fn make_zip(dir: &Path) -> PathBuf {
    let path = dir.join("game.zip");
    let mut w = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    w.add_directory("data/", zip::write::SimpleFileOptions::default())
        .unwrap();
    w.start_file("data/one.bin", zip::write::SimpleFileOptions::default())
        .unwrap();
    w.write_all(&vec![7u8; 200_000]).unwrap();
    w.finish().unwrap();
    path
}

#[test]
fn the_offer_lowers_the_reserve_and_finishes() {
    // SAFETY: nothing has started yet; the platform below is the first
    // thread this process sees, and none reads the environment before it runs.
    unsafe { std::env::set_var("HATCHZIP_FREE", "1000000000") };
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: false,
        threading: true,
        renderer_name: Some("software".into()),
    })))
    .unwrap();

    // Register the catalogs before selecting the language for each window.
    drop(MainWindow::new().unwrap());
    for language in ["en", "hi"] {
        slint::select_bundled_translation(language).unwrap();
        for scheme in [ColorScheme::Light, ColorScheme::Dark] {
            let dir = tempfile::tempdir().unwrap();
            let archive = make_zip(dir.path());
            let original = std::fs::read(&archive).unwrap();
            let args = TaskArgs {
                kind: TaskKind::ExtractTo,
                archives: vec![archive.clone()],
                keep_archive: false,
            };
            // The 2 GB reserve exceeds the 1 GB of free space; the ZIP
            // itself needs only a few MB.
            let settings = Settings {
                keep_free: 2_000_000_000,
                close_when_done: false,
                ..Settings::default()
            };
            let (ui, start) = task_window::build(args, settings).unwrap();
            ui.global::<Palette>().set_color_scheme(scheme);
            ui.window().set_size(LogicalSize::new(420.0, 270.0));
            start();

            let w = ui.as_weak();
            wait_until("the space check to stop for the reserve", move || {
                w.unwrap().get_task().phase == Phase::NoSpace
            });
            assert!(ui.get_task().lower_reserve);
            let amount = ui.get_task().reserve_amount.to_string();
            assert!(amount.ends_with(" MB"), "{amount}");
            let message = ui.get_task().message;
            assert_eq!(
                message,
                ui.global::<Messages>()
                    .invoke_task_no_space(ui.get_plan().short)
            );
            let _ = ui.window().take_snapshot().unwrap();
            assert_eq!(
                ElementHandle::find_by_accessible_label(&ui, &message).count(),
                1,
                "the shortfall must appear once ({language}, {scheme:?}): {message}"
            );
            assert_eq!(std::fs::read(&archive).unwrap(), original);

            ui.invoke_lower_reserve();
            let w = ui.as_weak();
            wait_until("the extraction to finish", move || {
                let phase = w.unwrap().get_task().phase;
                phase == Phase::Done || phase == Phase::Failed
            });
            assert_eq!(
                ui.get_task().phase,
                Phase::Done,
                "{}",
                ui.get_task().message
            );
            assert!(!archive.exists(), "the archive was removed");
            assert_eq!(
                std::fs::read(dir.path().join("game/data/one.bin")).unwrap(),
                vec![7u8; 200_000]
            );
            let message = ui.get_task().message;
            assert!(!message.is_empty());
            let _ = ui.window().take_snapshot().unwrap();
            assert_eq!(
                ElementHandle::find_by_accessible_label(&ui, &message).count(),
                1,
                "the completion message must remain visible ({language}, {scheme:?})"
            );
            ui.hide().unwrap();
        }
    }
}
