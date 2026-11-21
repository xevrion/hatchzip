//! Renders every window of the desktop app to PNGs through the real window
//! system, for looking at what each OS draws without building locally.
//!
//! ```sh
//! SLINT_BACKEND=winit-software cargo run --example screenshots -p hatchzip-gui -- <out-dir>
//! ```
//!
//! Each scene is saved in light and dark and in every bundled language, at
//! the window's default size, and the main and Settings windows also at
//! their minimum size: `<scene>-<language>-<theme>[-min].png`. The software
//! renderer is the one that can draw a window into a buffer on any machine;
//! `SLINT_BACKEND=winit-software` picks it, as for the app.
//!
//! Opening, the space plan and the finished extractions are real, on an
//! archive made in a temporary folder. A running extraction and a failure
//! pass too quickly or need a broken archive, so they are drawn from the
//! values a run would hold, worded by the same functions the app uses.

use std::cell::RefCell;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::rc::Rc;
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use hatchzip_core::human;
use hatchzip_gui::args::{TaskArgs, TaskKind};
use hatchzip_gui::backend::{self, Drops};
use hatchzip_gui::settings::{Appearance, Settings};
use hatchzip_gui::{
    MainWindow, Messages, Phase, Screen, TaskView, locale, main_window, task_window,
};
use i_slint_backend_testing::ElementHandle;
use slint::{ComponentHandle, LogicalSize, Timer};

/// The bundled languages, by the codes the Settings window saves.
const LANGUAGES: [&str; 4] = ["en", "es", "hi", "zh_CN"];

/// The `min-width` and `min-height` in `ui/main-window.slint` and
/// `ui/settings-window.slint`.
const MAIN_MIN: (f32, f32) = (640.0, 440.0);
const SETTINGS_MIN: (f32, f32) = (600.0, 420.0);

const TIMEOUT: Duration = Duration::from_secs(30);

fn main() -> ExitCode {
    let Some(out) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: screenshots <out-dir>");
        return ExitCode::from(2);
    };
    match run(out) {
        Ok(count) => {
            println!("{count} screenshots written");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("screenshots: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(out: PathBuf) -> Result<usize> {
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let drops = Rc::new(Drops::default());
    // The app's own backend setup, so SLINT_BACKEND chooses the renderer
    // exactly as it does there.
    backend::select(drops.clone())?;
    // Slint registers its bundled catalogs with the first component, so one
    // has to exist before a language can be selected.
    drop(MainWindow::new()?);

    let result: Rc<RefCell<Option<Result<usize>>>> = Rc::default();
    let slot = result.clone();
    slint::spawn_local(async move {
        let outcome = every_scene(&out, &drops).await;
        *slot.borrow_mut() = Some(outcome);
        let _ = slint::quit_event_loop();
    })
    .map_err(|e| anyhow!("starting the scenes: {e}"))?;
    // Windows come and go between scenes; only the scenes end the loop.
    slint::run_event_loop_until_quit()?;
    let outcome = result.borrow_mut().take();
    outcome.unwrap_or_else(|| Err(anyhow!("the event loop stopped before the scenes finished")))
}

async fn every_scene(out: &Path, drops: &Drops) -> Result<usize> {
    let mut count = 0;
    for language in LANGUAGES {
        // Before any window of this pass exists, since the Rust side words
        // some text once, when a window is built or a state is set.
        locale::select(Some(language));
        for (theme, appearance) in [("light", Appearance::Light), ("dark", Appearance::Dark)] {
            let settings = Settings {
                language: Some(language.to_string()),
                appearance,
                close_when_done: false,
                ..Settings::default()
            };
            for min in [false, true] {
                let shots = Shots {
                    dir: out,
                    language,
                    theme,
                    min,
                    count: 0,
                };
                count += main_and_settings(shots, drops, &settings).await?;
            }
            let shots = Shots {
                dir: out,
                language,
                theme,
                min: false,
                count: 0,
            };
            count += task_window_scenes(shots, &settings).await?;
        }
    }
    Ok(count)
}

struct Shots<'a> {
    dir: &'a Path,
    language: &'a str,
    theme: &'a str,
    min: bool,
    count: usize,
}

impl Shots<'_> {
    async fn take(&mut self, window: &slint::Window, scene: &str) -> Result<()> {
        // Let state changes finish animating, as a person would see them,
        // and the window system catch up with a new size.
        sleep(Duration::from_millis(500)).await;
        let name = format!(
            "{scene}-{}-{}{}.png",
            self.language,
            self.theme,
            if self.min { "-min" } else { "" }
        );
        let image = window
            .take_snapshot()
            .map_err(|e| anyhow!("rendering {name}: {e}"))?;
        let path = self.dir.join(&name);
        let file =
            std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        let mut png =
            png::Encoder::new(std::io::BufWriter::new(file), image.width(), image.height());
        png.set_color(png::ColorType::Rgba);
        png.set_depth(png::BitDepth::Eight);
        png.write_header()?.write_image_data(image.as_bytes())?;
        println!("{name} {}x{}", image.width(), image.height());
        self.count += 1;
        Ok(())
    }

    /// The minimum size for a minimum-size pass; the default size, which is
    /// the window's preferred size, is left alone.
    async fn size(&self, window: &slint::Window, (width, height): (f32, f32)) {
        if !self.min {
            return;
        }
        window.set_size(LogicalSize::new(width, height));
        let applied = wait_until("the window to take its minimum size", || {
            let size = window.size().to_logical(window.scale_factor());
            (size.width - width).abs() < 1.0 && (size.height - height).abs() < 1.0
        })
        .await;
        if applied.is_err() {
            let size = window.size().to_logical(window.scale_factor());
            eprintln!(
                "warning: asked for {width}x{height}, the window is {}x{}",
                size.width, size.height
            );
        }
    }
}

/// The main window through a real open and extraction, then the Settings
/// window and the About window it opens.
async fn main_and_settings(
    mut shots: Shots<'_>,
    drops: &Drops,
    settings: &Settings,
) -> Result<usize> {
    let dir = tempfile::tempdir()?;
    let (archive, total) = make_zip(dir.path())?;
    let file = dir.path().join("config/settings.json");
    let ui = main_window::build(None, drops, settings.clone(), Some(file))?;
    shots.size(ui.window(), MAIN_MIN).await;
    wait_until("the start screen", || ui.get_screen() == Screen::Empty).await?;
    shots.take(ui.window(), "main-start").await?;

    // Opened the way a file dropped on the window is.
    {
        let drop_file = drops.on_drop.borrow();
        (drop_file
            .as_ref()
            .context("the main window takes no drops")?)(archive.clone());
    }
    wait_until("the archive to open", || {
        ui.get_screen() == Screen::Opened && ui.get_plan().ready
    })
    .await?;
    if !ui.get_plan().fits {
        eprintln!(
            "warning: the plan does not fit on this runner: {}",
            ui.get_plan().refusal
        );
    }
    shots.take(ui.window(), "main-opened").await?;

    // 100 TB kept free cannot fit anywhere.
    let keep_free = ui.get_keep_free();
    ui.set_keep_free("100000".into());
    ui.invoke_keep_free_edited("100000".into());
    wait_until("the plan to say no", || {
        let plan = ui.get_plan();
        plan.ready && !plan.fits
    })
    .await?;
    shots.take(ui.window(), "main-no-space").await?;
    ui.set_keep_free(keep_free.clone());
    ui.invoke_keep_free_edited(keep_free);
    wait_until("the plan to fit again", || ui.get_plan().fits).await?;

    let name = ui.get_archive_name();
    let output = ui.get_output();
    let messages = ui.global::<Messages>();
    let destination = messages.invoke_task_destination(output.clone());
    let done = total * 42 / 100;
    ui.set_task(TaskView {
        phase: Phase::Working,
        heading: messages.invoke_task_heading(Phase::Working, false, false, name.clone()),
        detail: destination.clone(),
        fraction: 0.42,
        amounts: messages.invoke_task_amounts(human(done).into(), human(total).into()),
        rate: messages.invoke_task_rate(
            human(48_000_000).into(),
            messages.invoke_time_minutes(2, "2".into()),
        ),
        archive_left: messages.invoke_task_archive_left(human(total - done).into()),
        ..TaskView::default()
    });
    shots.take(ui.window(), "main-running").await?;

    // Engine errors stay English in every language, as in the app.
    let reason = "Unexpected end of data in Northwind/data/level07.pak.";
    ui.set_task(TaskView {
        phase: Phase::Failed,
        heading: messages.invoke_task_heading(Phase::Failed, false, false, name),
        detail: destination,
        message: messages.invoke_task_resumable_error(reason.into()),
        can_retry: true,
        can_give_back: true,
        ..TaskView::default()
    });
    shots.take(ui.window(), "main-failed").await?;

    ui.set_task(TaskView::default());
    ui.invoke_extract();
    wait_until("the extraction", || {
        matches!(ui.get_task().phase, Phase::Done | Phase::Failed)
    })
    .await?;
    if ui.get_task().phase != Phase::Done {
        bail!("the extraction failed: {}", ui.get_task().message);
    }
    shots.take(ui.window(), "main-done").await?;

    ui.invoke_open_settings();
    let sw = main_window::settings_window().context("Settings did not open")?;
    shots.size(sw.window(), SETTINGS_MIN).await;
    for (section, scene) in [
        (0, "settings-general"),
        (1, "settings-extraction"),
        (2, "settings-integration"),
        (3, "settings-advanced"),
    ] {
        // Integration exists only where the right-click menu can be changed.
        if section == 2 && !sw.get_shell_menu_available() {
            continue;
        }
        sw.set_section(section);
        shots.take(sw.window(), scene).await?;
    }
    // The button is found by its English label, which does not depend on
    // the language the shot is in, and then pressed as assistive technology
    // would.
    locale::select(Some("en"));
    let reset = ElementHandle::find_by_accessible_label(&sw, "Reset all settings…").next();
    locale::select(Some(shots.language));
    reset
        .context("no \"Reset all settings…\" button in Advanced")?
        .invoke_accessible_default_action();
    shots.take(sw.window(), "settings-reset").await?;
    sw.hide()?;

    if !shots.min {
        ui.invoke_about();
        let about = main_window::about_window().context("About did not open")?;
        shots.take(about.window(), "about").await?;
        about.hide()?;
    }
    ui.hide()?;
    Ok(shots.count)
}

/// The window a right-click opens, extracting and done.
async fn task_window_scenes(mut shots: Shots<'_>, settings: &Settings) -> Result<usize> {
    let dir = tempfile::tempdir()?;
    let (archive, total) = make_zip(dir.path())?;
    let args = TaskArgs {
        kind: TaskKind::ExtractHere,
        archives: vec![archive.clone()],
        keep_archive: false,
    };
    // Nothing touches the archive until `start` is called.
    let (ui, start) = task_window::build(args, settings.clone())?;

    // This window shows the task's text as `src/task.rs` words it, so the
    // running state is worded the same way here.
    let name = archive
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let done = total * 42 / 100;
    ui.set_task(TaskView {
        phase: Phase::Working,
        heading: format!("Extracting {name}").into(),
        detail: format!("Destination: {}", dir.path().display()).into(),
        space: format!(
            "Required: {}. Available: {}.",
            human(total),
            human(21_300_000_000)
        )
        .into(),
        fraction: 0.42,
        amounts: format!("{} of {}", human(done), human(total)).into(),
        rate: format!("{}/s, 2 minutes remaining", human(48_000_000)).into(),
        archive_left: format!("Archive remaining: {}", human(total - done)).into(),
        ..TaskView::default()
    });
    shots.take(ui.window(), "task-running").await?;

    start();
    wait_until("the right-click extraction", || {
        matches!(ui.get_task().phase, Phase::Done | Phase::Failed)
    })
    .await?;
    if ui.get_task().phase != Phase::Done {
        bail!(
            "the right-click extraction failed: {}",
            ui.get_task().message
        );
    }
    shots.take(ui.window(), "task-done").await?;
    ui.hide()?;
    Ok(shots.count)
}

/// A small game-like archive, and the size of what it holds.
fn make_zip(dir: &Path) -> Result<(PathBuf, u64)> {
    let path = dir.join("northwind.zip");
    let mut w = zip::ZipWriter::new(std::fs::File::create(&path)?);
    let opts = zip::write::SimpleFileOptions::default();
    for d in ["Northwind/", "Northwind/bin/", "Northwind/data/"] {
        w.add_directory(d, opts)?;
    }
    let mut total = 0;
    let mut add = |w: &mut zip::ZipWriter<std::fs::File>, name: &str, data: &[u8]| -> Result<()> {
        w.start_file(name, opts)?;
        w.write_all(data)?;
        total += data.len() as u64;
        Ok(())
    };
    add(&mut w, "Northwind/README.txt", b"Northwind demo build.\n")?;
    add(&mut w, "Northwind/bin/northwind", &vec![7u8; 300_000])?;
    for i in 0..30 {
        add(
            &mut w,
            &format!("Northwind/data/level{i:02}.pak"),
            &vec![i as u8; 40_000 + i * 1000],
        )?;
    }
    w.finish()?;
    Ok((path, total))
}

/// Gives the event loop `what` until `done` holds, so worker threads can
/// report in and windows can draw.
async fn wait_until(what: &str, done: impl Fn() -> bool) -> Result<()> {
    let start = Instant::now();
    while !done() {
        if start.elapsed() > TIMEOUT {
            bail!("timed out waiting for {what}");
        }
        sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

fn sleep(duration: Duration) -> Sleep {
    Sleep {
        until: Instant::now() + duration,
    }
}

/// A pause that keeps the Slint event loop running.
struct Sleep {
    until: Instant,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Poll::Ready(());
        }
        let waker = cx.waker().clone();
        Timer::single_shot(left, move || waker.wake());
        Poll::Pending
    }
}
