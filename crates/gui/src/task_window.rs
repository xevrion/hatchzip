//! The small window behind "Extract here", "Extract to a folder", "Extract
//! files" and "Test". The right-click was the confirmation, so it starts as
//! soon as it shows, unless the space check says no. Several archives run one
//! after another, each checked just before it starts; one that doesn't
//! finish stops the rest until it is resumed.

use std::path::{Path, PathBuf};

use anyhow::Result;
use hatchzip_core::session;
use slint::{CloseRequestResponse, ComponentHandle};

use crate::args::{TaskArgs, TaskKind};
use crate::runner::Runner;
use crate::settings::Settings;
use crate::task::Task;
use crate::work::Request;
use crate::{Messages, TaskWindow};

/// Where each kind of task puts the files. `None` for a test, and for files
/// whose folder is not chosen yet.
pub fn output_for(kind: &TaskKind, primary: &Path) -> Option<PathBuf> {
    match kind {
        TaskKind::ExtractHere => Some(primary.parent().unwrap_or(Path::new(".")).to_path_buf()),
        TaskKind::ExtractTo => Some(session::default_output(primary)),
        TaskKind::ExtractFiles(folder) => folder.clone(),
        TaskKind::Test => None,
    }
}

pub fn run(mut args: TaskArgs) -> Result<()> {
    // The window exists before the picker so its title comes from the same
    // translated messages as the rest, and the picker still runs on the
    // thread that will show it.
    let ui = TaskWindow::new()?;
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
    let (ui, start) = build_in(ui, args, Settings::saved())?;
    crate::backend::when_drawn(ui.window(), start);
    ui.run()?;
    Ok(())
}

/// The task and request for one archive. The main window's reserved space
/// and verification settings apply. The archive is kept when the menu entry
/// says so or when the main window's setting does, so someone who never
/// wants archives removed gets that from every entry.
fn job_for(
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
    match output_for(kind, &primary) {
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

/// The window, shown, and the function that starts the task. Nothing
/// touches an archive until that is called.
pub fn build(args: TaskArgs, settings: Settings) -> Result<(TaskWindow, impl FnOnce() + 'static)> {
    build_in(TaskWindow::new()?, args, settings)
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
    if settings.close_when_done {
        // Hiding the only window ends the app, as the close button does.
        let weak = ui.as_weak();
        runner.when_extracted(move || {
            if let Some(ui) = weak.upgrade() {
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
        assert_eq!(
            output_for(&TaskKind::ExtractHere, primary),
            Some(PathBuf::from("/home/me/Downloads"))
        );
        assert_eq!(
            output_for(&TaskKind::ExtractTo, primary),
            Some(PathBuf::from("/home/me/Downloads/game"))
        );
        let chosen = PathBuf::from("/media/usb");
        assert_eq!(
            output_for(&TaskKind::ExtractFiles(Some(chosen.clone())), primary),
            Some(chosen)
        );
        assert_eq!(output_for(&TaskKind::Test, primary), None);
    }
}
