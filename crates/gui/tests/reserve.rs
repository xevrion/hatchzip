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
use hatchzip_gui::{Phase, task_window};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;
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

    let dir = tempfile::tempdir().unwrap();
    let archive = make_zip(dir.path());
    let args = TaskArgs {
        kind: TaskKind::ExtractTo,
        archives: vec![archive.clone()],
        keep_archive: false,
    };
    // The 2 GB default reserve is tripled over against 1 GB of free space;
    // the zip itself needs a few MB.
    let settings = Settings {
        keep_free: 2_000_000_000,
        close_when_done: false,
        ..Settings::default()
    };
    let (ui, start) = task_window::build(args, settings).unwrap();
    ui.window().set_size(slint::LogicalSize::new(480.0, 270.0));
    start();

    let w = ui.as_weak();
    wait_until("the space check to stop for the reserve", move || {
        w.unwrap().get_task().phase == Phase::NoSpace
    });
    assert!(ui.get_task().lower_reserve);
    let amount = ui.get_task().reserve_amount.to_string();
    assert!(amount.ends_with(" MB"), "{amount}");

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
    assert!(
        dir.path().join("game/data/one.bin").exists(),
        "the file was extracted"
    );
    ui.hide().unwrap();
}
