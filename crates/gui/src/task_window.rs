//! The small window behind "Extract here", "Extract to a folder", "Extract
//! files" and "Test". The right-click was the confirmation, so it starts as
//! soon as it shows, unless the space check says no. Several archives run one
//! after another, each checked just before it starts; one that doesn't
//! finish stops the rest until it is resumed.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::Result;
use hatchzip_core::session;
use slint::{CloseRequestResponse, ComponentHandle};

use crate::args::{TaskArgs, TaskKind};
use crate::runner::Runner;
use crate::settings::{AfterExtracting, Appearance, Existing, Settings};
use crate::task::Task;
use crate::work::{Request, Written};
use crate::{Messages, Palette, TaskWindow};

/// Where each kind of task puts the files. `None` for a test, and for files
/// whose folder is not chosen yet. "Extract to a folder" honours the default
/// destination in Settings; "Extract here" says where, so it doesn't.
pub fn output_for(kind: &TaskKind, primary: &Path, settings: &Settings) -> Option<PathBuf> {
    match kind {
        TaskKind::ExtractHere => Some(primary.parent().unwrap_or(Path::new(".")).to_path_buf()),
        TaskKind::ExtractTo => Some(settings.output_for(&session::default_output(primary))),
        TaskKind::ExtractFiles(folder) => folder.clone(),
        TaskKind::Test => None,
    }
}

pub fn run(mut args: TaskArgs) -> Result<()> {
    // The window exists before the picker so its title comes from the same
    // translated messages as the rest, and the picker still runs on the
    // thread that will show it.
    let settings = Settings::saved();
    let ui = TaskWindow::new()?;
    crate::locale::apply(settings.language.as_deref());
    if args.kind == TaskKind::ExtractFiles(None) {
        let title = ui.global::<Messages>().invoke_destination_title();
        let mut dialog = rfd::FileDialog::new().set_title(title.as_str());
        if let Some(dir) = args
            .archives
            .first()
            .and_then(|a| std::path::absolute(a).ok())
            .and_then(|a| a.parent().map(Path::to_path_buf))
        {
            dialog = dialog.set_directory(dir);
        }
        // Closing the picker is how to change one's mind; nothing was read.
        let Some(folder) = dialog.pick_folder() else {
            return Ok(());
        };
        args.kind = TaskKind::ExtractFiles(Some(folder));
    }
    let (ui, start) = build_in(ui, args, settings)?;
    crate::backend::when_drawn(ui.window(), start);
    ui.run()?;
    Ok(())
}

/// The task and request for one archive. The main window's reserved space
/// and verification settings apply. The archive is kept when the menu entry
/// says so or when the main window's setting does, so someone who never
/// wants archives removed gets that from every entry.
pub(crate) fn job_for(
    kind: &TaskKind,
    archive: &Path,
    keep_archive: bool,
    settings: &Settings,
) -> (Task, Request) {
    // Only resolves the path; a missing file is reported by the run itself.
    let primary = session::locate(archive).map_or_else(|_| archive.to_path_buf(), |(p, _)| p);
    let name = primary
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let archive = archive.to_path_buf();
    match output_for(kind, &primary, settings) {
        Some(out) => (
            Task::extract(&name, Some(&out), keep_archive),
            Request::Extract {
                archive,
                output: Some(out),
                keep_free: settings.keep_free,
                verify_first: settings.verify_first,
                keep_archive,
                password: None,
                here: *kind == TaskKind::ExtractHere,
                existing: settings.existing_files,
                only: None,
                nested: None,
            },
        ),
        None => (
            Task::test(&name),
            Request::Test {
                archive,
                password: None,
            },
        ),
    }
}

/// A nested archive found inside a finished extraction: next to itself in
/// its own folder, asking about existing files, and not scanned again.
/// `written` is what the run that found it wrote, which the job checks the
/// archive against again before it starts.
pub(crate) fn nested_job(
    archive: &Path,
    written: Written,
    keep_archive: bool,
    keep_free: u64,
    verify_first: bool,
) -> (Task, Request) {
    let primary = session::locate(archive).map_or_else(|_| archive.to_path_buf(), |(p, _)| p);
    let name = primary
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let out = session::default_output(&primary);
    (
        Task::extract(&name, Some(&out), keep_archive),
        Request::Extract {
            archive: archive.to_path_buf(),
            output: Some(out),
            keep_free,
            verify_first,
            keep_archive,
            password: None,
            here: false,
            existing: Existing::Ask,
            only: None,
            nested: Some(written),
        },
    )
}

/// The window, shown, and the function that starts the task. Nothing
/// touches an archive until that is called.
pub fn build(args: TaskArgs, settings: Settings) -> Result<(TaskWindow, impl FnOnce() + 'static)> {
    let ui = TaskWindow::new()?;
    crate::locale::apply(settings.language.as_deref());
    build_in(ui, args, settings)
}

/// Fills a window made outside, so `run` can reach the picker's title through
/// it before the folder is known.
fn build_in(
    ui: TaskWindow,
    args: TaskArgs,
    settings: Settings,
) -> Result<(TaskWindow, impl FnOnce() + 'static)> {
    if args.kind == TaskKind::ExtractFiles(None) {
        anyhow::bail!("no destination folder was chosen");
    }
    crate::locale::style(ui.global::<crate::Theme>());
    if settings.appearance != Appearance::System {
        ui.global::<Palette>()
            .set_color_scheme(crate::settings_window::color_scheme(settings.appearance));
    }
    let jobs: Vec<(Task, Request)> = args
        .archives
        .iter()
        .map(|a| {
            let keep = args.keep_archive || settings.keep_archive;
            job_for(&args.kind, a, keep, &settings)
        })
        .collect();
    let Some((task, _)) = jobs.first() else {
        anyhow::bail!("no archive given");
    };

    let weak = ui.as_weak();
    let runner = Runner::install(move |task: &Task| {
        if let Some(ui) = weak.upgrade() {
            ui.set_task(crate::text::localize(task, &ui.global::<Messages>()));
            ui.set_plan(task.plan.clone());
        }
    });
    ui.set_task(crate::text::localize(task, &ui.global::<Messages>()));
    let open_folder = settings.after_extracting == AfterExtracting::OpenFolder;
    // The folder this window's run wrote to, recorded when "Extract inside"
    // replaces the queue: the last nested task's output is a folder inside
    // it, not the one to show.
    let parent_output: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
    if settings.close_when_done || open_folder {
        let (weak, r) = (ui.as_weak(), Rc::downgrade(&runner));
        let close = settings.close_when_done;
        let parent_output = parent_output.clone();
        // A nested queue runs after the parent, so "when extracted" fires
        // again; open the parent's folder only the first time.
        let folder_opened = Cell::new(false);
        runner.when_extracted(move || {
            // Several archives queued from one folder each have their own
            // output; the last one is where the run ended.
            let out = parent_output
                .borrow()
                .clone()
                .or_else(|| r.upgrade()?.task.borrow().as_ref()?.output.clone());
            if open_folder
                && let Some(out) = out
                && !folder_opened.replace(true)
            {
                let _ = open::that_detached(out);
            }
            // Hiding the only window ends the app, as the close button does.
            if close && let Some(ui) = weak.upgrade() {
                let _ = ui.hide();
            }
        });
    }

    let r = runner.clone();
    ui.on_pause(move || r.pause());
    let r = runner.clone();
    ui.on_resume(move || r.retry());
    let r = runner.clone();
    ui.on_open_folder(move || {
        let task = r.task.borrow();
        if let Some(out) = task.as_ref().and_then(|t| t.output.clone()) {
            let _ = open::that_detached(out);
        }
    });
    let r = runner.clone();
    let keep = args.keep_archive || settings.keep_archive;
    let parent_output = parent_output.clone();
    ui.on_extract_inside(move || {
        // Remember the parent before the queue below replaces it.
        *parent_output.borrow_mut() = r.task.borrow().as_ref().and_then(|t| t.output.clone());
        let keep = r.task.borrow().as_ref().map_or(keep, |t| t.keeps_archive());
        let keep_free = r.keep_free().unwrap_or(settings.keep_free);
        let (nested, written) = r
            .task
            .borrow()
            .as_ref()
            .map(|t| (t.nested.clone(), t.written.clone()))
            .unwrap_or_default();
        let nested = nested
            .into_iter()
            .filter(|p| hatchzip_core::archive::nested_kind(p).is_some())
            .collect::<Vec<_>>();
        let password = r.password();
        let jobs = nested
            .iter()
            .map(|path| {
                let (task, mut request) = nested_job(
                    path,
                    written.clone(),
                    keep,
                    keep_free,
                    settings.verify_first,
                );
                if let Some(p) = &password {
                    request.set_password(p.clone());
                }
                (task, request)
            })
            .collect();
        r.start_queue(jobs);
    });
    let r = runner.clone();
    ui.on_give_back(move || r.give_back());
    let r = runner.clone();
    ui.on_lower_reserve(move || r.lower_reserve());
    crate::password::host_in!(ui);
    crate::replace::host_in!(ui);
    let r = runner.clone();
    ui.on_dismiss(move || {
        if r.may_close() {
            let _ = slint::quit_event_loop();
        }
    });
    let r = runner.clone();
    ui.window().on_close_requested(move || {
        if r.may_close() {
            CloseRequestResponse::HideWindow
        } else {
            CloseRequestResponse::KeepWindowShown
        }
    });

    ui.show()?;
    Ok((ui, move || runner.start_queue(jobs)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_task_writes_where_its_menu_entry_says() {
        let primary = Path::new("/home/me/Downloads/game.7z.001");
        let s = Settings::default();
        assert_eq!(
            output_for(&TaskKind::ExtractHere, primary, &s),
            Some(PathBuf::from("/home/me/Downloads"))
        );
        assert_eq!(
            output_for(&TaskKind::ExtractTo, primary, &s),
            Some(PathBuf::from("/home/me/Downloads/game"))
        );
        let chosen = PathBuf::from("/media/usb");
        assert_eq!(
            output_for(&TaskKind::ExtractFiles(Some(chosen.clone())), primary, &s),
            Some(chosen.clone())
        );
        assert_eq!(output_for(&TaskKind::Test, primary, &s), None);

        // A default destination moves "Extract to a folder" only.
        let dir = tempfile::tempdir().unwrap();
        let s = Settings {
            destination: crate::settings::Destination::Folder,
            destination_folder: Some(dir.path().to_path_buf()),
            ..Settings::default()
        };
        assert_eq!(
            output_for(&TaskKind::ExtractTo, primary, &s),
            Some(dir.path().join("game"))
        );
        assert_eq!(
            output_for(&TaskKind::ExtractHere, primary, &s),
            Some(PathBuf::from("/home/me/Downloads"))
        );
        assert_eq!(
            output_for(&TaskKind::ExtractFiles(Some(chosen.clone())), primary, &s),
            Some(chosen)
        );
    }
}
