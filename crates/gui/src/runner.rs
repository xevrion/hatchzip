//! Runs one request at a time on a worker thread and keeps a [`Task`] up to
//! date for whichever window shows it.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use slint::{Timer, TimerMode};

use crate::task::Task;
use crate::work::{self, Counters, Request, Update};
use crate::{password, replace};

/// About ten refreshes a second: smooth to the eye, and cheap however fast
/// the disk is, since the worker only bumps counters.
const TICK: Duration = Duration::from_millis(100);

thread_local! {
    // Updates come from the worker thread through the event loop, and only
    // `Send` data can make that trip, so they find the runner here.
    static RUNNER: RefCell<Option<Rc<Runner>>> = const { RefCell::new(None) };
}

pub struct Runner {
    pub task: RefCell<Option<Task>>,
    request: RefCell<Option<Request>>,
    counters: Arc<Counters>,
    timer: Timer,
    quit_when_stopped: Cell<bool>,
    show: Box<dyn Fn(&Task)>,
    /// Archives still to do after the current one, from a right-click on
    /// several. The next starts only once the current one is done.
    queue: RefCell<VecDeque<(Task, Request)>>,
    /// Which of how many the current one is, counting from 1.
    place: Cell<(usize, usize)>,
    /// Called once every queued archive is extracted.
    when_extracted: RefCell<Option<Box<dyn Fn()>>>,
}

impl Runner {
    pub fn install(show: impl Fn(&Task) + 'static) -> Rc<Self> {
        let runner = Rc::new(Self {
            task: RefCell::new(None),
            request: RefCell::new(None),
            counters: Arc::new(Counters::default()),
            timer: Timer::default(),
            quit_when_stopped: Cell::new(false),
            show: Box::new(show),
            queue: RefCell::default(),
            place: Cell::new((1, 1)),
            when_extracted: RefCell::new(None),
        });
        RUNNER.with(|r| *r.borrow_mut() = Some(runner.clone()));
        runner
    }

    /// Runs `f` when the last queued archive is extracted. Not after a test,
    /// a restore, a pause or a failure, whose outcome is still on screen.
    pub fn when_extracted(&self, f: impl Fn() + 'static) {
        *self.when_extracted.borrow_mut() = Some(Box::new(f));
    }

    /// The password the last request opened its archive with, if any.
    pub fn password(&self) -> Option<String> {
        self.request
            .borrow()
            .as_ref()?
            .password()
            .map(str::to_string)
    }

    /// Stops a paused or failed extraction for good, giving the archive
    /// back. The engine's journal makes this safe at any point that isn't
    /// mid-run.
    pub fn give_back(&self) {
        if self.running() {
            return;
        }
        let Some(archive) = self
            .request
            .borrow()
            .as_ref()
            .map(|r| r.archive().to_path_buf())
        else {
            return;
        };
        let name = self
            .task
            .borrow()
            .as_ref()
            .map_or_else(String::new, |t| t.name().to_string());
        // Stopping one archive for good stops the rest too; nothing more
        // starts behind the person's back.
        self.queue.borrow_mut().clear();
        self.place.set((1, 1));
        self.start(Task::restore(&name), Request::Restore { archive });
    }

    pub fn running(&self) -> bool {
        self.task.borrow().as_ref().is_some_and(Task::running)
    }

    pub fn start(&self, task: Task, request: Request) {
        self.start_after(|| {}, task, request);
    }

    /// Runs each in turn, the next once the last is done. One that pauses,
    /// fails or doesn't fit holds the rest until it is resumed and done.
    pub fn start_queue(&self, jobs: Vec<(Task, Request)>) {
        let mut jobs = VecDeque::from(jobs);
        let Some((task, request)) = jobs.pop_front() else {
            return;
        };
        self.place.set((1, jobs.len() + 1));
        *self.queue.borrow_mut() = jobs;
        self.start(task, request);
    }

    /// "Archive 2 of 3" while several are queued, and what is left when one
    /// stops them.
    fn step(&self, task: &Task) -> String {
        let (at, of) = self.place.get();
        if of < 2 {
            return String::new();
        }
        let left = of - at;
        if task.view.phase == crate::Phase::Done && left == 0 {
            return format!("All {of} archives completed.");
        }
        if task.running() || left == 0 {
            return format!("Archive {at} of {of}");
        }
        let rest = if left == 1 {
            "1 more archive has not started".to_string()
        } else {
            format!("{left} more archives have not started")
        };
        format!("Archive {at} of {of}. {rest}.")
    }

    /// Starts the next queued archive if the current one is done.
    fn next_in_queue(&self) -> bool {
        let done = self
            .task
            .borrow()
            .as_ref()
            .is_some_and(|t| t.view.phase == crate::Phase::Done);
        if !done {
            return false;
        }
        let next = self.queue.borrow_mut().pop_front();
        let Some((task, request)) = next else {
            return false;
        };
        let (at, of) = self.place.get();
        self.place.set((at + 1, of));
        self.start(task, request);
        true
    }

    /// Like [`Self::start`], with `before` run first on the worker thread,
    /// for cleanup that may have to wait on the disk.
    pub fn start_after(
        &self,
        before: impl FnOnce() + Send + 'static,
        task: Task,
        request: Request,
    ) {
        *self.task.borrow_mut() = Some(task);
        *self.request.borrow_mut() = Some(request);
        self.launch(before);
    }

    /// Runs the last request again: after a pause, a failure or freeing
    /// space. The engine picks up from its journal.
    pub fn retry(&self) {
        if self.running() || self.request.borrow().is_none() {
            return;
        }
        self.launch(|| {});
    }

    /// Runs the last extraction again with less reserved space, as the
    /// no-space state offers when the plan fits without it. Only this run:
    /// the saved default does not change.
    pub fn lower_reserve(&self) {
        if self.running() {
            return;
        }
        let Some(offer) = self.task.borrow().as_ref().and_then(|t| t.lower_reserve) else {
            return;
        };
        let lowered = self
            .request
            .borrow_mut()
            .as_mut()
            .is_some_and(|r| r.set_keep_free(offer));
        if lowered {
            self.launch(|| {});
        }
    }

    fn launch(&self, before: impl FnOnce() + Send + 'static) {
        let Some(request) = self.request.borrow().clone() else {
            return;
        };
        self.counters.reset();
        self.update(|t| t.apply(Update::Checking));
        let counters = self.counters.clone();
        let name = self
            .task
            .borrow()
            .as_ref()
            .map_or_else(String::new, |t| t.name().to_string());
        std::thread::spawn(move || {
            before();
            let send = |u: Update| {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(r) = RUNNER.with(|r| r.borrow().clone()) {
                        r.on_update(u);
                    }
                });
            };
            work::run(
                &request,
                &counters,
                &send,
                &mut |wrong| password::ask(&name, wrong),
                &mut replace::ask,
            );
        });
        self.timer.start(TimerMode::Repeated, TICK, || {
            if let Some(r) = RUNNER.with(|r| r.borrow().clone()) {
                r.tick();
            }
        });
    }

    pub fn pause(&self) {
        let mut asked = false;
        self.update(|t| asked = t.request_pause());
        if asked {
            self.counters.stop.store(true, Ordering::Relaxed);
        }
    }

    /// Whether the window may close now. A run that is writing is paused
    /// first, and the app quits once the engine has stopped cleanly.
    pub fn may_close(&self) -> bool {
        if !self.running() {
            return true;
        }
        let writing =
            self.task.borrow().as_ref().is_some_and(|t| {
                matches!(t.view.phase, crate::Phase::Working | crate::Phase::Pausing)
            });
        if !writing {
            // Checking and testing write nothing, or nothing a rerun doesn't
            // already recover from, so cutting them short is safe.
            return true;
        }
        self.quit_when_stopped.set(true);
        self.pause();
        false
    }

    fn tick(&self) {
        let now = Instant::now();
        self.update(|t| t.tick(now, &self.counters));
    }

    fn on_update(&self, u: Update) {
        if let Update::Unlocked(p) = &u
            && let Some(r) = self.request.borrow_mut().as_mut()
        {
            r.set_password(p.clone());
        }
        // One last read so a pause shows exactly where it stopped.
        self.tick();
        self.update(|t| t.apply(u));
        if !self.running() {
            if self.next_in_queue() {
                return;
            }
            self.timer.stop();
            if self.quit_when_stopped.get() {
                let _ = slint::quit_event_loop();
            } else if self.task.borrow().as_ref().is_some_and(Task::extracted)
                && let Some(f) = self.when_extracted.borrow().as_ref()
            {
                f();
            }
        }
    }

    fn update(&self, f: impl FnOnce(&mut Task)) {
        let mut task = self.task.borrow_mut();
        if let Some(t) = task.as_mut() {
            f(t);
            t.view.step = self.step(t).into();
            (self.show)(t);
        }
    }
}
