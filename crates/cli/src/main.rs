mod cli;

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anstream::{eprintln, print, println};
use anstyle::{AnsiColor, Style};
use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser};
use indicatif::{ProgressBar, ProgressStyle};

use cli::{
    Cli, Command, EXIT_DAMAGED, EXIT_FAILURE, EXIT_NO_SPACE, EXIT_PASSWORD, EXIT_PAUSED, Secret,
    Space, Target,
};
use hatchzip_core::archive::storage::{Reads, Storage};
use hatchzip_core::archive::{Damaged, Kind, PasswordNeeded, WrongPassword};
use hatchzip_core::engine::{self, Job, Outcome, Progress, Run};
use hatchzip_core::human;
use hatchzip_core::journal::{Journal, Paths};
use hatchzip_core::session::{self, State, display_name, locate};

const ERROR: Style = AnsiColor::Red.on_default().bold();
const GOOD: Style = AnsiColor::Green.on_default().bold();
const BOLD: Style = Style::new().bold();
const DIM: Style = Style::new().dimmed();

static STOP: AtomicBool = AtomicBool::new(false);
static QUIET: AtomicBool = AtomicBool::new(false);

/// Normal output, silenced by `--quiet`. Errors and prompts always show.
macro_rules! say {
    ($($t:tt)*) => {
        if !QUIET.load(Ordering::Relaxed) {
            println!($($t)*);
        }
    };
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    QUIET.store(cli.quiet, Ordering::Relaxed);
    let code = match run(cli.command) {
        Ok(code) => code,
        Err(e) => fail(&e),
    };
    ExitCode::from(code)
}

/// Prints an error and returns the exit status it calls for.
fn fail(e: &anyhow::Error) -> u8 {
    eprintln!("{ERROR}error:{ERROR:#} {e:#}");
    if e.is::<PasswordNeeded>() || e.is::<WrongPassword>() {
        if !io::stdin().is_terminal() {
            eprintln!("Pass --password or set HATCHZIP_PASSWORD when not running in a terminal.");
        }
        EXIT_PASSWORD
    } else if e.is::<Damaged>() {
        EXIT_DAMAGED
    } else {
        EXIT_FAILURE
    }
}

/// Opens the archive, asking for a password when it is encrypted and none
/// was given, and again (up to three times) when one was wrong. Nothing is
/// touched until the password is proven.
fn open(
    archive: &Path,
    output: Option<PathBuf>,
    writable: bool,
    secret: Secret,
) -> Result<(State, Paths)> {
    let (primary, paths) = locate(archive)?;
    let reads = Arc::new(Reads::default());
    let mut reading = Some(Reading::show(&reads));
    let mut tries = 0;
    let opened = session::open_encrypted_noting(
        &primary,
        &paths,
        output,
        writable,
        secret.password,
        &reads,
        |wrong| {
            // The bar would draw over the prompt.
            if let Some(r) = reading.take() {
                r.finish();
            }
            if wrong {
                eprintln!("{ERROR}Incorrect password.{ERROR:#}");
            }
            tries += 1;
            if tries > 3 || !io::stdin().is_terminal() {
                return None;
            }
            rpassword::prompt_password("Password: ").ok()
        },
        |file, size| {
            // Small files decode in a moment; say nothing then.
            if size >= 64 << 20 && !QUIET.load(Ordering::Relaxed) {
                eprintln!(
                    "Checking the password by decoding '{}' ({}). This archive has no password \
                     check value, so this may take a while.",
                    file.display(),
                    human(size)
                );
            }
        },
    );
    if let Some(r) = reading {
        r.finish();
    }
    let (state, _) = opened?;
    Ok((state, paths))
}

/// A bar for a long read through the archive while it opens: a compressed
/// tar is decoded in full before anything else, which takes minutes for a
/// large one. It appears only once opening has taken a moment, and only on
/// a terminal without `--quiet`.
struct Reading {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Reading {
    const DELAY: Duration = Duration::from_millis(500);

    fn show(reads: &Arc<Reads>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = (io::stderr().is_terminal() && !QUIET.load(Ordering::Relaxed)).then(|| {
            let (stop, reads) = (stop.clone(), reads.clone());
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let mut bar: Option<ProgressBar> = None;
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                    let total = reads.total.load(Ordering::Relaxed);
                    let done = reads.done.load(Ordering::Relaxed).min(total);
                    if bar.is_none() && start.elapsed() >= Self::DELAY && done < total {
                        // No speed or time left: the bar starts part way
                        // through, which throws the estimate off for its
                        // first seconds.
                        let b = ProgressBar::new(total).with_position(done);
                        b.set_style(
                            ProgressStyle::with_template(
                                "{prefix} [{bar:32}] {binary_bytes}/{binary_total_bytes}  {percent}%",
                            )
                            .expect("valid template")
                            .progress_chars("=> "),
                        );
                        b.set_prefix("Reading");
                        b.enable_steady_tick(Duration::from_millis(200));
                        bar = Some(b);
                    }
                    if let Some(b) = &bar {
                        b.set_length(total);
                        b.set_position(done);
                    }
                }
                if let Some(b) = bar {
                    b.finish_and_clear();
                }
            })
        });
        Self { stop, thread }
    }

    fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(command: Command) -> Result<u8> {
    match command {
        Command::Extract {
            target,
            keep,
            replace,
            verify_first,
            space,
            yes,
        } => {
            let opts = Options {
                keep: keep.keep_archive,
                replace: replace.replace,
                skip_existing: replace.skip_existing,
                verify_first,
                keep_free: space.keep_free,
                yes,
                several: false,
            };
            extract_all(target, opts)
        }
        Command::Check {
            target,
            keep,
            replace,
            space,
        } => check_all(target, keep.keep_archive, replace.replace, space),
        Command::List { archives, secret } => {
            let archives = distinct(&archives)?;
            Ok(each(&archives, true, |a| list(a, secret.clone())))
        }
        Command::Test { archives, secret } => {
            let archives = distinct(&archives)?;
            let code = each(&archives, true, |a| test_command(a, secret.clone()));
            if archives.len() > 1 && code == 0 {
                say!("{GOOD}All {} archives passed.{GOOD:#}", archives.len());
            }
            Ok(code)
        }
        Command::Restore { archive, output } => restore(&archive, output.as_deref()),
        Command::Completions { shell } => {
            // Named after however we were called, so `hz completions bash`
            // completes the `hz` alias.
            let name = std::env::args_os()
                .next()
                .and_then(|a| {
                    Path::new(&a)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                })
                .filter(|n| n == "hz")
                .unwrap_or_else(|| "hatchzip".into());
            clap_complete::generate(shell, &mut Cli::command(), name, &mut io::stdout());
            Ok(0)
        }
        Command::Man => {
            clap_mangen::Man::new(Cli::command()).render(&mut io::stdout())?;
            Ok(0)
        }
    }
}

/// The archives as given, minus repeats. Volumes of one split set count
/// once, so `game.7z.*` from a shell glob means the one archive.
fn distinct(archives: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for a in archives {
        if seen.insert(locate(a)?.0) {
            out.push(a.clone());
        }
    }
    Ok(out)
}

/// Runs `f` on every archive, even after one fails, and returns the first
/// status that is not success. With several archives, each one's output is
/// headed by its name when `headed`.
fn each(archives: &[PathBuf], headed: bool, mut f: impl FnMut(&Path) -> Result<u8>) -> u8 {
    let mut first = 0;
    for (i, a) in archives.iter().enumerate() {
        if archives.len() > 1 {
            if i > 0 {
                say!();
            }
            if headed {
                say!("{BOLD}{}{BOLD:#}", a.display());
            }
        }
        let code = f(a).unwrap_or_else(|e| fail(&e));
        if first == 0 {
            first = code;
        }
    }
    first
}

/// Prints the space plan and returns whether it fits. The verdict prints
/// even under `--quiet`, since it is the whole answer.
fn report(
    job: &Job,
    out: &Path,
    journal: Option<&Journal>,
    keep_free: u64,
    keep: bool,
) -> Result<bool> {
    let plan = session::plan(job, out, journal.is_some(), keep_free, keep)?;
    let left = job.remaining_size();

    say!(
        "{BOLD}{}{BOLD:#}  {} {}, {} uncompressed in {}",
        display_name(job),
        human(job.storage.original_len()),
        job.format_name,
        human(journal.map_or(left, |j| j.total_size)),
        entries(job.item_count())
    );
    if let Some(j) = journal {
        say!(
            "  resuming: {} of {} remaining",
            human(left),
            human(j.total_size)
        );
    }
    say!("  available                    {}", human(plan.free));
    if plan.keep_archive {
        say!(
            "  required                     {BOLD}{}{BOLD:#} {DIM}(+{} reserved){DIM:#}",
            human(plan.needed),
            human(plan.keep_free)
        );
        say!(
            "  {DIM}note: the archive is kept, so space is required for all extracted files{DIM:#}"
        );
    } else {
        say!(
            "  standard extraction needs    {}",
            human(plan.normal_needs)
        );
        say!(
            "  required                     {BOLD}{}{BOLD:#} {DIM}(+{} reserved){DIM:#}",
            human(plan.needed),
            human(plan.keep_free)
        );
        if !plan.reclaims {
            say!(
                "  {DIM}note: the destination is on another drive, so removing the archive does not free space there{DIM:#}"
            );
        }
        if plan.archive_kept_whole {
            say!(
                "  {DIM}note: this drive cannot release space inside a file, so the compressed archive remains whole until extraction completes and room is required for the archive and every extracted file{DIM:#}"
            );
        }
    }
    if plan.fits {
        say!("  available after              {}", human(plan.free_after));
        if !plan.keep_archive && plan.either && plan.normal_fits() {
            say!(
                "  {DIM}note: there is also enough space to keep the archive (--keep-archive){DIM:#}"
            );
        }
    } else {
        let short = human(plan.needed + plan.keep_free - plan.free);
        println!(
            "{ERROR}Insufficient disk space:{ERROR:#} {short} more is required. Free up space or lower --keep-free."
        );
        if plan.keep_archive && plan.either && plan.in_place_fits() {
            println!(
                "Without --keep-archive, the archive is removed during extraction and only {} is required.",
                human(plan.in_place_needs)
            );
        }
        if plan.solid_block_bound {
            println!(
                "{DIM}Most of the required space is one solid block, which can only be released after \
                 it is fully extracted. Archives created with smaller solid blocks require much \
                 less space.{DIM:#}"
            );
        }
    }
    Ok(plan.fits)
}

fn check_all(target: Target, keep: bool, replace: bool, space: Space) -> Result<u8> {
    let archives = distinct(&target.archives)?;
    let code = each(&archives, false, |a| {
        check(
            a,
            target.output.clone(),
            target.secret.clone(),
            (keep, replace),
            &space,
        )
    });
    if archives.len() > 1 {
        say!();
        say!(
            "{DIM}Each archive is checked against the space available now. Extracting one changes \
             the space available for the next; extract checks each one again just before it \
             starts.{DIM:#}"
        );
    }
    Ok(code)
}

fn check(
    archive: &Path,
    output: Option<PathBuf>,
    secret: Secret,
    (keep, replace): (bool, bool),
    space: &Space,
) -> Result<u8> {
    let (state, _) = open(archive, output, false, secret)?;
    let fits = match state {
        State::Fresh(job, out) => {
            in_place_possible(&job, keep)?;
            let fits = report(&job, &out, None, space.keep_free, keep)?;
            // Anything that would stop replacing them, extract reports.
            if let Ok(found) = session::existing(&job, &out)
                && !found.is_empty()
            {
                if replace {
                    say!(
                        "  {DIM}note: {} will be replaced{DIM:#}",
                        existing_files(found.len())
                    );
                } else {
                    say!(
                        "  {DIM}note: {} in the destination; extract with --replace to replace existing files{DIM:#}",
                        already_exist(found.len())
                    );
                }
            }
            fits
        }
        State::Resume(job, journal) => {
            let keep = session::keeps_archive(&journal, keep)?;
            report(&job, &journal.output, Some(&journal), space.keep_free, keep)?
        }
        State::Orphan(_) => {
            say!("All entries are already extracted. Run extract to complete the cleanup.");
            true
        }
    };
    Ok(if fits { 0 } else { EXIT_NO_SPACE })
}

/// Refuses, before anything is touched, an archive that can only be
/// extracted while it is kept, such as a solid RAR, unless it is.
fn in_place_possible(job: &Job, keep: bool) -> Result<()> {
    match job.in_place_refusal() {
        Some(why) if !keep => bail!(
            "{why}. Run the command again with --keep-archive to extract it and keep the archive, \
             or extract it with another program"
        ),
        _ => Ok(()),
    }
}

fn list(archive: &Path, secret: Secret) -> Result<u8> {
    let job = match open(archive, None, false, secret)?.0 {
        State::Fresh(job, _) | State::Resume(job, _) => job,
        State::Orphan(_) => bail!("no entries remain in this archive"),
    };
    println!("{BOLD}{:>10}  Name{BOLD:#}", "Size");
    for item in job.units.iter().rev().flat_map(|u| &u.items) {
        let path = item.path.display();
        match &item.kind {
            Kind::Dir => println!("{:>10}  {path}/", ""),
            Kind::File => println!("{:>10}  {path}", human(item.size)),
            Kind::Symlink => println!("{:>10}  {path} {DIM}(symlink){DIM:#}", ""),
            Kind::HardLink(t) => {
                println!("{:>10}  {path} {DIM}(link to {}){DIM:#}", "", t.display())
            }
        }
    }
    say!(
        "{DIM}{}, {} uncompressed, {} {} archive{DIM:#}",
        entries(job.item_count()),
        human(job.remaining_size()),
        human(job.storage.original_len()),
        job.format_name
    );
    Ok(0)
}

fn test_command(archive: &Path, secret: Secret) -> Result<u8> {
    let job = match open(archive, None, false, secret)?.0 {
        State::Fresh(job, _) => job,
        _ => bail!(
            "this archive is partially extracted and cannot be tested. Run extract to resume it, or restore to undo it"
        ),
    };
    match check_sfv(&job.storage) {
        Ok(Some(sfv)) => {
            let name = sfv.file_name().unwrap_or_default().to_string_lossy();
            say!("{GOOD}Checksums match:{GOOD:#} volumes verified against {name}.");
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("{ERROR}error:{ERROR:#} {e:#}");
            return Ok(if STOP.load(Ordering::Relaxed) {
                EXIT_PAUSED
            } else {
                EXIT_DAMAGED
            });
        }
    }
    if let Err(e) = verify(&job) {
        eprintln!("{ERROR}error:{ERROR:#} {e:#}");
        return Ok(EXIT_DAMAGED);
    }
    say!(
        "{GOOD}Test passed:{GOOD:#} {} verified.",
        entries(job.item_count())
    );
    Ok(0)
}

fn check_sfv(storage: &Storage) -> Result<Option<PathBuf>> {
    let reads = storage.reads();
    reads.reset();
    let reading = Reading::show(&reads);
    let result = session::check_sfv(storage, &|| STOP.load(Ordering::Relaxed));
    reading.finish();
    result
}

fn entries(n: usize) -> String {
    if n == 1 {
        "1 entry".into()
    } else {
        format!("{n} entries")
    }
}

fn existing_files(n: usize) -> String {
    if n == 1 {
        "1 existing file".into()
    } else {
        format!("{n} existing files")
    }
}

fn already_exist(n: usize) -> String {
    if n == 1 {
        "1 file already exists".into()
    } else {
        format!("{n} files already exist")
    }
}

/// Refuses files in the way of a run without `--replace`. Nothing was
/// touched yet.
fn refuse_existing(out: &Path, found: &[PathBuf]) -> anyhow::Error {
    let first = out.join(&found[0]);
    let what = match found.len() {
        1 => format!("{} already exists", first.display()),
        n => format!(
            "{} and {} other {} already exist",
            first.display(),
            n - 1,
            if n == 2 { "file" } else { "files" }
        ),
    };
    anyhow::anyhow!(
        "{what}. Use --replace to replace existing files, or choose another destination"
    )
}

fn restore(archive: &Path, output: Option<&Path>) -> Result<u8> {
    let r = hatchzip_core::restore::restore(archive, output)?;
    let name = r.archive.file_name().unwrap_or_default().to_string_lossy();
    if r.kept {
        say!("{GOOD}Extraction progress discarded.{GOOD:#} {name} was kept and was not modified.");
    } else {
        say!(
            "{GOOD}Archive restored.{GOOD:#} {name} is a standard {} archive again and contains \
             the {} not yet extracted.",
            r.format,
            entries(r.entries)
        );
    }
    say!("Extracted files remain in {}.", r.output.display());
    if r.set_aside > 0 {
        let (files, them) = if r.set_aside == 1 {
            (
                "1 replaced file remains",
                "the file extracted at its path is",
            )
        } else {
            (
                &*format!("{} replaced files remain", r.set_aside),
                "the files extracted at their paths are",
            )
        };
        say!(
            "{files} in {}, since {them} no longer in the archive.",
            hatchzip_core::replace::folder(&r.output).display()
        );
    }
    Ok(0)
}

/// How one archive of a run ended, for the summary of several.
enum Ended {
    Extracted,
    /// Stopped with the given status; `saved` if a journal holds progress.
    Stopped {
        code: u8,
        saved: bool,
    },
}

struct Options {
    keep: bool,
    replace: bool,
    skip_existing: bool,
    verify_first: bool,
    keep_free: u64,
    yes: bool,
    /// Several archives: the summary says how to continue, not each one.
    several: bool,
}

fn extract_all(target: Target, mut opts: Options) -> Result<u8> {
    let archives = distinct(&target.archives)?;
    opts.several = archives.len() > 1;
    // A mistyped name is cheap to catch before the first archive is
    // removed, rather than halfway through.
    if opts.several {
        for a in &archives {
            // A run that ended just before its cleanup has a journal left.
            let (primary, paths) = locate(a)?;
            if !primary.exists() && !paths.exists() {
                bail!("{} does not exist. No archive was extracted", a.display());
            }
        }
    }
    // Installed once: a second handler can't be set in the same process.
    ctrlc::set_handler(|| {
        if STOP.swap(true, Ordering::Relaxed) {
            std::process::exit(EXIT_PAUSED.into());
        }
        eprintln!(
            "\nPausing after the current batch. Partly written files are redone on resume. \
             Press Ctrl+C again to exit immediately."
        );
    })
    .context("installing Ctrl+C handler")?;

    let mut done = 0;
    for (i, archive) in archives.iter().enumerate() {
        if opts.several {
            if i > 0 {
                say!();
            }
            say!(
                "{BOLD}Archive {} of {}:{BOLD:#} {}",
                i + 1,
                archives.len(),
                archive.display()
            );
        }
        let ended = match extract(archive, target.output.clone(), target.secret.clone(), &opts) {
            Ok(e) => e,
            Err(e) => Ended::Stopped {
                code: fail(&e),
                saved: locate(archive).is_ok_and(|(_, p)| p.exists()),
            },
        };
        match ended {
            Ended::Extracted => done += 1,
            Ended::Stopped { code, saved } => {
                if opts.several {
                    summary(&archives, done, code, saved);
                }
                return Ok(code);
            }
        }
    }
    if opts.several {
        say!();
        say!("{GOOD}All {} archives extracted.{GOOD:#}", archives.len());
    }
    Ok(0)
}

/// What a run of several archives did, after `archives[done]` stopped it.
/// Shown even under `--quiet`, since it is the answer to what happened.
fn summary(archives: &[PathBuf], done: usize, code: u8, saved: bool) {
    let stopped = if code == EXIT_PAUSED {
        "paused"
    } else {
        "not extracted"
    };
    println!();
    println!(
        "{ERROR}Stopped at archive {} of {}.{ERROR:#}",
        done + 1,
        archives.len()
    );
    for (i, a) in archives.iter().enumerate() {
        let state = match i.cmp(&done) {
            std::cmp::Ordering::Less => "extracted",
            std::cmp::Ordering::Equal if saved => &format!("{stopped}, progress saved"),
            std::cmp::Ordering::Equal => stopped,
            std::cmp::Ordering::Greater => "not started",
        };
        println!("  {}  {DIM}({state}){DIM:#}", a.display());
    }
    println!(
        "To continue, run extract again with the archives that are not extracted. An archive \
         with saved progress resumes where it stopped."
    );
}

fn extract(
    archive: &Path,
    output: Option<PathBuf>,
    secret: Secret,
    opts: &Options,
) -> Result<Ended> {
    let keep_free = opts.keep_free;
    let no_space = Ok(Ended::Stopped {
        code: EXIT_NO_SPACE,
        saved: false,
    });
    // Refused before the writable open, which would finish a truncation
    // the last run left pending.
    if opts.keep {
        let (_, paths) = locate(archive)?;
        if paths.exists() {
            session::keeps_archive(&Journal::load(&paths)?.0, true)?;
        }
    }
    let (state, paths) = open(archive, output, true, secret)?;
    let (job, journal) = match state {
        State::Orphan(journal) => {
            engine::finish_orphaned(&journal, &paths)?;
            if journal.keep {
                say!("This extraction was started with --keep-archive; the archive is kept.");
            }
            say!(
                "{GOOD}Extraction complete.{GOOD:#} Files are in {}.",
                journal.output.display()
            );
            return Ok(Ended::Extracted);
        }
        State::Resume(job, journal) => {
            let keep = session::keeps_archive(&journal, opts.keep)?;
            if keep && !opts.keep {
                say!("This extraction was started with --keep-archive; the archive is kept.");
            }
            if !report(&job, &journal.output, Some(&journal), keep_free, keep)? {
                return Ok(Ended::Stopped {
                    code: EXIT_NO_SPACE,
                    saved: true,
                });
            }
            session::clear_uncommitted(&job, &journal.output)?;
            (job, journal)
        }
        State::Fresh(job, out) => {
            in_place_possible(&job, opts.keep)?;
            let found = session::existing(&job, &out)?;
            if !found.is_empty() && !opts.replace && !opts.skip_existing {
                return Err(refuse_existing(&out, &found));
            }
            if !report(&job, &out, None, keep_free, opts.keep)? {
                return no_space;
            }
            if !found.is_empty() && opts.skip_existing {
                say!(
                    "  {DIM}note: {} will be kept instead of the archive's copies{DIM:#}",
                    existing_files(found.len())
                );
            } else if !found.is_empty() {
                say!(
                    "  {DIM}note: {} will be replaced, and deleted when the extraction completes{DIM:#}",
                    existing_files(found.len())
                );
            }
            if opts.verify_first
                && let Err(e) = verify(&job)
            {
                eprintln!("{ERROR}error:{ERROR:#} {e:#}");
                return Ok(Ended::Stopped {
                    code: EXIT_DAMAGED,
                    saved: false,
                });
            }
            match check_sfv(&job.storage) {
                Ok(Some(sfv)) => {
                    let name = sfv.file_name().unwrap_or_default().to_string_lossy();
                    say!("{GOOD}Checksums match:{GOOD:#} volumes verified against {name}.");
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("{ERROR}error:{ERROR:#} {e:#}");
                    return Ok(Ended::Stopped {
                        code: if STOP.load(Ordering::Relaxed) {
                            EXIT_PAUSED
                        } else {
                            EXIT_DAMAGED
                        },
                        saved: false,
                    });
                }
            }
            // Nothing is removed when the archive is kept, so there is
            // nothing to confirm.
            if !opts.yes && !opts.keep {
                confirm(&display_name(&job), &out)?;
            }
            let choices = hatchzip_core::replace::Choices {
                keep: if opts.skip_existing {
                    found
                } else {
                    Vec::new()
                },
                both: Vec::new(),
            };
            let replace = (opts.replace || opts.skip_existing).then_some(&choices);
            let journal = session::begin(&job, &paths, out, opts.keep, replace)?;
            (job, journal)
        }
    };

    let keep = journal.keep;
    let bar = progress_bar(journal.total_size, "Extracting");
    bar.set_position(journal.total_size.saturating_sub(job.remaining_size()));
    if !keep {
        bar.set_message(format!("archive: {}", human(journal.committed_len)));
    }

    let output = journal.output.clone();
    let count = job.item_count();
    let (b1, b2) = (bar.clone(), bar.clone());
    let run = Run {
        job,
        journal,
        paths,
        stop: &STOP,
        progress: Progress {
            on_bytes: Box::new(move |n| b1.inc(n)),
            on_commit: Box::new(move |len| b2.set_message(format!("archive: {}", human(len)))),
        },
        keep_free,
    };
    let outcome = run.execute();
    bar.finish_and_clear();

    match outcome {
        Ok(Outcome::Finished) => {
            let archive = if keep {
                "The archive was kept."
            } else {
                "The archive was removed."
            };
            say!(
                "{GOOD}Extraction complete:{GOOD:#} {} in {}. {archive}",
                entries(count),
                output.display()
            );
            Ok(Ended::Extracted)
        }
        Ok(Outcome::Paused) => {
            if !opts.several {
                say!("Extraction paused. Run the same command to resume.");
            }
            Ok(Ended::Stopped {
                code: EXIT_PAUSED,
                saved: true,
            })
        }
        Err(e) => {
            eprintln!("{ERROR}error:{ERROR:#} {e:#}");
            if !opts.several {
                eprintln!("Progress is saved. Run the same command to resume.");
            }
            // A file whose password differs from the one proven at the
            // start, possible in RAR 4, is still a password problem.
            let wrong = e.chain().any(|c| c.is::<WrongPassword>());
            Ok(Ended::Stopped {
                code: if wrong { EXIT_PASSWORD } else { EXIT_FAILURE },
                saved: true,
            })
        }
    }
}

fn verify(job: &Job) -> Result<()> {
    let bar = progress_bar(job.remaining_size(), "Verifying");
    let b = bar.clone();
    let result = job.verify(&move |n| b.inc(n));
    bar.finish_and_clear();
    result.context("the archive is damaged; no files were extracted")
}

fn confirm(archive: &str, output: &Path) -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!(
            "HatchZip removes the archive during extraction; pass --yes to confirm, or \
             --keep-archive to keep it"
        );
    }
    print!(
        "\nExtract to {} and remove {BOLD}{archive}{BOLD:#} during extraction? [y/N] ",
        output.display()
    );
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        bail!("extraction cancelled. The archive was not modified");
    }
    Ok(())
}

fn progress_bar(total: u64, prefix: &'static str) -> ProgressBar {
    if QUIET.load(Ordering::Relaxed) {
        return ProgressBar::hidden();
    }
    let bar = ProgressBar::new(total);
    bar.set_style(
        ProgressStyle::with_template(
            "{prefix} [{bar:32}] {binary_bytes}/{binary_total_bytes}  {binary_bytes_per_sec}  eta {eta}  {msg}",
        )
        .expect("valid template")
        .progress_chars("=> "),
    );
    bar.set_prefix(prefix);
    bar.enable_steady_tick(Duration::from_millis(200));
    bar
}
