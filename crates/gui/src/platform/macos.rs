use std::cell::RefCell;
use std::path::PathBuf;

use block2::DynBlock;
use objc2::ffi;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Imp, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEventType, NSFilePromiseProvider, NSFilePromiseProviderDelegate, NSView,
    NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSCocoaErrorDomain, NSError, NSOperationQueue, NSPoint, NSRect,
    NSSize, NSString, NSURL,
};
use slint::winit_030::WinitWindowAccessor;
use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::drag::{Drag, Source};

/// The settings switch, named after this system's file manager.
pub const SHELL_MENU_NAME: super::ShellMenuName = super::ShellMenuName::Finder;

/// hatchzip-shell can't add Finder entries yet, so the setting is hidden.
pub const SHELL_MENU: bool = false;

/// The menu bar is the system's, at the top of the screen.
pub const IN_WINDOW_MENU: bool = false;

/// Let Slint pick its default renderer (FemtoVG) when SLINT_BACKEND is unset.
pub const DEFAULT_RENDERER: Option<&str> = None;

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

/// Rows dragged out go to the Finder as file promises.
pub const DRAG_OUT: bool = true;

/// The Finder asks for each entry on a thread of its own once it knows the
/// folder, and the window keeps working meanwhile, so any size goes.
pub const DRAG_LIMIT: Option<u64> = None;

/// One entry the Finder is promised.
struct PromiseIvars {
    source: Source,
    entry: PathBuf,
    name: String,
    /// Where the Finder's request to write it is served: not the main
    /// thread, which would hang the window for as long as it takes.
    queue: Retained<NSOperationQueue>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Promise
    // implements no Drop.
    #[unsafe(super(NSObject))]
    #[name = "HatchZipFilePromise"]
    #[ivars = PromiseIvars]
    struct Promise;

    unsafe impl NSObjectProtocol for Promise {}

    unsafe impl NSFilePromiseProviderDelegate for Promise {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name(
            &self,
            _provider: &NSFilePromiseProvider,
            _file_type: &NSString,
        ) -> Retained<NSString> {
            NSString::from_str(&self.ivars().name)
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write_promise(
            &self,
            _provider: &NSFilePromiseProvider,
            url: &NSURL,
            done: &DynBlock<dyn Fn(*mut NSError)>,
        ) {
            let promise = self.ivars();
            let written = match url.to_file_path() {
                Some(dest) => crate::drag::write_promise(&promise.source, &promise.entry, &dest),
                None => Err(anyhow::anyhow!(
                    "{} was dropped somewhere that is not a folder",
                    promise.name
                )),
            };
            match written {
                Ok(()) => done.call((std::ptr::null_mut(),)),
                Err(e) => {
                    crate::main_window::drag_failed(e);
                    // SAFETY: a constant of Foundation's, there for the
                    // life of the process. 512 is NSFileWriteUnknownError.
                    let error = NSError::new(512, unsafe { NSCocoaErrorDomain });
                    done.call((Retained::as_ptr(&error).cast_mut(),));
                }
            }
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn operation_queue(&self, _provider: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }
    }
);

impl Promise {
    fn new(ivars: PromiseIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        // SAFETY: NSObject's init, on an object just allocated.
        unsafe { msg_send![super(this), init] }
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and DragSource
    // implements no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HatchZipDragSource"]
    struct DragSource;

    unsafe impl NSObjectProtocol for DragSource {}

    unsafe impl NSDraggingSource for DragSource {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn operation_mask(
            &self,
            _session: &NSDraggingSession,
            _context: NSDraggingContext,
        ) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(
            &self,
            _session: &NSDraggingSession,
            _point: NSPoint,
            _operation: NSDragOperation,
        ) {
            crate::main_window::drag_ended();
        }
    }
);

impl DragSource {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's init, on an object just allocated.
        unsafe { msg_send![super(this), init] }
    }
}

thread_local! {
    /// The source of the last drag, until the next one replaces it.
    static SOURCE: RefCell<Option<Retained<DragSource>>> = const { RefCell::new(None) };
    /// A file promise holds its delegate weakly, and the Finder may ask for
    /// the file after the drag is over, so they are kept for the process.
    /// Each is a few words.
    static PROMISES: RefCell<Vec<Retained<Promise>>> = const { RefCell::new(Vec::new()) };
}

/// Starts dragging `drag` out of `window`. AppKit only starts a drag from
/// the mouse event that moves it, which is the one being handled: Slint
/// calls this from inside winit's handling of it.
pub fn drag_out(window: &slint::Window, drag: Drag) -> bool {
    window
        .with_winit_window(|w| start_drag(w, &drag))
        .unwrap_or(false)
}

fn start_drag(window: &slint::winit_030::winit::window::Window, drag: &Drag) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let Some(event) = NSApplication::sharedApplication(mtm).currentEvent() else {
        return false;
    };
    if event.r#type() != NSEventType::LeftMouseDragged {
        return false;
    }
    let Ok(handle) = window.window_handle() else {
        return false;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return false;
    };
    // SAFETY: winit's NSView for this window, alive as long as the window,
    // borrowed here on the main thread while the window handles an event.
    let view: &NSView = unsafe { appkit.ns_view.cast::<NSView>().as_ref() };
    let at = view.convertPoint_fromView(event.locationInWindow(), None);
    let queue = NSOperationQueue::new();
    let workspace = NSWorkspace::sharedWorkspace();
    let mut items = Vec::new();
    for (i, entry) in drag.entries.iter().enumerate() {
        let Some(name) = entry.path.file_name() else {
            continue;
        };
        let promise = Promise::new(PromiseIvars {
            source: drag.source.clone(),
            entry: entry.path.clone(),
            name: name.to_string_lossy().into_owned(),
            queue: queue.clone(),
        });
        let kind = NSString::from_str(if entry.dir {
            "public.folder"
        } else {
            "public.data"
        });
        let provider = NSFilePromiseProvider::initWithFileType_delegate(
            NSFilePromiseProvider::alloc(),
            &kind,
            ProtocolObject::from_ref(&*promise),
        );
        let item = NSDraggingItem::initWithPasteboardWriter(
            NSDraggingItem::alloc(),
            ProtocolObject::from_ref(&*provider),
        );
        let icon_type = match entry.path.extension() {
            _ if entry.dir => "public.folder".to_string(),
            Some(ext) => ext.to_string_lossy().into_owned(),
            None => "public.data".to_string(),
        };
        // The replacement, iconForContentType:, takes a UTType, from
        // bindings this app has no other use for; this one takes an
        // extension or a type identifier and still works.
        #[allow(deprecated)]
        let icon = workspace.iconForFileType(&NSString::from_str(&icon_type));
        let contents: &AnyObject = &icon;
        // A few entries fan out a little, as the Finder's own drags do.
        let step = i.min(4) as f64 * 4.0;
        let frame = NSRect::new(
            NSPoint::new(at.x - 16.0 + step, at.y - 16.0 - step),
            NSSize::new(32.0, 32.0),
        );
        // SAFETY: a dragging frame's contents are an image, as this is.
        unsafe { item.setDraggingFrame_contents(frame, Some(contents)) };
        PROMISES.with(|p| p.borrow_mut().push(promise));
        items.push(item);
    }
    if items.is_empty() {
        return false;
    }
    let source = DragSource::new(mtm);
    let items = NSArray::from_retained_slice(&items);
    let _session = view.beginDraggingSessionWithItems_event_source(
        &items,
        &event,
        ProtocolObject::from_ref(&*source),
    );
    SOURCE.with(|s| *s.borrow_mut() = Some(source));
    true
}
