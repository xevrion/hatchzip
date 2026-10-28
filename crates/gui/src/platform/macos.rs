use std::cell::RefCell;
use std::path::PathBuf;

use objc2::ffi;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::sel;
use objc2_app_kit::NSApplication;
use objc2_foundation::{MainThreadMarker, NSArray, NSURL};

/// The settings switch, named after this system's file manager.
pub const SHELL_MENU_LABEL: &str = "Show in the Finder context menu";

/// hatchzip-shell can't add Finder entries yet, so the setting is hidden.
pub const SHELL_MENU: bool = false;

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
