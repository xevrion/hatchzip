use std::cell::RefCell;
use std::path::PathBuf;

use objc2::ffi;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::sel;
use objc2_app_kit::NSApplication;
use objc2_foundation::{MainThreadMarker, NSArray, NSURL};

/// The settings switch, named after this system's file manager.
pub const SHELL_MENU_NAME: super::ShellMenuName = super::ShellMenuName::Finder;

/// hatchzip-shell can't add Finder entries yet, so the setting is hidden.
pub const SHELL_MENU: bool = false;

/// The menu bar is the system's, at the top of the screen.
pub const IN_WINDOW_MENU: bool = false;

/// The faces with real bold weights for these scripts. `""` keeps the
/// system font.
///
/// Chinese: fontique cannot draw the outlines PingFang ships in on macOS,
/// so asking for it falls through to Heiti SC, whose Light and Medium are
/// weights 300 and 400: text and headings both drew at 400. Hiragino Sans
/// GB is in every macOS and has W3 (300) and W6 (600), so text gets W3 and
/// a 600 heading gets W6. Devanagari: the fallback has tight line metrics.
pub fn font_family(language: &str) -> &'static str {
    match language {
        "zh_CN" => "Hiragino Sans GB",
        "hi" => "Kohinoor Devanagari",
        _ => "",
    }
}

/// `~/Library/Application Support/HatchZip`.
pub fn config_dir() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join("Library/Application Support/HatchZip"))
}

type OnOpen = Box<dyn Fn(PathBuf)>;

thread_local! {
    static ON_OPEN: RefCell<Option<OnOpen>> = const { RefCell::new(None) };
}

/// Finder hands the files of "Open With" and double-click to the app as an
/// Apple event, never as arguments. winit's application delegate doesn't
/// answer `application:openURLs:`, so AppKit falls back to its document
/// system, which refuses with "HatchZip cannot open files in the Archive
/// format". This teaches the delegate that method, passing each file to
/// `on_open`. Call it once the event loop exists, which is when winit sets
/// its delegate, and before it runs, when the event arrives.
pub fn forward_opened_files(on_open: impl Fn(PathBuf) + 'static) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let Some(delegate) = app.delegate() else {
        return;
    };
    ON_OPEN.with(|f| *f.borrow_mut() = Some(Box::new(on_open)));
    let object: &AnyObject = delegate.as_ref();
    let class: *const AnyClass = object.class();
    type OpenUrls = extern "C-unwind" fn(&AnyObject, Sel, &NSApplication, &NSArray<NSURL>);
    // SAFETY: the function takes the receiver, the selector and two objects
    // and returns nothing, as the type encoding says. A delegate that has the
    // method already, such as from a later winit, keeps its own.
    unsafe {
        let imp = std::mem::transmute::<OpenUrls, Imp>(open_urls);
        let _ = ffi::class_addMethod(
            class.cast_mut(),
            sel!(application:openURLs:),
            imp,
            c"v@:@@".as_ptr(),
        );
    }
}

extern "C-unwind" fn open_urls(
    _this: &AnyObject,
    _cmd: Sel,
    _app: &NSApplication,
    urls: &NSArray<NSURL>,
) {
    for url in urls.iter() {
        if let Some(path) = url.to_file_path() {
            ON_OPEN.with(|f| {
                if let Some(f) = &*f.borrow() {
                    f(path);
                }
            });
        }
    }
}
