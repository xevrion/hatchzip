//! The prompt that asks what to do with files already in the destination
//! folder, one file at a time until the person answers for the rest. Like
//! the password prompt, the questions come from the worker thread before
//! anything is touched, cross to the UI thread, and the worker waits for
//! each answer.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};

use hatchzip_core::replace::Choices;

/// What to do with one existing file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Replace,
    Skip,
    Both,
}

impl Answer {
    /// The order of the prompt's `answer` callback.
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Skip,
            2 => Self::Both,
            _ => Self::Replace,
        }
    }
}

pub struct Question {
    pub(crate) file: String,
    pub(crate) folder: String,
    pub(crate) left: usize,
    pub(crate) removing: bool,
    reply: Sender<Option<(Answer, bool)>>,
}

type Host = Box<dyn Fn(Question)>;

thread_local! {
    // Whichever window is open shows the prompt. Without one, the question
    // is dropped, which the worker reads as a cancel.
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// Called on the UI thread by the window that will show the prompt.
pub fn set_host(show: impl Fn(Question) + 'static) {
    HOST.with(|h| *h.borrow_mut() = Some(Box::new(show)));
}

/// Asks on the UI thread what to do with each of the files `found` in
/// `folder`, and waits for the answers. `None` when the person cancels.
/// `removing`: the archive goes as it is extracted, so a skipped file's copy
/// in it is not kept. For worker threads only.
pub fn ask(found: &[PathBuf], folder: &Path, removing: bool) -> Option<Choices> {
    let mut choices = Choices::default();
    let mut every = None;
    for (i, file) in found.iter().enumerate() {
        let answer = match every {
            Some(a) => a,
            None => {
                let (a, rest) = one(file, folder, found.len() - i - 1, removing)?;
                if rest {
                    every = Some(a);
                }
                a
            }
        };
        match answer {
            Answer::Replace => {}
            Answer::Skip => choices.keep.push(file.clone()),
            Answer::Both => choices.both.push(file.clone()),
        }
    }
    Some(choices)
}

fn one(file: &Path, folder: &Path, left: usize, removing: bool) -> Option<(Answer, bool)> {
    let (reply, answer) = channel();
    let question = Question {
        file: file.display().to_string(),
        folder: folder.display().to_string(),
        left,
        removing,
        reply,
    };
    let sent = slint::invoke_from_event_loop(move || {
        HOST.with(|h| {
            if let Some(show) = &*h.borrow() {
                show(question);
            }
        })
    });
    sent.ok()?;
    answer.recv().ok().flatten()
}

/// The window side: holds the open question until the person answers.
#[derive(Default)]
pub struct Pending(RefCell<Option<Question>>);

impl Pending {
    pub fn put(&self, q: Question) {
        if let Some(old) = self.0.replace(Some(q)) {
            let _ = old.reply.send(None);
        }
    }

    pub fn answer(&self, answer: Option<(Answer, bool)>) {
        if let Some(q) = self.0.take() {
            let _ = q.reply.send(answer);
        }
    }
}

/// Shows questions in a window with the `replace-*` properties and
/// callbacks that host a `ReplacePrompt`, as `password::host_in` does.
macro_rules! host_in {
    ($ui:expr) => {{
        let pending = std::rc::Rc::new($crate::replace::Pending::default());
        let (p, weak) = (pending.clone(), $ui.as_weak());
        $crate::replace::set_host(move |q| {
            if let Some(ui) = weak.upgrade() {
                ui.set_replace_text(
                    ui.invoke_replace_question(q.file.as_str().into(), q.folder.as_str().into()),
                );
                ui.set_replace_note(ui.invoke_replace_instructions(q.removing));
                ui.set_replace_rest(ui.invoke_replace_remaining(
                    i32::try_from(q.left).unwrap_or(i32::MAX),
                    $crate::text::count(q.left).into(),
                ));
                ui.set_replace_shown(true);
                p.put(q);
            }
        });
        let (p, weak) = (pending.clone(), $ui.as_weak());
        $ui.on_replace_answer(move |choice, rest| {
            if let Some(ui) = weak.upgrade() {
                ui.set_replace_shown(false);
            }
            p.answer(Some(($crate::replace::Answer::from_index(choice), rest)));
        });
        let (p, weak) = (pending, $ui.as_weak());
        $ui.on_replace_cancel(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_replace_shown(false);
            }
            p.answer(None);
        });
    }};
}
pub(crate) use host_in;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_keep_the_prompt_order() {
        assert_eq!(Answer::from_index(0), Answer::Replace);
        assert_eq!(Answer::from_index(1), Answer::Skip);
        assert_eq!(Answer::from_index(2), Answer::Both);
    }
}
