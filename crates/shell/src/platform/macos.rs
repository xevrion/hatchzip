//! Finder's right-click menu, through Quick Actions: one Automator workflow
//! bundle per action in `~/Library/Services`, each running a one line shell
//! script. Their titles all start with "HatchZip:", so they sit together in
//! Finder's Quick Actions submenu.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};

use crate::{ACTIONS, Action, Dirs};

const MARK: &str = "Added by HatchZip; `hatchzip-gui --unregister-shell` removes it.";

/// What Automator writes for a Quick Action that takes files in Finder and
/// runs one "Run Shell Script" action with them as arguments.
const WORKFLOW: &str = include_str!("macos-quick-action.wflow");

/// The types the actions are offered on. A file matches a type it conforms
/// to, so `org.gnu.gnu-zip-archive` also covers `.tgz`. `.001` has no
/// declared type, so it gets the dynamic one macOS derives from the
/// extension.
const FILE_TYPES: &[&str] = &[
    "public.zip-archive",
    "org.7-zip.7-zip-archive",
    "public.tar-archive",
    "org.gnu.gnu-zip-archive",
    "org.gnu.gnu-zip-tar-archive",
    "public.bzip2-archive",
    "public.tar-bzip2-archive",
    "org.tukaani.xz-archive",
    "org.tukaani.tar-xz-archive",
    "com.facebook.zstandard-archive",
    "com.facebook.zstandard-tar-archive",
    "dyn.ah62d4rv4ge8xaqbv",
];
/// `.r00` has the same type; HatchZip.app declares both extensions.
#[cfg(feature = "rar")]
const RAR_TYPES: &[&str] = &["com.rarlab.rar-archive"];
#[cfg(not(feature = "rar"))]
const RAR_TYPES: &[&str] = &[];

pub fn current_dirs() -> Result<Dirs> {
    let home = std::env::home_dir().ok_or_else(|| anyhow!("no home folder"))?;
    Ok(Dirs {
        data: home.join("Library/Services"),
    })
}

fn bundle(dirs: &Dirs, id: &str) -> PathBuf {
    dirs.data.join(format!("hatchzip-{id}.workflow"))
}

/// Where a bundle is put together before it replaces the old one. The
/// leading dot and missing `.workflow` keep the services scan away from it.
fn staging(dirs: &Dirs, id: &str) -> PathBuf {
    dirs.data.join(format!(".hatchzip-{id}.tmp"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// `HatchZip.app` when `gui` is the executable inside it.
fn app_bundle(gui: &Path) -> Option<&Path> {
    let macos = gui.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app")
        .then_some(app)
}

/// The script a Quick Action runs, with the selected files as its
/// arguments.
fn script(gui: &Path, action: &Action) -> String {
    let run = match app_bundle(gui) {
        // Launched through Launch Services, the app runs on its own instead
        // of as a child the Quick Action waits for. `-n` because `open`
        // passes `--args` only to an app it starts, and would just bring an
        // open HatchZip forward.
        Some(app) => format!(
            "exec /usr/bin/open -n -a {} --args {}",
            sh_quote(app),
            action.flag
        ),
        // A build run from outside an app bundle.
        None => format!("exec {} {}", sh_quote(gui), action.flag),
    };
    format!("# {MARK}\n[ \"$#\" -gt 0 ] || exit 0\n{run} \"$@\"\n")
}

fn info_plist(action: &Action) -> String {
    let types: String = FILE_TYPES
        .iter()
        .chain(RAR_TYPES)
        .map(|t| format!("\t\t\t\t<string>{t}</string>\n"))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- {MARK} -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>NSServices</key>\n\
         \t<array>\n\
         \t\t<dict>\n\
         \t\t\t<key>NSIconName</key>\n\
         \t\t\t<string>NSActionTemplate</string>\n\
         \t\t\t<key>NSMenuItem</key>\n\
         \t\t\t<dict>\n\
         \t\t\t\t<key>default</key>\n\
         \t\t\t\t<string>HatchZip: {}</string>\n\
         \t\t\t</dict>\n\
         \t\t\t<key>NSMessage</key>\n\
         \t\t\t<string>runWorkflowAsService</string>\n\
         \t\t\t<key>NSRequiredContext</key>\n\
         \t\t\t<dict>\n\
         \t\t\t\t<key>NSApplicationIdentifier</key>\n\
         \t\t\t\t<string>com.apple.finder</string>\n\
         \t\t\t</dict>\n\
         \t\t\t<key>NSSendFileTypes</key>\n\
         \t\t\t<array>\n\
         {types}\
         \t\t\t</array>\n\
         \t\t</dict>\n\
         \t</array>\n\
         </dict>\n\
         </plist>\n",
        xml_escape(action.label)
    )
}

fn remove_dir(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Builds the bundle next to its final place and renames it in, so the
/// services scan never reads a half written one.
fn write_bundle(dirs: &Dirs, gui: &Path, action: &Action) -> Result<()> {
    let tmp = staging(dirs, action.id);
    remove_dir(&tmp)?;
    let contents = tmp.join("Contents");
    fs::create_dir_all(&contents).with_context(|| format!("creating {}", contents.display()))?;
    let workflow = WORKFLOW.replace("@COMMAND@", &xml_escape(&script(gui, action)));
    for (name, text) in [
        ("Info.plist", info_plist(action)),
        ("document.wflow", workflow),
    ] {
        let path = contents.join(name);
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    let dest = bundle(dirs, action.id);
    remove_dir(&dest)?;
    fs::rename(&tmp, &dest).with_context(|| format!("creating {}", dest.display()))
}

/// Finder reads Quick Actions from a cache. Best effort: when this fails,
/// the change shows once macOS rebuilds the cache by itself.
fn refresh_services() {
    for arg in ["-flush", "-update"] {
        let _ = Command::new("/System/Library/CoreServices/pbs")
            .arg(arg)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

pub fn register(dirs: &Dirs, gui: &Path) -> Result<Vec<String>> {
    for a in ACTIONS {
        write_bundle(dirs, gui, a)?;
    }
    refresh_services();
    Ok(vec![format!(
        "Finder Quick Actions: {}",
        dirs.data.display()
    )])
}

pub fn unregister(dirs: &Dirs) -> Result<()> {
    for a in ACTIONS {
        remove_dir(&staging(dirs, a.id))?;
        remove_dir(&bundle(dirs, a.id))?;
    }
    refresh_services();
    Ok(())
}

pub fn is_registered(dirs: &Dirs) -> bool {
    ACTIONS
        .iter()
        .all(|a| bundle(dirs, a.id).join("Contents/document.wflow").exists())
}
