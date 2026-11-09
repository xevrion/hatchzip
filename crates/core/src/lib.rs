//! The engine behind HatchZip: extracts ZIP, 7z, tar, `.tar.gz` and RAR archives,
//! split or not, while truncating the archive behind itself (or, for a
//! `.tar.gz`, punching out what it has decoded), so the disk only ever has to
//! hold about the output.
//!
//! RAR needs the `rar` feature, which compiles UnRAR's C++ source under its
//! own license (`LICENSE-UNRAR`); without it, RAR archives are refused before
//! anything is touched.
//!
//! Front ends start in [`session`]: locate an archive, open it (fresh or
//! half-extracted), get a [`session::SpacePlan`], then hand a [`engine::Run`]
//! the job. [`restore`] turns a half-extracted archive back into a normal
//! one instead.

pub mod archive;
mod chosen;
pub mod engine;
mod folders;
mod fsutil;
pub mod journal;
mod platform;
pub mod replace;
pub mod restore;
pub mod session;
mod sfv;

pub use engine::human;
