//! Drives the real windows without a display, through Slint's testing
//! backend and its software renderer: open an archive by dropping it, watch
//! the space plan react, test it, extract it, type a password, give a half
//! extracted archive back, switch the right-click menu, and run a
//! right-click task, over existing files too.
//!
//! With `HATCHZIP_GUI_SHOTS=<dir>` every state is also saved as a PNG, in
//! light and dark, for checking the look by eye.
//!
//! The backend can be installed once per process, so this is one test.

use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use hatchzip_core::session::{self, State};
use hatchzip_gui::args::{TaskArgs, TaskKind};
use hatchzip_gui::backend::Drops;
use hatchzip_gui::settings::Settings;
use hatchzip_gui::{
    Palette, Phase, PlanView, Screen, TaskView, TaskWindow, main_window, task_window,
};
use i_slint_backend_testing::{ElementHandle, TestingBackend, TestingBackendOptions};
use slint::language::ColorScheme;
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model, Timer, TimerMode};

const PASSWORD: &str = "open sesame";

fn make_zip(dir: &Path) -> PathBuf {
    make_zip_with(dir, None)
}

fn make_zip_with(dir: &Path, password: Option<&str>) -> PathBuf {
    let path = dir.join("northwind.zip");
    let mut w = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    let plain = zip::write::SimpleFileOptions::default();
    let opts = match password {
        Some(p) => plain.with_aes_encryption(zip::AesMode::Aes256, p),
        None => plain,
    };
    for d in ["Northwind/", "Northwind/bin/", "Northwind/data/"] {
        w.add_directory(d, plain).unwrap();
    }
    w.start_file("Northwind/README.txt", opts).unwrap();
    w.write_all(b"Northwind demo build.\n").unwrap();
    w.start_file("Northwind/bin/northwind", opts).unwrap();
    w.write_all(&vec![7u8; 300_000]).unwrap();
    for i in 0..30 {
        w.start_file(format!("Northwind/data/level{i:02}.pak"), opts)
            .unwrap();
        w.write_all(&vec![i as u8; 40_000 + i * 1000]).unwrap();
    }
    w.finish().unwrap();
    path
}

/// Runs the event loop until `done` holds, so worker threads can report in.
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
    // Hiding the last window also asks the loop to quit, so a stale quit can
    // end a run early; go round again until the condition or the deadline.
    while !ok.get() && start.elapsed() <= Duration::from_secs(30) {
        slint::run_event_loop().unwrap();
    }
    assert!(ok.get(), "timed out waiting for {what}");
}

fn shot(window: &slint::Window, name: &str) {
    let Some(dir) = std::env::var_os("HATCHZIP_GUI_SHOTS") else {
        return;
    };
    // Let state changes finish animating, as a person would see them.
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

fn main_window_flow(scheme: ColorScheme, tag: &str) {
    let dir = tempfile::tempdir().unwrap();
    let archive = make_zip(dir.path());
    let drops = Drops::default();
    let settings_file = dir.path().join("config/settings.json");
    let ui = main_window::build(
        None,
        &drops,
        Settings::default(),
        Some(settings_file.clone()),
    )
    .unwrap();
    ui.global::<Palette>().set_color_scheme(scheme);
    ui.window().set_size(LogicalSize::new(980.0, 660.0));
    assert_eq!(ui.get_screen(), Screen::Empty);
    assert_eq!(ui.get_keep_free(), "2");
    shot(ui.window(), &format!("{tag}-1-empty"));

    // A file dropped on the window opens it.
    (drops.on_drop.borrow().as_ref().unwrap())(archive.clone());
    let w = ui.as_weak();
    wait_until("the archive to open", move || {
        let ui = w.unwrap();
        ui.get_screen() == Screen::Opened && ui.get_plan().ready
    });
    assert!(ui.get_plan().fits, "{:?}", ui.get_plan().verdict);
    assert!(
        ui.get_summary().contains("35 entries"),
        "{}",
        ui.get_summary()
    );
    assert_eq!(
        ui.get_output(),
        dir.path().join("northwind").display().to_string()
    );
    shot(ui.window(), &format!("{tag}-2-opened"));

    // The tree accepts arrows immediately after load, without a mouse click.
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::LeftArrow.into(),
    });
    assert_eq!(
        ui.get_entries()
            .iter()
            .map(|r| r.path.to_string())
            .collect::<Vec<_>>(),
        ["Northwind"]
    );
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::RightArrow.into(),
    });
    assert!(ui.get_entries().iter().any(|r| r.path == "Northwind/bin"));
    // Click a folder and navigate the same tree by keyboard.
    let window = ui.window();
    let folder = ElementHandle::find_by_accessible_label(&ui, "Northwind/bin")
        .next()
        .expect("the bin folder is visible");
    assert_eq!(folder.accessible_description().unwrap(), "Folder");
    assert_eq!(folder.accessible_expandable(), Some(true));
    assert_eq!(folder.accessible_expanded(), Some(false));
    assert_eq!(folder.accessible_item_selectable(), Some(true));
    assert_eq!(folder.accessible_item_selected(), Some(false));
    // Assistive technology can open and close a folder without a pointer.
    folder.invoke_accessible_expand_action();
    assert_eq!(folder.accessible_expanded(), Some(true));
    assert_eq!(folder.accessible_item_selected(), Some(true));
    assert!(
        ui.get_entries()
            .iter()
            .any(|r| r.path == "Northwind/bin/northwind")
    );
    folder.invoke_accessible_default_action();
    assert_eq!(folder.accessible_expanded(), Some(false));
    assert!(
        !ui.get_entries()
            .iter()
            .any(|r| r.path == "Northwind/bin/northwind")
    );
    let origin = folder.absolute_position();
    let size = folder.size();
    let pos =
        slint::LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0);
    window.dispatch_event(WindowEvent::PointerMoved { position: pos });
    window.dispatch_event(WindowEvent::PointerPressed {
        position: pos,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: pos,
        button: PointerEventButton::Left,
    });
    assert!(
        ui.get_entries()
            .iter()
            .any(|r| r.path == "Northwind/bin/northwind")
    );
    assert_eq!(folder.accessible_expanded(), Some(true));
    let file = ElementHandle::find_by_accessible_label(&ui, "Northwind/bin/northwind")
        .next()
        .expect("the file in the expanded folder is visible");
    assert_eq!(file.accessible_description().unwrap(), "File");
    assert_eq!(file.accessible_expandable(), Some(false));
    window.dispatch_event(WindowEvent::KeyPressed {
        text: Key::LeftArrow.into(),
    });
    assert!(
        !ui.get_entries()
            .iter()
            .any(|r| r.path == "Northwind/bin/northwind")
    );
    window.dispatch_event(WindowEvent::KeyPressed {
        text: Key::RightArrow.into(),
    });
    assert!(
        ui.get_entries()
            .iter()
            .any(|r| r.path == "Northwind/bin/northwind")
    );
    shot(ui.window(), &format!("{tag}-2-tree-expanded"));
    window.dispatch_event(WindowEvent::KeyPressed {
        text: Key::DownArrow.into(),
    });
    window.dispatch_event(WindowEvent::KeyPressed {
        text: Key::DownArrow.into(),
    });
    window.dispatch_event(WindowEvent::KeyPressed {
        text: Key::RightArrow.into(),
    });
    assert!(
        ui.get_entries()
            .iter()
            .any(|r| r.path == "Northwind/data/level29.pak")
    );
    for _ in 0..29 {
        window.dispatch_event(WindowEvent::KeyPressed {
            text: Key::DownArrow.into(),
        });
    }
    shot(ui.window(), &format!("{tag}-2-tree-scrolled"));

    // Asking to keep 100 TB free makes it not fit, and says by how much.
    ui.set_keep_free("100000".into());
    ui.invoke_keep_free_edited("100000".into());
    let w = ui.as_weak();
    wait_until("the plan to say no", move || !w.unwrap().get_plan().fits);
    assert!(
        ui.get_plan()
            .verdict
            .starts_with("Insufficient disk space. ")
    );
    shot(ui.window(), &format!("{tag}-3-no-fit"));

    ui.invoke_keep_free_edited("not a number".into());
    assert!(!ui.get_keep_free_valid());
    ui.set_keep_free("0".into());
    ui.invoke_keep_free_edited("0".into());
    assert!(ui.get_keep_free_valid());
    let w = ui.as_weak();
    wait_until("the plan to fit again", move || w.unwrap().get_plan().fits);
    // Remembered for next time, in the file this window was given.
    wait_until("the settings to be saved", move || {
        Settings::load_from(&settings_file).keep_free == 0
    });

    ui.invoke_test();
    let w = ui.as_weak();
    wait_until("the test", move || {
        w.unwrap().get_task().phase == Phase::Done
    });
    assert!(ui.get_task().message == "Test passed: 35 entries verified.");
    shot(ui.window(), &format!("{tag}-4-tested"));
    ui.invoke_back();
    assert_eq!(ui.get_task().phase, Phase::Idle);

    ui.set_verify_first(true);
    ui.invoke_verify_first_toggled();
    ui.invoke_extract();
    let w = ui.as_weak();
    wait_until("the extraction", move || {
        w.unwrap().get_task().phase == Phase::Done
    });
    let out = dir.path().join("northwind");
    assert!(!archive.exists(), "the archive is gone once extracted");
    assert_eq!(
        std::fs::read(out.join("Northwind/data/level03.pak")).unwrap(),
        vec![3u8; 43_000]
    );
    assert_eq!(
        ui.get_task().message,
        "All entries were extracted. The archive was removed."
    );
    assert_eq!(ui.get_task().heading, "Extraction complete");
    assert_eq!(
        ui.get_task().detail,
        format!("Destination: {}", out.display())
    );
    shot(ui.window(), &format!("{tag}-5-done"));

    ui.invoke_about();
    let about = main_window::about_window().expect("About opens from the menu");
    about.global::<Palette>().set_color_scheme(scheme);
    about.window().set_size(LogicalSize::new(420.0, 470.0));
    shot(about.window(), &format!("{tag}-6-about"));
    about.hide().unwrap();
    ui.hide().unwrap();
}

fn one(kind: TaskKind, archive: &Path) -> TaskArgs {
    TaskArgs {
        kind,
        archives: vec![archive.to_path_buf()],
        keep_archive: false,
    }
}

/// The small window stays open when it is done, so a flow can read and
/// capture the finished state.
fn stay_open() -> Settings {
    Settings {
        close_when_done: false,
        ..Settings::default()
    }
}

/// "Extract here" on an archive of loose files puts them in a folder named
/// after it rather than among whatever is next to the archive. One folder at
/// the top goes next to the archive, as `keep_task_flow` checks.
fn here_flow() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("loose.zip");
    let mut w = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    for name in ["readme.txt", "data/level.pak"] {
        w.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(name.as_bytes()).unwrap();
    }
    w.finish().unwrap();
    let (ui, start) =
        task_window::build(one(TaskKind::ExtractHere, &archive), stay_open()).unwrap();
    start();
    let w = ui.as_weak();
    wait_until("the extraction", move || {
        w.unwrap().get_task().phase == Phase::Done
    });
    assert!(dir.path().join("loose/readme.txt").exists());
    assert!(dir.path().join("loose/data/level.pak").exists());
    assert!(!dir.path().join("readme.txt").exists());
    assert_eq!(
        ui.get_task().detail,
        format!("Destination: {}", dir.path().join("loose").display())
    );
    ui.hide().unwrap();

    // "Extract files…" goes exactly where the person pointed the picker.
    let archive = make_zip(dir.path());
    let chosen = dir.path().join("elsewhere");
    std::fs::create_dir(&chosen).unwrap();
    let args = one(TaskKind::ExtractFiles(Some(chosen.clone())), &archive);
    let (ui, start) = task_window::build(args, stay_open()).unwrap();
    start();
    let w = ui.as_weak();
    wait_until("the extraction", move || {
        w.unwrap().get_task().phase == Phase::Done
    });
    assert!(chosen.join("Northwind/README.txt").exists());
    assert!(!archive.exists());
    ui.hide().unwrap();
    // Without a folder there is nothing to run.
    assert!(task_window::build(one(TaskKind::ExtractFiles(None), &archive), stay_open()).is_err());
}

/// By default the small window closes itself once everything is extracted,
/// as other extractors' do, but a test's result stays on screen.
fn close_flow() {
    let dir = tempfile::tempdir().unwrap();
    let archive = make_zip(dir.path());
    let (ui, start) =
        task_window::build(one(TaskKind::Test, &archive), Settings::default()).unwrap();
    start();
    let w = ui.as_weak();
    wait_until("the test", move || {
        w.unwrap().get_task().phase == Phase::Done
    });
    assert!(ui.window().is_visible(), "a test's result stays on screen");
    ui.hide().unwrap();

    let (ui, start) =
        task_window::build(one(TaskKind::ExtractTo, &archive), Settings::default()).unwrap();
    assert!(ui.window().is_visible());
    start();
    let w = ui.as_weak();
    wait_until("the extraction", move || {
        w.unwrap().get_task().phase == Phase::Done
    });
    assert!(!ui.window().is_visible(), "the window closed itself");
    assert!(dir.path().join("northwind/Northwind/README.txt").exists());
}

/// With "Remove the archive during extraction" cleared in the app, a plain
/// "Extract here" keeps the archive too, as other extractors always do.
fn keep_setting_flow() {
    let dir = tempfile::tempdir().unwrap();
    let archive = make_zip(dir.path());
    let before = std::fs::read(&archive).unwrap();
    let settings = Settings {
        keep_archive: true,
        ..stay_open()
    };
    let (ui, start) = task_window::build(one(TaskKind::ExtractHere, &archive), settings).unwrap();
    start();
    let w = ui.as_weak();
    wait_until("the extraction", move || {
        w.unwrap().get_task().phase == Phase::Done
    });
    assert_eq!(
        ui.get_task().message,
        "All entries were extracted. The archive was kept."
    );
    assert_eq!(std::fs::read(&archive).unwrap(), before);
    assert!(dir.path().join("Northwind/README.txt").exists());
    ui.hide().unwrap();
}

/// Unticking "Remove the archive during extraction" plans for every file,
/// remembers the choice, and extracts without touching the archive.
fn keep_flow(scheme: ColorScheme, tag: &str) {
    let dir = tempfile::tempdir().unwrap();
    let archive = make_zip(dir.path());
    let before = std::fs::read(&archive).unwrap();
    let settings_file = dir.path().join("config/settings.json");
    let drops = Drops::default();
    let ui = main_window::build(
        Some(archive.clone()),
        &drops,
        Settings {
            keep_free: 0,
            ..Settings::default()
        },
        Some(settings_file.clone()),
    )
    .unwrap();
    ui.global::<Palette>().set_color_scheme(scheme);
    ui.window().set_size(LogicalSize::new(980.0, 660.0));
    let w = ui.as_weak();
    wait_until("the archive to open", move || {
        let ui = w.unwrap();
        ui.get_screen() == Screen::Opened && ui.get_plan().ready
    });
    assert!(ui.get_remove_archive(), "removing is the default");
    assert!(!ui.get_plan().keep_archive);

    ui.set_remove_archive(false);
    ui.invoke_remove_archive_toggled();
    let w = ui.as_weak();
    wait_until("the plan for keeping", move || {
        w.unwrap().get_plan().keep_archive
    });
    let plan = ui.get_plan();
    assert!(plan.fits, "{}", plan.verdict);
    assert_eq!(plan.needed, plan.normal, "keeping needs every file");
    assert_eq!(plan.verdict, "Sufficient disk space");
    shot(ui.window(), &format!("{tag}-17-keep-opened"));
    wait_until("the choice to be saved", move || {
        Settings::load_from(&settings_file).keep_archive
    });

    ui.invoke_extract();
    let w = ui.as_weak();
    wait_until("the extraction", move || {
        w.unwrap().get_task().phase == Phase::Done
    });
    assert_eq!(
        ui.get_task().message,
        "All entries were extracted. The archive was kept."
    );
    assert_eq!(std::fs::read(&archive).unwrap(), before);
    assert_eq!(
        std::fs::read(dir.path().join("northwind/Northwind/data/level07.pak")).unwrap(),
        vec![7u8; 47_000]
    );
    assert!(!session::locate(&archive).unwrap().1.exists());
    shot(ui.window(), &format!("{tag}-18-keep-done"));
    ui.hide().unwrap();
}

/// "Extract here (keep archive)" from the right-click menu.
fn keep_task_flow(scheme: ColorScheme, tag: &str) {
    let dir = tempfile::tempdir().unwrap();
    let archive = make_zip(dir.path());
    let before = std::fs::read(&archive).unwrap();
    let args = TaskArgs {
        keep_archive: true,
        ..one(TaskKind::ExtractHere, &archive)
    };
    let (ui, start) = task_window::build(args, stay_open()).unwrap();
    ui.global::<Palette>().set_color_scheme(scheme);
    ui.window().set_size(LogicalSize::new(480.0, 250.0));
    start();
    let w = ui.as_weak();
    wait_until("the task", move || {
        w.unwrap().get_task().phase == Phase::Done
    });
    assert_eq!(
        ui.get_task().message,
        "All entries were extracted. The archive was kept."
    );
    assert_eq!(std::fs::read(&archive).unwrap(), before);
    assert!(dir.path().join("Northwind/README.txt").exists());
    shot(ui.window(), &format!("{tag}-19-task-keep-done"));
    ui.hide().unwrap();
}

/// Several archives from one right-click run in turn; one that fails stops
/// the rest and says how many never started.
fn queue_flow(scheme: ColorScheme, tag: &str) {
    let dir = tempfile::tempdir().unwrap();
    let names = ["Northwind-1.zip", "Northwind-2.zip", "Northwind-3.zip"];
    let archives: Vec<PathBuf> = names
        .iter()
        .map(|n| {
            let p = dir.path().join(n);
            std::fs::rename(make_zip(dir.path()), &p).unwrap();
            p
        })
        .collect();
    let args = TaskArgs {
        kind: TaskKind::ExtractTo,
        archives: archives.clone(),
        keep_archive: false,
    };
    let (ui, start) = task_window::build(args, stay_open()).unwrap();
    ui.global::<Palette>().set_color_scheme(scheme);
    ui.window().set_size(LogicalSize::new(480.0, 270.0));
    start();
    let w = ui.as_weak();
    wait_until("every archive", move || {
        w.unwrap().get_task().step == "All 3 archives completed."
    });
    assert_eq!(ui.get_task().phase, Phase::Done);
    for (archive, folder) in archives
        .iter()
        .zip(["Northwind-1", "Northwind-2", "Northwind-3"])
    {
        assert!(!archive.exists());
        assert!(
            dir.path()
                .join(folder)
                .join("Northwind/README.txt")
                .exists()
        );
    }
    shot(ui.window(), &format!("{tag}-20-task-queue-done"));
    ui.hide().unwrap();

    // The second can't go where it should: a file is in the way, and the
    // person declines to replace it.
    let dir = tempfile::tempdir().unwrap();
    let archives: Vec<PathBuf> = names
        .iter()
        .map(|n| {
            let p = dir.path().join(n);
            std::fs::rename(make_zip(dir.path()), &p).unwrap();
            p
        })
        .collect();
    let blocker = dir.path().join("Northwind-2/Northwind/README.txt");
    std::fs::create_dir_all(blocker.parent().unwrap()).unwrap();
    std::fs::write(&blocker, b"mine").unwrap();
    let args = TaskArgs {
        kind: TaskKind::ExtractTo,
        archives: archives.clone(),
        keep_archive: false,
    };
    let (ui, start) = task_window::build(args, Settings::default()).unwrap();
    ui.global::<Palette>().set_color_scheme(scheme);
    ui.window().set_size(LogicalSize::new(480.0, 270.0));
    start();
    let w = ui.as_weak();
    wait_until("the second to ask", move || w.unwrap().get_replace_shown());
    ui.invoke_replace_cancel();
    let w = ui.as_weak();
    wait_until("the second to fail", move || {
        w.unwrap().get_task().phase == Phase::Failed
    });
    assert_eq!(
        ui.get_task().step,
        "Archive 2 of 3. 1 more archive has not started."
    );
    assert_eq!(ui.get_task().heading, "Extraction failed");
    assert!(ui.window().is_visible(), "a failure stays on screen");
    assert!(!archives[0].exists());
    assert!(archives[1].exists() && archives[2].exists());
    assert!(!dir.path().join("Northwind-3").exists());
    assert_eq!(std::fs::read(&blocker).unwrap(), b"mine");
    shot(ui.window(), &format!("{tag}-21-task-queue-stopped"));
    ui.hide().unwrap();
}

fn task_window_flow(scheme: ColorScheme, tag: &str) {
    let dir = tempfile::tempdir().unwrap();
    let archive = make_zip(dir.path());
    let (ui, start) = task_window::build(one(TaskKind::ExtractTo, &archive), stay_open()).unwrap();
    ui.global::<Palette>().set_color_scheme(scheme);
    ui.window().set_size(LogicalSize::new(480.0, 250.0));
    // Nothing happens before the window says go.
    assert_eq!(ui.get_task().phase, Phase::Checking);
    assert!(archive.exists());
    start();
    let w = ui.as_weak();
    wait_until("the task", move || {
        w.unwrap().get_task().phase == Phase::Done
    });
    assert!(dir.path().join("northwind/Northwind/README.txt").exists());
    assert!(!archive.exists());
    assert!(
        ui.get_task().space.starts_with("Required: "),
        "the plan stays in view: {}",
        ui.get_task().space
    );
    shot(ui.window(), &format!("{tag}-7-task-done"));
    ui.hide().unwrap();

    // A right-click test of an encrypted archive asks in the small window.
    let locked = tempfile::tempdir().unwrap();
    let archive = make_zip_with(locked.path(), Some(PASSWORD));
    let (ui, start) =
        task_window::build(one(TaskKind::Test, &archive), Settings::default()).unwrap();
    ui.global::<Palette>().set_color_scheme(scheme);
    ui.window().set_size(LogicalSize::new(480.0, 250.0));
    start();
    let w = ui.as_weak();
    wait_until("the task's prompt", move || w.unwrap().get_password_shown());
    shot(ui.window(), &format!("{tag}-14-task-password"));
    ui.invoke_password_cancel();
    let w = ui.as_weak();
    wait_until("the task to give up", move || {
        w.unwrap().get_task().phase == Phase::Failed
    });
    assert_eq!(
        ui.get_task().message,
        "Test cancelled. The archive was not modified."
    );
    ui.hide().unwrap();
}

/// A right-click extraction over files already there asks about each one
/// until told to do the same for the rest, and touches nothing until every
/// answer is in. Skip keeps the existing file, and keep both adds the
/// archive's copy beside it.
fn replace_flow(scheme: ColorScheme, tag: &str) {
    let dir = tempfile::tempdir().unwrap();
    let archive = make_zip(dir.path());
    let out = dir.path().join("northwind");
    let names = [
        "Northwind/README.txt",
        "Northwind/bin/northwind",
        "Northwind/data/level00.pak",
    ];
    for name in names {
        let path = out.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "old").unwrap();
    }

    for replace in [false, true] {
        let (ui, start) =
            task_window::build(one(TaskKind::ExtractTo, &archive), stay_open()).unwrap();
        ui.global::<Palette>().set_color_scheme(scheme);
        ui.window().set_size(LogicalSize::new(480.0, 270.0));
        start();
        let w = ui.as_weak();
        wait_until("the replace prompt", move || w.unwrap().get_replace_shown());
        let first = ui.get_replace_text().to_string();
        assert!(
            first.ends_with(&format!("' already exists in {}.", out.display())),
            "{first}"
        );
        assert_eq!(ui.get_replace_rest(), "Do the same for the next 2 files");
        assert!(
            ui.get_replace_note()
                .contains("the archive's copy is not kept")
        );
        if replace {
            // Skip the first, then keep both of the other two at once.
            ui.invoke_replace_answer(1, false);
            let w = ui.as_weak();
            let asked = first.clone();
            wait_until("the second question", move || {
                let ui = w.unwrap();
                ui.get_replace_shown() && ui.get_replace_text() != asked.as_str()
            });
            assert_eq!(ui.get_replace_rest(), "Do the same for the next file");
            ui.invoke_replace_answer(2, true);
            let w = ui.as_weak();
            wait_until("the task", move || {
                w.unwrap().get_task().phase == Phase::Done
            });
            for name in names {
                let path = out.join(name);
                assert_eq!(std::fs::read(&path).unwrap(), b"old", "{name}");
                let mut beside = path.file_stem().unwrap().to_os_string();
                beside.push(" (2)");
                if let Some(ext) = path.extension() {
                    beside.push(".");
                    beside.push(ext);
                }
                let beside = path.with_file_name(beside);
                // Windows shows the path with its own separator.
                let skipped = first.replace('\\', "/").contains(&format!("'{name}'"));
                assert_eq!(beside.exists(), !skipped, "{name}");
            }
            assert!(!archive.exists());
            assert!(!out.join(".hatchzip-replaced").exists());
        } else {
            shot(ui.window(), &format!("{tag}-15-task-replace"));
            ui.invoke_replace_cancel();
            let w = ui.as_weak();
            wait_until("the task to give up", move || {
                w.unwrap().get_task().phase == Phase::Failed
            });
            assert_eq!(
                ui.get_task().message,
                "Extraction cancelled. The archive was not modified."
            );
            for name in names {
                assert_eq!(std::fs::read(out.join(name)).unwrap(), b"old");
            }
            assert!(archive.exists());
        }
        ui.hide().unwrap();
    }
}

fn password_flow(scheme: ColorScheme, tag: &str) {
    let dir = tempfile::tempdir().unwrap();
    let archive = make_zip_with(dir.path(), Some(PASSWORD));
    let drops = Drops::default();
    let ui = main_window::build(Some(archive.clone()), &drops, Settings::default(), None).unwrap();
    ui.global::<Palette>().set_color_scheme(scheme);
    ui.window().set_size(LogicalSize::new(980.0, 660.0));

    let w = ui.as_weak();
    wait_until("the prompt", move || w.unwrap().get_password_shown());
    assert!(!ui.get_password_wrong());
    assert_eq!(ui.get_password_name(), "northwind.zip");
    shot(ui.window(), &format!("{tag}-11-password"));

    ui.invoke_password_ok("not it".into());
    assert!(
        !ui.get_password_shown(),
        "the prompt closes while it checks"
    );
    let w = ui.as_weak();
    wait_until("the prompt to come back", move || {
        let ui = w.unwrap();
        ui.get_password_shown() && ui.get_password_wrong()
    });
    shot(ui.window(), &format!("{tag}-12-password-wrong"));

    ui.invoke_password_ok(PASSWORD.into());
    let w = ui.as_weak();
    wait_until("the archive to open", move || {
        let ui = w.unwrap();
        ui.get_screen() == Screen::Opened && ui.get_plan().ready
    });

    // The password is remembered for this archive: testing doesn't ask.
    ui.invoke_test();
    let w = ui.as_weak();
    wait_until("the test", move || {
        let ui = w.unwrap();
        ui.get_task().phase == Phase::Done || ui.get_password_shown()
    });
    assert!(!ui.get_password_shown(), "asked twice");
    assert!(ui.get_task().message == "Test passed: 35 entries verified.");
    ui.hide().unwrap();
}

fn give_back_flow(scheme: ColorScheme, tag: &str) {
    let dir = tempfile::tempdir().unwrap();
    let archive = make_zip(dir.path());
    // Start an extraction and stop before the first batch: the journal
    // exists, so the archive is half extracted.
    let (primary, paths) = session::locate(&archive).unwrap();
    let reads = Default::default();
    let State::Fresh(job, out) = session::open(&primary, &paths, None, true, None, &reads).unwrap()
    else {
        panic!("a new archive opens fresh");
    };
    session::begin(&job, &paths, out, false, None).unwrap();
    drop(job);

    let drops = Drops::default();
    let ui = main_window::build(Some(archive.clone()), &drops, Settings::default(), None).unwrap();
    ui.global::<Palette>().set_color_scheme(scheme);
    ui.window().set_size(LogicalSize::new(980.0, 660.0));
    let w = ui.as_weak();
    wait_until("the half-extracted archive", move || {
        let ui = w.unwrap();
        ui.get_screen() == Screen::Opened && ui.get_plan().ready
    });
    assert_eq!(ui.get_mode(), hatchzip_gui::Mode::Resume);
    shot(ui.window(), &format!("{tag}-15-resume"));

    ui.invoke_give_back();
    let w = ui.as_weak();
    wait_until("the archive back", move || {
        w.unwrap().get_task().phase == Phase::Done
    });
    assert!(
        ui.get_task()
            .message
            .starts_with("Archive restored. It contains the 35 entries not yet extracted."),
        "{}",
        ui.get_task().message
    );
    assert!(!paths.exists(), "the journal is gone");
    shot(ui.window(), &format!("{tag}-16-given-back"));

    // Looking again shows a normal archive.
    ui.invoke_back();
    let w = ui.as_weak();
    wait_until("the archive to reopen", move || {
        let ui = w.unwrap();
        ui.get_screen() == Screen::Opened && ui.get_mode() == hatchzip_gui::Mode::Fresh
    });
    ui.hide().unwrap();
}

/// Only where the menu entries are plain files under `$XDG_DATA_HOME`, which
/// the test points at a temporary folder; elsewhere it would touch the real
/// registry.
fn shell_switch_flow(scheme: ColorScheme, tag: &str, data: &Path) {
    let drops = Drops::default();
    let ui = main_window::build(None, &drops, Settings::default(), None).unwrap();
    ui.global::<Palette>().set_color_scheme(scheme);
    ui.window().set_size(LogicalSize::new(980.0, 660.0));
    assert!(ui.get_shell_menu_available());

    ui.set_shell_menu_on(true);
    ui.invoke_shell_menu_toggled();
    let w = ui.as_weak();
    wait_until("the menu to be added", move || {
        w.unwrap().get_notice() != ""
    });
    assert_eq!(ui.get_notice(), "Context menu entries added.");
    assert!(!ui.get_notice_bad());
    // The startup check, answering late, didn't flip the switch back.
    assert!(ui.get_shell_menu_on());
    assert!(data.join("kio/servicemenus/hatchzip.desktop").exists());
    shot(ui.window(), &format!("{tag}-13-right-click-menu"));

    ui.set_notice("".into());
    ui.set_shell_menu_on(false);
    ui.invoke_shell_menu_toggled();
    let w = ui.as_weak();
    wait_until("the menu to be removed", move || {
        w.unwrap().get_notice() != ""
    });
    assert!(!data.join("kio/servicemenus/hatchzip.desktop").exists());
    ui.hide().unwrap();
}

/// States a real run passes through too quickly to catch, drawn from the
/// values they would hold, only for looking at.
fn task_window_states(scheme: ColorScheme, tag: &str) {
    if std::env::var_os("HATCHZIP_GUI_SHOTS").is_none() {
        return;
    }
    let ui = TaskWindow::new().unwrap();
    ui.global::<Palette>().set_color_scheme(scheme);
    ui.window().set_size(LogicalSize::new(480.0, 230.0));
    ui.show().unwrap();
    let base = TaskView {
        heading: "Extracting Northwind-1.4.7z".into(),
        detail: "Destination: /home/sam/Games/Northwind-1.4".into(),
        space: "Required: 3.1 GB. Available: 21.3 GB.".into(),
        fraction: 0.42,
        amounts: "12.6 GB of 30.0 GB".into(),
        archive_left: "Archive remaining: 17.1 GB".into(),
        ..Default::default()
    };
    ui.set_task(TaskView {
        phase: Phase::Working,
        rate: "182.4 MB/s, 2 minutes remaining".into(),
        ..base.clone()
    });
    shot(ui.window(), &format!("{tag}-8-task-working"));
    ui.set_task(TaskView {
        phase: Phase::Paused,
        heading: "Extraction paused".into(),
        message: "Progress is saved. The extraction can be resumed now or later.".into(),
        ..base.clone()
    });
    shot(ui.window(), &format!("{tag}-9-task-paused"));
    ui.set_plan(PlanView {
        ready: true,
        free: "21.3 GB".into(),
        needed: "30.4 GB".into(),
        keep: "2.0 GB".into(),
        note: "Most of this is one solid block, which can only be freed once all of it is out. \
               Archives packed with smaller blocks need far less."
            .into(),
        ..Default::default()
    });
    ui.set_task(TaskView {
        phase: Phase::NoSpace,
        heading: "Insufficient disk space".into(),
        can_retry: true,
        message: "Insufficient disk space. 11.1 GB more is required.".into(),
        ..base
    });
    shot(ui.window(), &format!("{tag}-10-task-no-space"));
    ui.hide().unwrap();
}

#[test]
fn windows_end_to_end() {
    let data = tempfile::tempdir().unwrap();
    let linux = std::env::consts::OS == "linux";
    if linux {
        // SAFETY: no other thread exists yet in this one-test binary.
        unsafe { std::env::set_var("XDG_DATA_HOME", data.path()) };
    }
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: false,
        threading: true,
        renderer_name: Some("software".into()),
    })))
    .unwrap();

    for (scheme, tag) in [(ColorScheme::Light, "light"), (ColorScheme::Dark, "dark")] {
        main_window_flow(scheme, tag);
        keep_flow(scheme, tag);
        task_window_flow(scheme, tag);
        replace_flow(scheme, tag);
        keep_task_flow(scheme, tag);
        queue_flow(scheme, tag);
        task_window_states(scheme, tag);
        password_flow(scheme, tag);
        give_back_flow(scheme, tag);
        if linux {
            shell_switch_flow(scheme, tag, data.path());
        }
    }
    close_flow();
    here_flow();
    keep_setting_flow();
}
