//! The main window: open an archive, see what is in it and whether it fits,
//! then extract or test it.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use hatchzip_core::archive::storage::Reads;
use hatchzip_core::engine::Job;
use hatchzip_core::human;
use hatchzip_core::session::{self, State, display_name};
use slint::{CloseRequestResponse, ComponentHandle, ModelRc, Timer, TimerMode};

use crate::backend::Drops;
use crate::entries::{self, Entries, Entry};
use crate::runner::Runner;
use crate::settings::{self, Settings};
use crate::task::Task;
use crate::text::{self, gb_text, parse_gb, plan_view};
use crate::work::{Request, sentence};
use crate::{AboutWindow, MainWindow, Mode, PlanView, Screen, TaskView, platform, shell};

const REPO: &str = "https://github.com/xevrion/hatchzip";
const RELEASES: &str = "https://github.com/xevrion/hatchzip/releases/latest";
/// What the Open dialog offers. `.r00` is the second volume of an old-style
/// RAR set; any volume opens the whole set.
const ARCHIVE_EXTENSIONS: &[&str] = if hatchzip_core::archive::RAR_SUPPORTED {
    &["zip", "7z", "tar", "gz", "tgz", "rar", "r00", "exe", "001"]
} else {
    &["zip", "7z", "tar", "gz", "tgz", "exe", "001"]
};

/// The Linux file choosers match a filter case-sensitively, so `GAME.RAR`
/// would be hidden by `*.rar` alone.
fn picker_extensions() -> Vec<String> {
    ARCHIVE_EXTENSIONS
        .iter()
        .flat_map(|e| [e.to_string(), e.to_uppercase()])
        .collect()
}

/// The formats line under the Open button.
const FORMATS: &str = if hatchzip_core::archive::RAR_SUPPORTED {
    "Or drag an archive onto this window. Supported formats: ZIP, 7z, tar, tar.gz, RAR, and split \
     archives."
} else {
    "Or drag an archive onto this window. Supported formats: ZIP, 7z, tar, tar.gz, and split \
     archives."
};

/// An archive opened read-only for looking at. It takes no lock, so another
/// HatchZip can still extract it; extraction reopens it for writing.
struct Loaded {
    name: String,
    summary: String,
    entries: Vec<Entry>,
    mode: Mode,
    /// Half extracted by a run that keeps the archive.
    kept: bool,
    output: PathBuf,
    job: Option<Job>,
    password: Option<String>,
}

/// Runs on a worker thread; an encrypted archive asks for its password
/// through the window.
fn load(archive: &Path, password: Option<String>, reads: &Arc<Reads>) -> Result<Loaded> {
    let (primary, paths) = session::locate(archive)?;
    let file_name = primary
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let (state, password) =
        session::open_encrypted(&primary, &paths, None, false, password, reads, |wrong| {
            crate::password::ask(&file_name, wrong)
        })?;
    let mut kept = false;
    let (job, mode, output, total, left) = match state {
        State::Fresh(job, out) => {
            let total = job.remaining_size();
            (Some(job), Mode::Fresh, out, total, None)
        }
        State::Resume(job, journal) => {
            let left = job.remaining_size();
            kept = journal.keep;
            (
                Some(job),
                Mode::Resume,
                journal.output,
                journal.total_size,
                Some(left),
            )
        }
        State::Orphan(journal) => (None, Mode::Orphan, journal.output, journal.total_size, None),
    };
    let name = match &job {
        Some(j) => display_name(j),
        None => file_name,
    };
    let mut summary = Vec::new();
    if let Some(j) = &job {
        summary.push(format!("{} archive", j.format_name.to_uppercase()));
        // Half extracted, the list only holds what is left to do.
        match left {
            None => {
                summary.push(human(j.storage.original_len()));
                summary.push(text::plural(j.item_count(), "entry", "entries"));
            }
            Some(left) => summary.push(format!(
                "{} in {} remaining",
                human(left),
                text::plural(j.item_count(), "entry", "entries")
            )),
        }
    }
    summary.push(format!("{} uncompressed", human(total)));
    Ok(Loaded {
        name,
        summary: summary.join(" \u{b7} "),
        entries: job.as_ref().map(entries::collect).unwrap_or_default(),
        mode,
        kept,
        output,
        job,
        password,
    })
}

/// Plan threads lock it while they work; the UI thread only swaps in a new
/// one, so it never waits on them.
type SharedJob = Arc<Mutex<Option<Job>>>;

struct App {
    ui: slint::Weak<MainWindow>,
    runner: Rc<Runner>,
    archive: RefCell<Option<PathBuf>>,
    name: RefCell<String>,
    mode: Cell<Mode>,
    /// The half-extracted run being resumed keeps its archive.
    kept_run: Cell<bool>,
    output: RefCell<PathBuf>,
    keep_free: Cell<u64>,
    /// Shared with the threads that work out the space plan.
    job: RefCell<SharedJob>,
    /// Bumped by every open and every plan request, so a slow answer to an
    /// old question never overwrites a newer one.
    generation: Cell<u64>,
    about: RefCell<Option<AboutWindow>>,
    /// The password that opened the current archive, for this process only.
    password: RefCell<Option<String>>,
    settings: RefCell<Settings>,
    /// Where settings are saved; `None` keeps them in memory, for tests.
    settings_file: Option<PathBuf>,
    notice_timer: Timer,
    /// Refreshes how far opening has read while the archive loads.
    loading_timer: Timer,
    /// The person changed the right-click switch, so the startup check,
    /// which may answer later, must not overwrite it.
    shell_touched: Cell<bool>,
}

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// Runs `f` on the UI thread with the app, from any thread.
fn post(f: impl FnOnce(&Rc<App>) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = APP.with(|a| a.borrow().clone()) {
            f(&app);
        }
    });
}

pub fn run(archive: Option<PathBuf>, drops: Rc<Drops>) -> Result<()> {
    let file = settings::file();
    let saved = file.as_deref().map(Settings::load_from).unwrap_or_default();
    let ui = build(archive, &drops, saved, file)?;
    // Opening only reads, so it needn't wait for the first frame like a
    // task does; this only arms the switch to software drawing.
    crate::backend::when_drawn(ui.window(), || {});
    ui.run()?;
    APP.with(|a| a.borrow_mut().take());
    Ok(())
}

/// The window with everything wired up, and `archive` opening in the
/// background. Shown but not run, so a test can drive it. Changed settings
/// are saved to `settings_file`, if there is one.
pub fn build(
    archive: Option<PathBuf>,
    drops: &Drops,
    settings: Settings,
    settings_file: Option<PathBuf>,
) -> Result<MainWindow> {
    let ui = MainWindow::new()?;
    ui.set_keep_free(gb_text(settings.keep_free).into());
    ui.set_verify_first(settings.verify_first);
    ui.set_remove_archive(!settings.keep_archive);
    ui.set_close_when_done(settings.close_when_done);
    ui.set_formats(FORMATS.into());
    ui.set_shell_menu_available(platform::SHELL_MENU);
    ui.set_shell_menu_label(platform::SHELL_MENU_LABEL.into());

    let weak = ui.as_weak();
    let runner = Runner::install(move |task: &Task| {
        if let Some(ui) = weak.upgrade() {
            ui.set_task(task.view.clone());
        }
    });
    let app = Rc::new(App {
        ui: ui.as_weak(),
        runner,
        archive: RefCell::new(None),
        name: RefCell::new(String::new()),
        mode: Cell::new(Mode::Fresh),
        kept_run: Cell::new(false),
        output: RefCell::new(PathBuf::new()),
        keep_free: Cell::new(settings.keep_free),
        job: RefCell::default(),
        generation: Cell::new(0),
        about: RefCell::new(None),
        password: RefCell::new(None),
        settings: RefCell::new(settings),
        settings_file,
        notice_timer: Timer::default(),
        loading_timer: Timer::default(),
        shell_touched: Cell::new(false),
    });
    APP.with(|a| *a.borrow_mut() = Some(app.clone()));

    wire(&ui, &app);
    crate::password::host_in!(ui);
    crate::replace::host_in!(ui);
    if platform::SHELL_MENU {
        // Reading the registry or the menu folders is disk work; the box
        // fills in a moment later.
        std::thread::spawn(|| {
            let on =
                hatchzip_shell::Dirs::current().is_ok_and(|d| hatchzip_shell::is_registered(&d));
            post(move |app| {
                if !app.shell_touched.get() {
                    app.ui().set_shell_menu_on(on);
                }
            });
        });
    }
    *drops.on_drop.borrow_mut() = Some(Box::new(|path| {
        if let Some(app) = APP.with(|a| a.borrow().clone()) {
            app.open(path);
        }
    }));
    let weak = ui.as_weak();
    *drops.on_hover.borrow_mut() = Some(Box::new(move |on| {
        if let Some(ui) = weak.upgrade() {
            ui.set_drop_hover(on);
        }
    }));

    if let Some(path) = archive {
        app.open(path);
    }
    ui.show()?;
    Ok(ui)
}

/// The About window, once it has been opened.
pub fn about_window() -> Option<AboutWindow> {
    APP.with(|a| {
        a.borrow()
            .as_ref()
            .and_then(|app| app.about.borrow().as_ref().map(|w| w.clone_strong()))
    })
}

fn wire(ui: &MainWindow, app: &Rc<App>) {
    let a = app.clone();
    ui.on_open_archive(move || a.pick_archive());
    let a = app.clone();
    ui.on_change_output(move || a.pick_output());
    let a = app.clone();
    ui.on_keep_free_edited(move |text| a.keep_free_edited(&text));
    let a = app.clone();
    ui.on_extract(move || a.extract());
    let a = app.clone();
    ui.on_test(move || a.test());
    let a = app.clone();
    ui.on_pause(move || a.runner.pause());
    let a = app.clone();
    ui.on_resume(move || a.runner.retry());
    let a = app.clone();
    ui.on_open_folder(move || a.open_folder());
    let a = app.clone();
    ui.on_back(move || a.back());
    let a = app.clone();
    ui.on_about(move || a.show_about());
    // The releases page says whether a newer version exists; the app itself
    // never connects to the network.
    ui.on_check_for_updates(|| {
        let _ = open::that_detached(RELEASES);
    });
    let a = app.clone();
    ui.on_verify_first_toggled(move || {
        let on = a.ui().get_verify_first();
        a.change_settings(|s| s.verify_first = on);
    });
    let a = app.clone();
    ui.on_remove_archive_toggled(move || {
        let keep = !a.ui().get_remove_archive();
        a.change_settings(|s| s.keep_archive = keep);
        a.refresh_plan();
    });
    let a = app.clone();
    ui.on_close_when_done_toggled(move || {
        let on = a.ui().get_close_when_done();
        a.change_settings(|s| s.close_when_done = on);
    });
    let a = app.clone();
    ui.on_shell_menu_toggled(move || a.shell_menu_toggled());
    let a = app.clone();
    ui.on_give_back(move || a.give_back());
    let a = app.clone();
    ui.on_quit(move || {
        if a.runner.may_close() {
            let _ = slint::quit_event_loop();
        }
    });
    let a = app.clone();
    ui.window().on_close_requested(move || {
        if a.runner.may_close() {
            let _ = slint::quit_event_loop();
            CloseRequestResponse::HideWindow
        } else {
            CloseRequestResponse::KeepWindowShown
        }
    });
}

impl App {
    fn ui(&self) -> MainWindow {
        self.ui.upgrade().expect("the main window outlives its app")
    }

    fn next_generation(&self) -> u64 {
        self.generation.set(self.generation.get() + 1);
        self.generation.get()
    }

    fn open(&self, path: PathBuf) {
        if self.runner.running() {
            return;
        }
        let ui = self.ui();
        let generation = self.next_generation();
        *self.runner.task.borrow_mut() = None;
        ui.set_task(TaskView::default());
        ui.set_plan(PlanView::default());
        ui.set_entries(ModelRc::default());
        ui.set_archive_name(
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .as_ref()
                .into(),
        );
        ui.set_screen(Screen::Loading);
        ui.set_loading_detail("".into());
        // Opening a compressed tar reads all of it, which takes minutes for
        // a large one; say how far it has got.
        let reads = Arc::new(Reads::default());
        let (watched, weak, start) = (reads.clone(), ui.as_weak(), Instant::now());
        self.loading_timer
            .start(TimerMode::Repeated, Duration::from_millis(100), move || {
                if let (Some(ui), Some(f)) = (weak.upgrade(), watched.fraction())
                    && f < 1.0
                    && start.elapsed() >= Duration::from_millis(500)
                {
                    ui.set_loading_detail(text::reading_text(f).into());
                }
            });
        // Opening the same archive again, after a run, needn't ask again.
        let same = self.archive.borrow().as_ref() == Some(&path);
        let password = if same {
            self.password
                .borrow()
                .clone()
                .or_else(|| self.runner.password())
        } else {
            None
        };
        *self.archive.borrow_mut() = Some(path.clone());
        *self.job.borrow_mut() = SharedJob::default();

        std::thread::spawn(move || {
            let result = load(&path, password, &reads);
            post(move |app| app.loaded(generation, result));
        });
    }

    fn loaded(&self, generation: u64, result: Result<Loaded>) {
        if generation != self.generation.get() {
            return;
        }
        self.loading_timer.stop();
        let ui = self.ui();
        ui.set_loading_detail("".into());
        let loaded = match result {
            Ok(l) => l,
            Err(e) => {
                ui.set_error_text(sentence(&format!("{e:#}")).into());
                ui.set_screen(Screen::Error);
                return;
            }
        };
        ui.set_archive_name(loaded.name.as_str().into());
        ui.set_summary(loaded.summary.into());
        ui.set_entries(ModelRc::new(Entries(loaded.entries)));
        ui.set_mode(loaded.mode);
        ui.set_kept_run(loaded.kept);
        ui.set_output(loaded.output.display().to_string().into());
        ui.set_screen(Screen::Opened);
        *self.name.borrow_mut() = loaded.name;
        *self.password.borrow_mut() = loaded.password;
        self.mode.set(loaded.mode);
        self.kept_run.set(loaded.kept);
        *self.output.borrow_mut() = loaded.output;
        *self.job.borrow_mut() = Arc::new(Mutex::new(loaded.job));
        self.refresh_plan();
    }

    fn refresh_plan(&self) {
        let ui = self.ui();
        if self.mode.get() == Mode::Orphan {
            ui.set_plan(PlanView {
                ready: true,
                fits: true,
                verdict: "No entries remaining".into(),
                ..Default::default()
            });
            return;
        }
        let generation = self.next_generation();
        let job = self.job.borrow().clone();
        let out = self.output.borrow().clone();
        let resuming = self.mode.get() == Mode::Resume;
        let keep = self.keeps_archive();
        let keep_free = self.keep_free.get();
        std::thread::spawn(move || {
            let job = job.lock().expect("plan thread panicked");
            let Some(job) = job.as_ref() else { return };
            let refusal = job.in_place_refusal().filter(|_| !keep);
            let view = match session::plan(job, &out, resuming, keep_free, keep) {
                Ok(plan) => match refusal {
                    // Such as a solid RAR: it can only be extracted kept.
                    Some(why) => PlanView {
                        fits: false,
                        verdict: sentence(&format!(
                            "{why}. Turn off \"Remove the archive during extraction\" to \
                             extract it and keep the archive"
                        ))
                        .into(),
                        ..plan_view(&plan)
                    },
                    None => plan_view(&plan),
                },
                Err(e) => PlanView {
                    ready: true,
                    verdict: sentence(&format!("{e:#}")).into(),
                    ..Default::default()
                },
            };
            post(move |app| {
                if generation == app.generation.get() {
                    app.ui().set_plan(view);
                }
            });
        });
    }

    /// A fresh archive goes the way the box says; a half-extracted one the
    /// way it started.
    fn keeps_archive(&self) -> bool {
        match self.mode.get() {
            Mode::Fresh => !self.ui().get_remove_archive(),
            _ => self.kept_run.get(),
        }
    }

    fn keep_free_edited(&self, text: &str) {
        let ui = self.ui();
        match parse_gb(text) {
            Some(bytes) => {
                ui.set_keep_free_valid(true);
                self.keep_free.set(bytes);
                self.change_settings(|s| s.keep_free = bytes);
                self.refresh_plan();
            }
            None => ui.set_keep_free_valid(false),
        }
    }

    fn change_settings(&self, f: impl FnOnce(&mut Settings)) {
        let mut settings = self.settings.borrow_mut();
        f(&mut settings);
        if let Some(file) = &self.settings_file {
            settings.save(file);
        }
    }

    fn shell_menu_toggled(&self) {
        self.shell_touched.set(true);
        let on = self.ui().get_shell_menu_on();
        std::thread::spawn(move || {
            let result = shell::set_menu(on);
            post(move |app| {
                let (text, bad) = match result {
                    Ok(()) if on => ("Context menu entries added.".to_string(), false),
                    Ok(()) => ("Context menu entries removed.".to_string(), false),
                    Err(e) => {
                        // Show the box as it really is.
                        app.ui().set_shell_menu_on(!on);
                        (sentence(&format!("{e:#}")), true)
                    }
                };
                app.notify(&text, bad);
            });
        });
    }

    /// A short message at the bottom of the window that goes away by itself.
    fn notify(&self, text: &str, bad: bool) {
        let ui = self.ui();
        ui.set_notice(text.into());
        ui.set_notice_bad(bad);
        let weak = ui.as_weak();
        self.notice_timer
            .start(TimerMode::SingleShot, Duration::from_secs(6), move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_notice("".into());
                }
            });
    }

    fn pick_archive(&self) {
        if self.runner.running() {
            return;
        }
        let ui = self.ui();
        let mut dialog = rfd::AsyncFileDialog::new()
            .set_title("Open archive")
            .add_filter("Archives", &picker_extensions())
            .add_filter("All files", &["*"])
            .set_parent(&ui.window().window_handle());
        if let Some(dir) = self.archive.borrow().as_ref().and_then(|a| a.parent()) {
            dialog = dialog.set_directory(dir);
        }
        let _ = slint::spawn_local(async move {
            if let Some(file) = dialog.pick_file().await
                && let Some(app) = APP.with(|a| a.borrow().clone())
            {
                app.open(file.path().to_path_buf());
            }
        });
    }

    fn pick_output(&self) {
        let ui = self.ui();
        let current = self.output.borrow().clone();
        // The default output doesn't exist yet; its parent always does, and
        // looking that up needs no disk access on this thread.
        let start = current.parent();
        let mut dialog = rfd::AsyncFileDialog::new()
            .set_title("Select destination folder")
            .set_parent(&ui.window().window_handle());
        if let Some(dir) = start {
            dialog = dialog.set_directory(dir);
        }
        let _ = slint::spawn_local(async move {
            if let Some(dir) = dialog.pick_folder().await
                && let Some(app) = APP.with(|a| a.borrow().clone())
            {
                app.set_output(dir.path().to_path_buf());
            }
        });
    }

    fn set_output(&self, dir: PathBuf) {
        self.ui().set_output(dir.display().to_string().into());
        *self.output.borrow_mut() = dir;
        self.refresh_plan();
    }

    fn extract(&self) {
        let Some(archive) = self.archive.borrow().clone() else {
            return;
        };
        let ui = self.ui();
        let output = self.output.borrow().clone();
        let keep = self.keeps_archive();
        let request = Request::Extract {
            archive,
            // A resume must go where it started; the journal knows where.
            output: (self.mode.get() == Mode::Fresh).then(|| output.clone()),
            keep_free: self.keep_free.get(),
            verify_first: ui.get_verify_first(),
            keep_archive: keep,
            password: self.password.borrow().clone(),
            here: false,
        };
        // On Windows our read handle would block the exclusive one the
        // extraction takes, so the worker closes it first, waiting out any
        // plan still being worked out.
        let job = self.job.replace(SharedJob::default());
        self.runner.start_after(
            move || drop(job.lock().map(|mut j| j.take())),
            Task::extract(&self.name.borrow(), Some(&output), keep),
            request,
        );
    }

    /// Stops a half-done extraction for good and makes the archive whole.
    fn give_back(&self) {
        if self.runner.running() {
            return;
        }
        let Some(archive) = self.archive.borrow().clone() else {
            return;
        };
        // Restore writes to the archive, so our read handle goes first, as
        // for an extraction.
        let job = self.job.replace(SharedJob::default());
        self.runner.start_after(
            move || drop(job.lock().map(|mut j| j.take())),
            Task::restore(&self.name.borrow()),
            Request::Restore { archive },
        );
    }

    fn test(&self) {
        let Some(archive) = self.archive.borrow().clone() else {
            return;
        };
        let request = Request::Test {
            archive,
            password: self.password.borrow().clone(),
        };
        self.runner.start(Task::test(&self.name.borrow()), request);
    }

    fn back(&self) {
        let was_test = self
            .runner
            .task
            .borrow()
            .as_ref()
            .is_some_and(|t| t.view.is_test);
        // A test only reads, so the archive is as it was.
        if was_test {
            *self.runner.task.borrow_mut() = None;
            self.ui().set_task(TaskView::default());
            return;
        }
        // Anything else may have changed the archive: look again. The path
        // is copied out first, since opening replaces it.
        let path = self.archive.borrow().clone();
        if let Some(path) = path {
            self.open(path);
        }
    }

    fn open_folder(&self) {
        let task = self.runner.task.borrow();
        if let Some(out) = task.as_ref().and_then(|t| t.output.clone()) {
            let _ = open::that_detached(out);
        }
    }

    fn show_about(&self) {
        let mut about = self.about.borrow_mut();
        if about.is_none() {
            let Ok(w) = AboutWindow::new() else { return };
            w.set_version(env!("CARGO_PKG_VERSION").into());
            w.on_open_link(|| {
                let _ = open::that_detached(REPO);
            });
            let weak = w.as_weak();
            w.on_dismiss(move || {
                if let Some(w) = weak.upgrade() {
                    let _ = w.hide();
                }
            });
            *about = Some(w);
        }
        if let Some(w) = about.as_ref() {
            let _ = w.show();
        }
    }
}
