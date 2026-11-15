//! Finder gets HatchZip through Quick Actions, which are Automator workflow
//! bundles in `~/Library/Services`. They are not written yet: nobody can
//! test them without a Mac, and a broken one fails silently in Finder.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};

use crate::Dirs;

pub fn current_dirs() -> Result<Dirs> {
    let home = std::env::home_dir().ok_or_else(|| anyhow!("no home folder"))?;
    Ok(Dirs {
        data: home.join("Library/Services"),
        config: PathBuf::new(),
        system_config: Vec::new(),
    })
}

pub fn register(_dirs: &Dirs, _gui: &Path) -> Result<Vec<String>> {
    bail!("the Finder right-click menu is not available on macOS yet")
}

pub fn unregister(_dirs: &Dirs) -> Result<()> {
    Ok(())
}

pub fn is_registered(_dirs: &Dirs) -> bool {
    false
}
