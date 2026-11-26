mod cli;
mod json;

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
    only: Option<&[PathBuf]>,
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
        only,
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
            nested,
            only,
        } => {
            let opts = Options {
                // The entries left out stay in the archive, so it is kept.
                keep: keep.keep_archive || !only.is_empty(),
                implied_keep: !keep.keep_archive && !only.is_empty(),
                only: (!only.is_empty()).then_some(only),
                replace: replace.replace,
                skip_existing: replace.skip_existing,
                verify_first,
                keep_free: space.keep_free,
                yes,
                several: false,
                nested,
            };
            extract_all(target, opts)
        }
        Command::Check {
            target,
            keep,
            replace,
            space,
            json: as_json,
        } => {
            if as_json {
                check_json_all(target, keep.keep_archive, space)
            } else {
                check_all(target, keep.keep_archive, replace.replace, space)
            }
        }
        Command::List {
            archives,
            secret,
            json: as_json,
        } => {
            let archives = distinct(&archives)?;
            if as_json {
                list_json_all(&archives, secret)
            } else {
                Ok(each(&archives, true, |a| list(a, secret.clone())))
            }
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
/// once, so `game.7z.*` from a shell glob means the one archive. The glob
/// also matches the progress files next to the first volume
/// (`game.z01.hatchzip`, ...), which are left out when their archive is
/// given too.
/// Whether every volume of the set whose first volume is `primary` that
/// exists on disk is in `written`. A set that is not split has one volume.
fn volumes_written(primary: &Path, written: &std::collections::HashSet<PathBuf>) -> bool {
    use hatchzip_core::archive::storage;
    let mut volumes = Vec::new();
    if storage::is_split_zip(primary) {
        let mut disk = 0;
        while let Some(segment) = storage::split_zip_segment(primary, disk) {
            if !segment.exists() {
                break;
            }
            volumes.push(segment);
            disk += 1;
        }
        volumes.extend(storage::split_zip_last(primary));
    } else {
        let mut index = 0;
        while let Some(volume) = storage::volume_path(primary, index) {
            if index > 0 && !volume.exists() {
                break;
            }
            volumes.push(volume);
            index += 1;
        }
    }
    volumes
        .iter()
        .filter(|v| v.exists())
        .all(|v| written.contains(v))
        && written.contains(primary)
}

fn distinct(archives: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let owner = |a: &Path| {
        let name = a.to_str()?;
        [".hatchzip", ".hatchzip-tail", ".hatchzip-folders"]
            .iter()
            .find_map(|s| name.strip_suffix(s))
            .map(PathBuf::from)
    };
    let mut given = std::collections::HashSet::new();
    for a in archives.iter().filter(|a| owner(a.as_path()).is_none()) {
        given.insert(locate(a)?.0);
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for a in archives {
        if let Some(of) = owner(a.as_path())
            && given.contains(&locate(&of)?.0)
        {
            continue;
        }
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

fn check_json_all(target: Target, keep: bool, space: Space) -> Result<u8> {
    let archives = distinct(&target.archives)?;
    let mut results = Vec::with_capacity(archives.len());
    let mut first_code = 0;

    for archive in &archives {
        let (result, code) = match open(
            archive,
            target.output.clone(),
            None,
            false,
            target.secret.clone(),
        ) {
            Ok((State::Fresh(job, out), _)) => {
                let metadata = json::ArchiveMetadata {
                    archive_path: json::archive_path(archive),
                    state: Some(json::ArchiveState::Fresh),
                    format: Some(job.format_name.to_string()),
                    archive_size_bytes: Some(job.storage.original_len()),
                    entry_count: Some(job.item_count()),
                    remaining_uncompressed_size_bytes: Some(job.remaining_size()),
                };

                let refusal = job.in_place_refusal();

                match session::plan(&job, &out, false, space.keep_free, keep) {
                    Ok(plan) => {
                        let code = match in_place_possible(&job, keep) {
                            Err(error) => fail(&error),
                            Ok(()) if plan.fits => 0,
                            Ok(()) => EXIT_NO_SPACE,
                        };

                        (
                            json::CheckArchive {
                                archive: metadata,
                                space_plan: Some(json::CheckSpacePlan {
                                    needed_bytes: plan.needed,
                                    available_bytes: plan.free,
                                    reserved_bytes: plan.keep_free,
                                    fits_in_place: if refusal.is_some() {
                                        None
                                    } else {
                                        Some(plan.in_place_fits())
                                    },
                                    fits_with_archive_kept: Some(plan.normal_fits()),
                                    in_place_refusal_reason: refusal,
                                }),
                                error: None,
                            },
                            code,
                        )
                    }
                    Err(error) => check_json_failure(metadata, error),
                }
            }

            Ok((State::Resume(job, journal), _)) => {
                let metadata = json::ArchiveMetadata {
                    archive_path: json::archive_path(archive),
                    state: Some(json::ArchiveState::Resuming),
                    format: Some(journal.format.clone()),
                    archive_size_bytes: None,
                    entry_count: Some(job.item_count()),
                    remaining_uncompressed_size_bytes: Some(job.remaining_size()),
                };

                let refusal = job.in_place_refusal();

                match session::keeps_archive(&journal, keep) {
                    Err(error) => check_json_failure(metadata, error),

                    Ok(keep) => {
                        match session::plan(&job, &journal.output, true, space.keep_free, keep) {
                            Ok(plan) => {
                                let code = if plan.fits { 0 } else { EXIT_NO_SPACE };

                                (
                                    json::CheckArchive {
                                        archive: metadata,
                                        space_plan: Some(json::CheckSpacePlan {
                                            needed_bytes: plan.needed,
                                            available_bytes: plan.free,
                                            reserved_bytes: plan.keep_free,
                                            fits_in_place: if keep {
                                                None
                                            } else {
                                                Some(plan.fits)
                                            },
                                            fits_with_archive_kept: if keep {
                                                Some(plan.fits)
                                            } else {
                                                None
                                            },
                                            in_place_refusal_reason: refusal,
                                        }),
                                        error: None,
                                    },
                                    code,
                                )
                            }
                            Err(error) => check_json_failure(metadata, error),
                        }
                    }
                }
            }

            Ok((State::Orphan(journal), _)) => (
                json::CheckArchive {
                    archive: json::ArchiveMetadata {
                        archive_path: json::archive_path(archive),
                        state: Some(json::ArchiveState::Resuming),
                        format: Some(journal.format),
                        archive_size_bytes: None,
                        entry_count: Some(0),
                        remaining_uncompressed_size_bytes: Some(0),
                    },
                    space_plan: None,
                    error: None,
                },
                0,
            ),

            Err(error) => {
                let metadata = json::ArchiveMetadata {
                    archive_path: json::archive_path(archive),
                    state: None,
                    format: None,
                    archive_size_bytes: None,
                    entry_count: None,
                    remaining_uncompressed_size_bytes: None,
                };

                check_json_failure(metadata, error)
            }
        };

        if first_code == 0 && code != 0 {
            first_code = code;
        }

        results.push(result);
    }

    let stdout = io::stdout();
    let mut output = stdout.lock();

    serde_json::to_writer(&mut output, &json::JsonDocument::new(results))?;
    writeln!(output)?;

    Ok(first_code)
}

fn check_json_failure(
    metadata: json::ArchiveMetadata,
    error: anyhow::Error,
) -> (json::CheckArchive, u8) {
    let message = format!("{error:#}");
    let code = fail(&error);

    (
        json::CheckArchive {
            archive: metadata,
            space_plan: None,
            error: Some(message),
        },
        code,
    )
}

fn check(
    archive: &Path,
    output: Option<PathBuf>,
    secret: Secret,
    (keep, replace): (bool, bool),
    space: &Space,
) -> Result<u8> {
    let (state, _) = open(archive, output, None, false, secret)?;
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
    let job = match open(archive, None, None, false, secret)?.0 {
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

fn list_json_all(archives: &[PathBuf], secret: Secret) -> Result<u8> {
    let mut results = Vec::with_capacity(archives.len());
    let mut first_code = 0;

    for archive in archives {
        let (result, code) = match open(archive, None, None, false, secret.clone()) {
            Ok((State::Fresh(job, _), _)) => {
                (list_json_job(archive, job, json::ArchiveState::Fresh), 0)
            }
            Ok((State::Resume(job, _), _)) => {
                (list_json_job(archive, job, json::ArchiveState::Resuming), 0)
            }
            Ok((State::Orphan(journal), _)) => {
                let error = anyhow::anyhow!("no entries remain in this archive");
                let message = format!("{error:#}");
                let code = fail(&error);

                (
                    json::ListArchive {
                        archive: json::ArchiveMetadata {
                            archive_path: json::archive_path(archive),
                            state: Some(json::ArchiveState::Resuming),
                            format: Some(journal.format.to_string()),
                            archive_size_bytes: None,
                            entry_count: Some(0),
                            remaining_uncompressed_size_bytes: Some(0),
                        },
                        entries: Some(Vec::new()),
                        error: Some(message),
                    },
                    code,
                )
            }
            Err(error) => {
                let message = format!("{error:#}");
                let code = fail(&error);

                (
                    json::ListArchive {
                        archive: json::ArchiveMetadata {
                            archive_path: json::archive_path(archive),
                            state: None,
                            format: None,
                            archive_size_bytes: None,
                            entry_count: None,
                            remaining_uncompressed_size_bytes: None,
                        },
                        entries: None,
                        error: Some(message),
                    },
                    code,
                )
            }
        };

        if first_code == 0 && code != 0 {
            first_code = code;
        }

        results.push(result);
    }

    let stdout = io::stdout();
    let mut output = stdout.lock();

    serde_json::to_writer(&mut output, &json::JsonDocument::new(results))?;
    writeln!(output)?;

    Ok(first_code)
}

fn list_json_job(archive: &Path, job: Job, state: json::ArchiveState) -> json::ListArchive {
    let entries = job
        .units
        .iter()
        .rev()
        .flat_map(|unit| {
            unit.items
                .iter()
                .map(move |item| json::ListEntry::from_item(item, unit.encrypted))
        })
        .collect();

    let archive_size_bytes = match state {
        json::ArchiveState::Fresh => Some(job.storage.original_len()),
        json::ArchiveState::Resuming => None,
    };

    json::ListArchive {
        archive: json::ArchiveMetadata {
            archive_path: json::archive_path(archive),
            state: Some(state),
            format: Some(job.format_name.to_string()),
            archive_size_bytes,
            entry_count: Some(job.item_count()),
            remaining_uncompressed_size_bytes: Some(job.remaining_size()),
        },
        entries: Some(entries),
        error: None,
    }
}

fn test_command(archive: &Path, secret: Secret) -> Result<u8> {
    let job = match open(archive, None, None, false, secret)?.0 {
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
    Extracted {
        output: PathBuf,
        /// Every file this run wrote and the person did not keep, as absolute
        /// paths: what `--nested` may pick archives from.
        written: Vec<PathBuf>,
    },
    /// Stopped with the given status; `saved` if a journal holds progress.
    Stopped { code: u8, saved: bool },
}

#[derive(Clone)]
struct Options {
    keep: bool,
    /// The archive is kept because of `--only`, not `--keep-archive`.
    implied_keep: bool,
    /// Only these entries, and the folders among them, are extracted.
    only: Option<Vec<PathBuf>>,
    replace: bool,
    skip_existing: bool,
    verify_first: bool,
    keep_free: u64,
    yes: bool,
    /// Several archives: the summary says how to continue, not each one.
    several: bool,
    /// After extracting, find the archives inside and extract them too.
    nested: bool,
}

fn extract_all(target: Target, mut opts: Options) -> Result<u8> {
    let archives = distinct(&target.archives)?;
    opts.several = archives.len() > 1;
    if opts.several && opts.only.is_some() {
        bail!("--only selects entries of one archive; give a single archive with it");
    }
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
    let mut outputs: Vec<PathBuf> = Vec::new();
    let mut nested_candidates: Vec<PathBuf> = Vec::new();
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
            Ended::Extracted { output, written } => {
                done += 1;
                if !outputs.contains(&output) {
                    outputs.push(output);
                }
                nested_candidates.extend(written);
            }
            Ended::Stopped { code, saved } => {
                if opts.several {
                    summary(&archives, done, code, saved);
                }
                return Ok(code);
            }
        }
    }
    if opts.nested {
        let parents: Vec<PathBuf> = archives
            .iter()
            .filter_map(|a| locate(a).ok().map(|(p, _)| p))
            .collect();
        let written: std::collections::HashSet<PathBuf> =
            nested_candidates.iter().cloned().collect();
        let mut nested: Vec<PathBuf> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for path in nested_candidates {
            if hatchzip_core::archive::nested_kind(&path).is_none() {
                continue;
            }
            // A split set counts once, by its first volume, and the parent
            // archives are never nested.
            let Ok((primary, _)) = locate(&path) else {
                continue;
            };
            if parents.contains(&primary) || !seen.insert(primary.clone()) {
                continue;
            }
            // Extracting a set in place removes every volume, so each one
            // must be a file this run wrote, never one the person had.
            if !volumes_written(&primary, &written) {
                continue;
            }
            nested.push(path);
        }
        if nested.is_empty() {
            say!("No archives among the extracted files.");
        } else {
            say!();
            say!("{BOLD}Archives inside:{BOLD:#}");
            for path in &nested {
                say!("  {}", path.display());
            }
            if !opts.yes && !opts.keep {
                confirm_nested(nested.len())?;
            }
            let mut nested_opts = opts.clone();
            // A nested run is its own extraction: the parent's replace and
            // skip choices must not touch the files it left.
            nested_opts.replace = false;
            nested_opts.skip_existing = false;
            // One summary for the phase, not one per archive.
            nested_opts.several = true;
            // The list above was the confirmation; each one is not asked
            // again.
            nested_opts.yes = true;
            let mut nested_done = 0;
            for (i, archive) in nested.iter().enumerate() {
                say!();
                if nested.len() == 1 {
                    say!("{BOLD}Nested archive:{BOLD:#} {}", archive.display());
                } else {
                    say!(
                        "{BOLD}Nested archive {} of {}:{BOLD:#} {}",
                        i + 1,
                        nested.len(),
                        archive.display()
                    );
                }
                let ended = match extract(archive, None, target.secret.clone(), &nested_opts) {
                    Ok(e) => e,
                    Err(e) => Ended::Stopped {
                        code: fail(&e),
                        saved: locate(archive).is_ok_and(|(_, p)| p.exists()),
                    },
                };
                match ended {
                    Ended::Extracted { .. } => nested_done += 1,
                    Ended::Stopped { code, saved } => {
                        nested_summary(&nested, nested_done, code, saved);
                        return Ok(code);
                    }
                }
            }
            say!();
            if nested.len() == 1 {
                say!("{GOOD}The nested archive was extracted.{GOOD:#}");
            } else {
                say!(
                    "{GOOD}All {} nested archives extracted.{GOOD:#}",
                    nested.len()
                );
            }
        }
    }
    if opts.several {
        say!();
        say!("{GOOD}All {} archives extracted.{GOOD:#}", archives.len());
    }
    Ok(0)
}

/// What the nested phase did when one archive stopped it. The command that
/// started the phase cannot be rerun to continue, since the parent archive is
/// gone, so this names the archive to run instead.
fn nested_summary(archives: &[PathBuf], done: usize, code: u8, saved: bool) {
    let stopped = if code == EXIT_PAUSED {
        "paused"
    } else {
        "not extracted"
    };
    println!();
    println!(
        "{ERROR}Stopped at nested archive {} of {}.{ERROR:#}",
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
        "To continue, run `hatchzip extract <archive>` on each one that is not extracted. An \
         archive with saved progress resumes where it stopped."
    );
}

/// The phase extracts and removes the archives by default, so it asks once
/// for all of them.
fn confirm_nested(count: usize) -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!(
            "HatchZip removes each nested archive during extraction; pass --yes to confirm, \
             or --keep-archive to keep them"
        );
    }
    let what = if count == 1 {
        "the archive inside".to_string()
    } else {
        format!("{count} archives inside")
    };
    print!("\nExtract {what} and remove them during extraction? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        bail!("extraction cancelled. Nothing was modified");
    }
    Ok(())
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
            let journal = Journal::load(&paths)?.0;
            // --only keeps the archive without --keep-archive being typed,
            // so the advice is about --only.
            if opts.implied_keep && !journal.keep {
                bail!(
                    "this archive is partially extracted in place with every entry, and --only \
                     needs the archive kept. Resume without --only, or run restore first. \
                     Nothing was modified"
                );
            }
            session::keeps_archive(&journal, true)?;
        }
    }
    let (state, paths) = open(archive, output, opts.only.as_deref(), true, secret)?;
    let mut kept: Vec<PathBuf> = Vec::new();
    let (job, journal) = match state {
        State::Orphan(mut journal) => {
            if let Some(warning) = engine::finish_orphaned(&mut journal, &paths)? {
                eprintln!("warning: {warning}");
            }
            if journal.only.is_some() {
                say!("This extraction was started with --only; the archive is kept.");
            } else if journal.keep {
                say!("This extraction was started with --keep-archive; the archive is kept.");
            }
            say!(
                "{GOOD}Extraction complete.{GOOD:#} Files are in {}.",
                journal.output.display()
            );
            return Ok(Ended::Extracted {
                output: journal.output,
                written: Vec::new(),
            });
        }
        State::Resume(job, journal) => {
            let keep = session::keeps_archive(&journal, opts.keep)?;
            if journal.only.is_some() && opts.only.is_none() {
                say!(
                    "This extraction was started with --only; only the selected entries are \
                     extracted, and the archive is kept."
                );
            } else if keep && !opts.keep {
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
            if opts.implied_keep {
                say!(
                    "Only the selected entries are extracted, so the archive is kept (--only implies --keep-archive)."
                );
            }
            let found = session::existing(&job, &out)?;
            if opts.skip_existing {
                kept.clone_from(&found);
            }
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
    // Every file this run may write, before the job moves into the run. Only
    // these are candidates for --nested: a folder scan would find archives
    // the person already had there.
    let candidates: Vec<PathBuf> = job
        .units
        .iter()
        .flat_map(|u| &u.items)
        .filter(|i| i.kind == hatchzip_core::archive::Kind::File)
        .map(|i| i.path.clone())
        .collect();
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
        Ok(Outcome::Finished(warning)) => {
            if let Some(warning) = warning {
                eprintln!("warning: {warning}");
            }
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
            let written = candidates
                .into_iter()
                .filter(|p| !kept.contains(p))
                .filter_map(|p| std::path::absolute(output.join(p)).ok())
                .collect();
            Ok(Ended::Extracted { output, written })
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
