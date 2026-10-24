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

const LONG_ABOUT: &str = "\
Extract large archives in place, removing the archive progressively.

HatchZip extracts an archive from its end and truncates each completed part from \
the archive file, so the released space is reused for the next files. Free space \
is required for the extracted files only, not for the files and the archive. The \
space plan is checked before the archive is modified, and an interrupted \
extraction resumes where it stopped when the same command is run again.

Supported: ZIP (including ZIP64 and self-extracting .exe), 7z, uncompressed tar, \
gzip-compressed tar (.tar.gz, .tgz), split archives (.001, .002, ...) of any of \
those, and RAR volume sets (.part1.rar, .part2.rar, ... or .rar, .r00, ...) that \
are not solid.";

const AFTER_LONG_HELP: &str = "\
Examples:
  hatchzip check game.zip              Show the space plan without modifying anything
  hatchzip extract game.zip            Extract to ./game, removing game.zip during extraction
  hatchzip x game.7z.001 -o D:\\Games   Extract a split archive from any of its parts
  hatchzip extract big.zip --verify-first --keep-free 10GB
  hatchzip test game.7z                Verify every entry without writing files
  hatchzip restore game.zip            Restore a partially extracted archive

To resume an interrupted extraction, run the same command again. Progress is \
stored in two files next to the archive (<archive>.hatchzip and \
<archive>.hatchzip-tail); do not delete them while an extraction is incomplete. \
To stop permanently instead, run restore: the entries not yet extracted become a \
standard archive again.

Exit status:
  0    success
  1    failure (progress, if any, is saved)
  2    invalid command line
  3    insufficient disk space; the archive was not modified
  4    the archive is damaged; no files were removed
  5    the archive is password protected and no correct password was provided
  130  paused with Ctrl+C; run the same command to resume

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
            interruption by Ctrl+C, a crash or a full disk, run the same command to resume."
    )]
    Extract {
        #[command(flatten)]
        target: Target,
        /// Verify every entry before modifying the archive
        #[arg(long)]
        verify_first: bool,
        #[command(flatten)]
        space: Space,
        /// Skip the confirmation prompt (required outside a terminal)
        #[arg(short, long)]
        yes: bool,
    },
    /// Show the space plan for an archive without modifying it
    #[command(visible_alias = "c")]
    Check {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        space: Space,
    },
    /// List the entries of an archive
    #[command(visible_alias = "l")]
    List {
        /// The archive, or any part of a split archive
        archive: PathBuf,
        #[command(flatten)]
        secret: Secret,
    },
    /// Verify every entry of an archive without writing files
    #[command(visible_alias = "t")]
    Test {
        /// The archive, or any part of a split archive
        archive: PathBuf,
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
            it again, or run extract to resume the extraction instead."
    )]
    Restore {
        /// The partially extracted archive, or any part of a split archive
        archive: PathBuf,
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
    /// The archive, or any part of a split archive
    pub archive: PathBuf,
    /// Destination folder [default: a folder named after the archive, next to it]
    #[arg(short, long, value_name = "DIR")]
    pub output: Option<PathBuf>,
    #[command(flatten)]
    pub secret: Secret,
}

#[derive(Args)]
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
