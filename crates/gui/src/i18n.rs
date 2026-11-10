//! Translated task presentation shared by both windows.
use crate::{Messages, TaskView, task::Task, text};
use hatchzip_core::human;

fn plural_count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

pub(crate) fn task_view(task: &Task, tr: &Messages) -> TaskView {
    let mut view = task.view.clone();
    if let Some(plan) = &task.space_plan {
        view.space = tr.invoke_task_space(human(plan.needed).into(), human(plan.free).into());
        if view.phase == crate::Phase::NoSpace {
            view.message = plan_view(plan, tr).verdict;
        }
    }
    if let Some((at, total)) = task.queue_position {
        view.step = if view.phase == crate::Phase::Done && at == total {
            tr.invoke_queue_done(total.to_string().into())
        } else {
            let position =
                tr.invoke_queue_position(at.to_string().into(), total.to_string().into());
            if task.running() || at == total {
                position
            } else {
                format!(
                    "{}. {}.",
                    position,
                    tr.invoke_queue_waiting(
                        plural_count(total - at),
                        (total - at).to_string().into()
                    )
                )
                .into()
            }
        };
    }
    if view.phase == crate::Phase::Failed {
        view.message = diagnostic(view.message.as_str(), tr).into();
    }
    view.heading = tr.invoke_task_heading(
        view.phase,
        view.is_test,
        task.is_restoring(),
        task.name().into(),
    );
    view.busy = tr.invoke_checking_space();
    view.detail = if task.is_restoring() {
        tr.invoke_restore_detail()
    } else if view.is_test {
        tr.invoke_test_detail()
    } else if let Some(path) = &task.output {
        tr.invoke_task_destination(path.display().to_string().into())
    } else {
        Default::default()
    };
    if task.waiting_for_disk() && !view.rate.is_empty() {
        view.rate = tr.invoke_waiting_disk();
    }
    match view.phase {
        crate::Phase::Pausing => view.message = tr.invoke_pausing_message(),
        crate::Phase::Verifying if !view.is_test => view.message = tr.invoke_verifying_message(),
        crate::Phase::Paused => view.message = tr.invoke_paused_message(),
        crate::Phase::Done if !view.is_test && !task.is_restoring() => {
            view.message = if task.keeps_archive() {
                tr.invoke_extracted_kept()
            } else {
                tr.invoke_extracted_removed()
            };
            if let Some(warning) = &task.warning {
                view.message = format!("{} {warning}.", view.message).into();
            }
        }
        _ => {}
    }
    if let Some(reason) = &task.resumable_failure {
        view.message = tr.invoke_task_resumable_error(diagnostic(reason, tr).into());
    }
    if let Some((entries, output, kept, aside)) = &task.restore_result {
        let held = tr.invoke_restore_held(plural_count(*entries), text::count(*entries).into());
        let mut message = tr
            .invoke_restore_result(*kept, held, output.display().to_string().into())
            .to_string();
        if *aside > 0 {
            message.push(' ');
            message.push_str(
                &tr.invoke_restore_aside(
                    plural_count(*aside),
                    text::count(*aside).into(),
                    hatchzip_core::replace::folder(output)
                        .display()
                        .to_string()
                        .into(),
                ),
            );
        }
        view.message = message.into();
    }
    if let Some(count) = task.tested_entries {
        view.message = tr.invoke_task_test_result(plural_count(count), text::count(count).into());
    }
    if let Some(percent) = task.reading_percent {
        view.busy = tr.invoke_reading_archive(percent);
    }
    if !view.amounts.is_empty()
        && let Some((done, total)) = task.progress_bytes
    {
        view.amounts = tr.invoke_task_amounts(human(done).into(), human(total).into());
    }
    if !view.archive_left.is_empty()
        && let Some(left) = task.remaining_archive_bytes
    {
        view.archive_left = tr.invoke_task_archive_left(human(left).into());
    }
    if !view.rate.is_empty()
        && let Some((speed, seconds)) = task.rate_estimate
    {
        let duration = match seconds {
            0..=4 => tr.invoke_time_short().to_string(),
            5..=59 => tr
                .invoke_time_seconds(
                    i32::try_from(seconds).unwrap_or(i32::MAX),
                    seconds.to_string().into(),
                )
                .to_string(),
            60..=3599 => {
                let minutes = seconds.div_ceil(60);
                tr.invoke_time_minutes(
                    i32::try_from(minutes).unwrap_or(i32::MAX),
                    minutes.to_string().into(),
                )
                .to_string()
            }
            _ => {
                let hours = seconds / 3600;
                let minutes = (seconds % 3600) / 60;
                format!(
                    "{} {}",
                    tr.invoke_time_hours(
                        i32::try_from(hours).unwrap_or(i32::MAX),
                        hours.to_string().into()
                    ),
                    tr.invoke_time_minutes(
                        i32::try_from(minutes).unwrap_or(i32::MAX),
                        minutes.to_string().into()
                    )
                )
            }
        };
        view.rate = tr.invoke_task_rate(human(speed).into(), duration.into());
    }

    view
}

pub(crate) fn plan_view(
    plan: &hatchzip_core::session::SpacePlan,
    tr: &Messages,
) -> crate::PlanView {
    let mut view = text::plan_view(plan);
    view.verdict = if plan.fits {
        tr.invoke_space_ok()
    } else {
        tr.invoke_space_short(human(plan.needed + plan.keep_free - plan.free).into())
    };
    let mut notes = Vec::new();
    if plan.either && plan.keep_archive && !plan.fits && plan.in_place_fits() {
        notes.push(tr.invoke_space_removing(human(plan.in_place_needs).into()));
    }
    if plan.either && !plan.keep_archive && plan.fits && plan.normal_fits() {
        notes.push(tr.invoke_space_keep());
    }
    if !plan.fits && plan.solid_block_bound {
        notes.push(tr.invoke_space_solid());
    }
    if !plan.reclaims && !plan.keep_archive {
        notes.push(tr.invoke_space_other_drive());
    }
    if plan.archive_kept_whole {
        notes.push(tr.invoke_space_whole());
    }
    view.note = notes
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .into();
    view
}

// Decoder and OS errors carry paths and recovery instructions. Preserve the
// original diagnostics rather than guessing translations from substrings.
pub(crate) fn diagnostic(message: &str, tr: &Messages) -> String {
    match message {
        "Test cancelled. The archive was not modified." => tr.invoke_test_cancelled().to_string(),
        "Extraction cancelled. The archive was not modified." => {
            tr.invoke_extract_cancelled().to_string()
        }
        _ => tr.invoke_error_details(message.into()).to_string(),
    }
}
