//! The main window on a disk with room to extract in place but not to keep
//! the archive: the plan says so, names what removing the archive needs,
//! and extracting in place then works.
//!
//! A binary of its own, since the free space it fakes (`HATCHZIP_FREE`,
//! read by debug builds only) must be set before any thread starts and
//! holds for the whole process. `HATCHZIP_GUI_SHOTS=<dir>` saves each
//! state as a PNG, as `tests/ui.rs` does.

use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use hatchzip_core::engine::Job;
use hatchzip_core::{human, session};
use hatchzip_gui::backend::Drops;
use hatchzip_gui::settings::Settings;
use hatchzip_gui::{MainWindow, Palette, Phase, Screen, main_window};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::language::ColorScheme;
use slint::{ComponentHandle, LogicalSize, Timer, TimerMode};

/// Data that doesn't compress, stored: the archive is as big as what it
/// holds, so removing it during extraction saves nearly all of it.
fn make_zip(dir: &Path) -> PathBuf {
    let path = dir.join("Northwind-HD.zip");
    let mut w = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in 0..10 {
        let data: Vec<u8> = (0..400_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        w.start_file(format!("Northwind/textures/pack{i:02}.pak"), opts)
            .unwrap();
        w.write_all(&data).unwrap();
    }
    w.finish().unwrap();
    path
}

fn wait_until(what: &str, done: impl Fn() -> bool + 'static) {
    let ok = Rc::new(Cell::new(false));
    let start = Instant::now();
    let timer = Timer::default();
    let o = ok.clone();
    timer.start(TimerMode::Repeated, Duration::from_millis(5), move || {
        if done() {
            o.set(true);
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

fn shot(window: &slint::Window, name: &str) {
    let Some(dir) = std::env::var_os("HATCHZIP_GUI_SHOTS") else {
        return;
    };
    let start = Instant::now();
    wait_until("a moment", move || {
        start.elapsed() > Duration::from_millis(400)
    });
    let image = window.take_snapshot().expect("rendering a snapshot");
    let path = Path::new(&dir).join(format!("{name}.png"));
    let file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    let mut png = png::Encoder::new(file, image.width(), image.height());
    png.set_color(png::ColorType::Rgba);
    png.set_depth(png::BitDepth::Eight);
    png.write_header()
        .unwrap()
        .write_image_data(image.as_bytes())
        .unwrap();
}

#[test]
fn keeping_does_not_fit_but_removing_does() {
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let archives: Vec<PathBuf> = dirs.iter().map(|d| make_zip(d.path())).collect();
    let (normal, in_place) = {
        let job = Job::open(&archives[0], false, None, Default::default()).unwrap();
        let plan = session::plan(&job, &dirs[0].path().join("out"), false, 0, true).unwrap();
        (plan.normal_needs, plan.in_place_needs)
    };
    assert!(in_place < normal / 2, "in place {in_place}, kept {normal}");
    // SAFETY: no other thread exists yet in this one-test binary.
    unsafe { std::env::set_var("HATCHZIP_FREE", ((normal + in_place) / 2).to_string()) };
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: false,
        threading: true,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    // The windows follow the system language, and this flow reads English
    // labels. Slint registers its bundled catalogs with the first component.
    drop(MainWindow::new().unwrap());
    slint::select_bundled_translation("en").unwrap();

    for ((scheme, tag), archive) in [(ColorScheme::Light, "light"), (ColorScheme::Dark, "dark")]
        .into_iter()
        .zip(&archives)
    {
        let drops = Drops::default();
        let settings = Settings {
            keep_free: 0,
            keep_archive: true,
            ..Settings::default()
        };
        let ui = main_window::build(Some(archive.clone()), &drops, settings, None).unwrap();
        ui.global::<Palette>().set_color_scheme(scheme);
        ui.window().set_size(LogicalSize::new(980.0, 660.0));
        let w = ui.as_weak();
        wait_until("the archive to open", move || {
            let ui = w.unwrap();
            ui.get_screen() == Screen::Opened && ui.get_plan().ready
        });
        assert!(!ui.get_remove_archive(), "the saved choice is applied");
        let plan = ui.get_plan();
        assert!(plan.keep_archive && !plan.fits);
        assert!(
            plan.verdict.starts_with("Insufficient disk space. "),
            "{}",
            plan.verdict
        );
        assert_eq!(
            plan.note,
            format!(
                "Removing the archive during extraction requires only {}.",
                human(in_place)
            )
        );
        shot(ui.window(), &format!("{tag}-22-keep-no-fit"));

        ui.set_remove_archive(true);
        ui.invoke_remove_archive_toggled();
        let w = ui.as_weak();
        wait_until("the plan for removing it", move || {
            let plan = w.unwrap().get_plan();
            !plan.keep_archive && plan.fits
        });
        assert_eq!(ui.get_plan().verdict, "Sufficient disk space");
        shot(ui.window(), &format!("{tag}-23-remove-fits"));

        ui.invoke_extract();
        let w = ui.as_weak();
        wait_until("the extraction", move || {
            matches!(w.unwrap().get_task().phase, Phase::Done | Phase::Failed)
        });
        assert_eq!(
            ui.get_task().message,
            "All entries were extracted. The archive was removed."
        );
        assert!(!archive.exists());
        ui.hide().unwrap();
    }
}
