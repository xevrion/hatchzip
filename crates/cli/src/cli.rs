//! Command line definition. Also the source of the shell completions and the
//! man page, so the three never disagree.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use clap_complete::Shell;

pub const EXIT_FAILURE: u8 = 1;
pub const EXIT_NO_SPACE: u8 = 3;
pub const EXIT_DAMAGED: u8 = 4;
pub const EXIT_PASSWORD: u8 = 5;
pub const EXIT_PAUSED: u8 = 130;

const ABOUT: &str = "Extract large archives in place, removing the archive progressively.";

/// The long help, ending with the formats this build reads.
macro_rules! long_about {
    ($formats:literal) => {
        concat!(
            "\
Extract large archives in place, removing the archive progressively.

HatchZip extracts an archive from its end and truncates each completed part from \
the archive file, so the released space is reused for the next files. Free space \
is required for the extracted files only, not for the files and the archive. The \
space plan is checked before the archive is modified, and an interrupted \
extraction resumes where it stopped when the same command is run again.

With --keep-archive, HatchZip extracts like any other tool and leaves the archive \
unchanged; free space is then required for all extracted files.

Supported: ZIP (including ZIP64 and self-extracting .exe), 7z, uncompressed tar, \
gzip-compressed tar (.tar.gz, .tgz), xz-compressed tar (.tar.xz, .txz), split archives \
(.001, .002, ...) of any of those",
            $formats
        )
    };
}

#[cfg(feature = "rar")]
const LONG_ABOUT: &str = long_about!(
    ", and RAR, single files and volume sets (.part1.rar, .part2.rar, ... or .rar, \
     .r00, ...). A solid RAR archive is extracted only with --keep-archive."
);

#[cfg(not(feature = "rar"))]
const LONG_ABOUT: &str = long_about!(
    ". This build does not include RAR support; the release builds from hatchzip.dev \
     and github.com/xevrion/hatchzip/releases do."
);

const AFTER_LONG_HELP: &str = "\
Examples:
  hatchzip check game.zip              Show the space plan without modifying anything
  hatchzip extract game.zip            Extract to ./game, removing game.zip during extraction
  hatchzip extract game.zip --keep-archive
                                       Extract to ./game and keep game.zip unchanged
  hatchzip check game.zip --keep-archive
                                       Show whether a standard extraction fits
  hatchzip x game.7z.001 -o D:\\Games   Extract a split archive from any of its parts
  hatchzip extract patch.zip -o Game --replace
                                       Extract over an existing folder, replacing its files
  hatchzip extract a.zip b.7z c.tar.gz Extract several archives, one after another
  hatchzip extract big.zip --verify-first --keep-free 10GB
  hatchzip test game.7z                Verify every entry without writing files
  hatchzip restore game.zip            Restore a partially extracted archive

To resume an interrupted extraction, run the same command again. Progress is \
stored in files next to the archive (<archive>.hatchzip, <archive>.hatchzip-tail \
and, for an archive with folder entries, <archive>.hatchzip-folders); do not \
delete them while an extraction is incomplete. \
To stop permanently instead, run restore: the entries not yet extracted become a \
standard archive again. An extraction with --keep-archive resumes the same way and \
keeps the archive when resumed.

Several archives are processed one after another. extract checks the space for each \
one just before it starts and stops at the first one that does not complete, then \
lists what was extracted and what was not. check, test and list process every archive.

Exit status:
  0    success
  1    failure (progress, if any, is saved)
  2    invalid command line
  3    insufficient disk space; the archive was not modified
  4    the archive is damaged; no files were removed
  5    the archive is password protected and no correct password was provided
  130  paused with Ctrl+C; run the same command to resume
With several archives, the status is that of the first archive that did not succeed.

Encrypted archives (ZIP with ZipCrypto or AES, 7z with AES): HatchZip prompts for \
the password, or reads --password or the HATCHZIP_PASSWORD environment variable. The \
password is verified on one file before the archive is modified. It is never \
stored, so resuming an interrupted extraction prompts again.";

#[derive(Parser)]
#[command(
    name = "hatchzip",
    version,
    about = ABOUT,
    long_about = LONG_ABOUT,
    after_long_help = AFTER_LONG_HELP,
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Print only errors and prompts
    #[arg(short, long, global = true)]
    pub quiet: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Extract an archive, removing it progressively
    #[command(
        visible_alias = "x",
        long_about = "Extract an archive, removing it progressively.\n\n\
            Prints the space plan and asks for confirmation before starting. After an \
            interruption by Ctrl+C, a crash or a full disk, run the same command to resume.\n\n\
            With --keep-archive the archive is not modified and no confirmation is needed; \
            free space is required for all extracted files.\n\n\
            Files already in the destination folder are never overwritten unless --replace \
            is given. Then they are moved into a .hatchzip-replaced folder there before \
            anything is extracted, and deleted when the extraction completes. restore moves \
            them back, except where a file extracted in place, and no longer in the archive, \
            took their path. A folder is never replaced.\n\n\
            Several archives are extracted one after another. The space for each one is \
            checked just before it starts, and the first one that does not complete stops \
            the run."
    )]
    Extract {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        keep: Keep,
        #[command(flatten)]
        replace: Replace,
        /// Verify every entry before modifying the archive
        #[arg(long)]
        verify_first: bool,
        #[command(flatten)]
        space: Space,
        /// Skip the confirmation prompt (required outside a terminal, unless
        /// --keep-archive is given)
        #[arg(short, long)]
        yes: bool,
    },
    /// Show the space plan for an archive without modifying it
    #[command(visible_alias = "c")]
    Check {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        keep: Keep,
        #[command(flatten)]
        replace: Replace,
        #[command(flatten)]
        space: Space,
    },
    /// List the entries of an archive
    #[command(visible_alias = "l")]
    List {
        /// The archive, or any part of a split archive. Several can be given
        #[arg(required = true, value_name = "ARCHIVE")]
        archives: Vec<PathBuf>,
        #[command(flatten)]
        secret: Secret,
    },
    /// Verify every entry of an archive without writing files
    #[command(visible_alias = "t")]
    Test {
        /// The archive, or any part of a split archive. Several can be given
        #[arg(required = true, value_name = "ARCHIVE")]
        archives: Vec<PathBuf>,
        #[command(flatten)]
        secret: Secret,
    },
    /// Restore a partially extracted archive to a standard archive
    #[command(
        long_about = "Restore a partially extracted archive to a standard archive.\n\n\
            An interrupted extraction leaves the archive containing only the entries not yet \
            extracted, with its index saved alongside, so only HatchZip can open it. restore \
            appends a new index for those entries, so 7-Zip, WinRAR, unzip and tar can open it \
            again. No data is moved; only the space for the new index is required. Files \
            already extracted remain in the destination folder. If restore is interrupted, run \
            it again, or run extract to resume the extraction instead.\n\n\
            An extraction started with --keep-archive never modified the archive, so restore \
            only discards its progress."
    )]
    Restore {
        /// The partially extracted archive, or any part of a split archive
        archive: PathBuf,
        /// The destination folder the extraction was started with, if it was
        /// not the default one
        #[arg(short, long, value_name = "DIR")]
        output: Option<PathBuf>,
    },
    /// Print a shell completion script
    #[command(long_about = "Print a shell completion script.\n\n\
        bash:        hatchzip completions bash > ~/.local/share/bash-completion/completions/hatchzip\n\
        zsh:         hatchzip completions zsh > \"${fpath[1]}/_hatchzip\"\n\
        fish:        hatchzip completions fish > ~/.config/fish/completions/hatchzip.fish\n\
        PowerShell:  hatchzip completions powershell >> $PROFILE")]
    Completions { shell: Shell },
    /// Print the man page (roff)
    #[command(long_about = "Print the man page in roff format.\n\n\
        hatchzip man > ~/.local/share/man/man1/hatchzip.1")]
    Man,
}

#[derive(Args)]
pub struct Target {
    /// The archive, or any part of a split archive. Several can be given
    #[arg(required = true, value_name = "ARCHIVE")]
    pub archives: Vec<PathBuf>,
    /// Destination folder [default: a folder named after the archive, next to it].
    /// With several archives, each one is extracted into this folder
    #[arg(short, long, value_name = "DIR")]
    pub output: Option<PathBuf>,
    #[command(flatten)]
    pub secret: Secret,
}

#[derive(Args)]
pub struct Keep {
    /// Keep the archive: extract without removing anything, like a standard
    /// extraction. Free space is required for all extracted files
    #[arg(long)]
    pub keep_archive: bool,
}

#[derive(Args)]
pub struct Replace {
    /// Replace files that already exist in the destination folder. They are
    /// moved into a .hatchzip-replaced folder there and deleted when the
    /// extraction completes
    #[arg(long)]
    pub replace: bool,
    /// Keep files that already exist in the destination folder. The
    /// archive's copies of them are extracted and then discarded, so when
    /// the archive is removed, they are not kept anywhere
    #[arg(long, conflicts_with = "replace")]
    pub skip_existing: bool,
}

#[derive(Args, Clone)]
pub struct Secret {
    /// Password for an encrypted archive. If omitted, HatchZip prompts for it
    /// with hidden input, which is preferred: this value is visible to other programs
    #[arg(
        short,
        long,
        env = "HATCHZIP_PASSWORD",
        hide_env_values = true,
        value_name = "PASSWORD"
    )]
    pub password: Option<String>,
}

#[derive(Args)]
pub struct Space {
    /// Disk space reserved during extraction, so the disk never fills completely
    #[arg(long, value_name = "SIZE", default_value = "2GB", value_parser = parse_size)]
    pub keep_free: u64,
}

/// Sizes like `500MB`, `2GB` or `1.5TB`, in powers of 1000 to match what
/// HatchZip prints.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num
        .parse()
        .map_err(|_| format!("'{s}' is not a valid size, such as 500MB or 2GB"))?;
    let mult = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "K" | "KB" => 1e3,
        "M" | "MB" => 1e6,
        "G" | "GB" => 1e9,
        "T" | "TB" => 1e12,
        _ => return Err(format!("unknown unit in '{s}'. Use B, KB, MB, GB or TB")),
    };
    Ok((num * mult) as u64)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("500MB"), Ok(500_000_000));
        assert_eq!(parse_size("2gb"), Ok(2_000_000_000));
        assert_eq!(parse_size("1.5 TB"), Ok(1_500_000_000_000));
        assert!(parse_size("ten").is_err());
        assert!(parse_size("5PB").is_err());
    }
}
