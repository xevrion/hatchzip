use std::cell::{Cell, RefCell};
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, GlobalFree, HGLOBAL, HWND,
    S_OK,
};
use windows::Win32::System::Com::{
    DVASPECT_CONTENT, FORMATETC, IDataObject, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::Memory::{GHND, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource,
    IDropSource_Impl, OleInitialize, OleUninitialize,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{DROPFILES, SHCreateDataObject};
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};
use windows_core::{BOOL, HRESULT, implement};

use crate::drag::Drag;

/// The settings switch, named after this system's file manager.
pub const SHELL_MENU_NAME: super::ShellMenuName = super::ShellMenuName::Explorer;

/// Explorer, through the per-user registry hive.
pub const SHELL_MENU: bool = true;

/// The menu bar is a native Win32 menu, with its own edge.
pub const IN_WINDOW_MENU: bool = false;

/// The system's UI face for each script. Left to itself, the software
/// renderer drew Latin text in Arial, and Simplified Chinese in a Japanese
/// face that lacks most of its characters.
pub fn font_family(language: &str) -> &'static str {
    match language {
        "zh_CN" => "Microsoft YaHei UI",
        "hi" => "Nirmala UI",
        _ => "Segoe UI",
    }
}

/// `%APPDATA%\HatchZip`, which roams with the user's profile.
pub fn config_dir() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(appdata).join("HatchZip"))
}

/// Explorer passes the files as arguments here, so there is nothing to
/// listen for.
pub fn forward_opened_files(_on_open: impl Fn(PathBuf) + 'static) {}

/// Rows dragged out go to Explorer through OLE, as files.
pub const DRAG_OUT: bool = true;

/// Explorer is handed files that exist, so the entries are extracted when
/// the button is released over a folder that takes them, and the window
/// waits for that. Beyond this much, "Extract selected…", which shows its
/// progress, is the way.
pub const DRAG_LIMIT: Option<u64> = Some(1_000_000_000);

thread_local! {
    static PENDING: RefCell<Option<Drag>> = const { RefCell::new(None) };
}

/// Starts dragging `drag` out once the current event is handled. DoDragDrop
/// runs a message loop of its own until the drop. Called from inside winit's
/// event handler, as Slint's callbacks are, that loop would hand winit a
/// repaint while the handler is busy, which winit refuses with a panic. A
/// thread timer's callback is called by the message loop itself, between
/// events, so the window keeps drawing while the drag is on.
pub fn drag_out(_window: &slint::Window, drag: Drag) -> bool {
    PENDING.with(|p| *p.borrow_mut() = Some(drag));
    // SAFETY: a thread timer, owned by no window, whose callback has the
    // signature TIMERPROC requires.
    let id = unsafe { SetTimer(None, 0, 0, Some(start_pending)) };
    if id == 0 {
        PENDING.with(|p| p.borrow_mut().take());
        return false;
    }
    true
}

unsafe extern "system" fn start_pending(_window: HWND, _message: u32, id: usize, _time: u32) {
    // SAFETY: the thread timer this call is for; it fires once.
    let _ = unsafe { KillTimer(None, id) };
    let Some(drag) = PENDING.with(|p| p.borrow_mut().take()) else {
        return;
    };
    if let Err(e) = drag_files(drag) {
        crate::main_window::drag_failed(e);
    }
    crate::main_window::drag_ended();
}

fn drag_files(drag: Drag) -> Result<()> {
    let work = crate::drag::new_work()?;
    let files: Vec<PathBuf> = drag
        .entries
        .iter()
        .map(|e| crate::drag::extracted(&work, &e.path))
        .collect();
    // SAFETY: on the UI thread, where winit already initialized OLE for its
    // own drop target, in the same apartment; this only counts up, and is
    // matched below.
    unsafe { OleInitialize(None) }.context("starting OLE to drag the entries")?;
    let result = drag_with_ole(drag, work, &files);
    // SAFETY: matches the OleInitialize above, which succeeded.
    unsafe { OleUninitialize() };
    result
}

/// What the drag shares with the drop source's callbacks.
struct State {
    drag: Drag,
    work: PathBuf,
    /// What the window under the pointer said it would do with the files.
    effect: Cell<DROPEFFECT>,
    error: RefCell<Option<anyhow::Error>>,
}

#[implement(IDropSource)]
struct DropSource(Rc<State>);

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        if keys.0 & MK_LBUTTON.0 != 0 {
            return S_OK;
        }
        // Released. Over somewhere that refuses the files, nothing is
        // extracted; otherwise they are, before the target hears of the drop.
        if self.0.effect.get() == DROPEFFECT_NONE {
            return DRAGDROP_S_CANCEL;
        }
        match crate::drag::extract_all(&self.0.drag, &self.0.work) {
            Ok(()) => DRAGDROP_S_DROP,
            Err(e) => {
                *self.0.error.borrow_mut() = Some(e);
                DRAGDROP_S_CANCEL
            }
        }
    }

    fn GiveFeedback(&self, effect: DROPEFFECT) -> HRESULT {
        self.0.effect.set(effect);
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

fn drag_with_ole(drag: Drag, work: PathBuf, files: &[PathBuf]) -> Result<()> {
    // SAFETY: no folder, no items and no inner object: an empty data object
    // of the shell's, which takes any format set on it and answers the ones
    // Explorer sets on it during the drag.
    let data: IDataObject = unsafe { SHCreateDataObject(None, None, None::<&IDataObject>) }
        .context("creating the data to drag")?;
    let names = file_list(files)?;
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let medium = STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: names },
        pUnkForRelease: ManuallyDrop::new(None),
    };
    // SAFETY: the format and the medium describe the same block; with
    // `release` set the data object owns it once this succeeds.
    if let Err(e) = unsafe { data.SetData(&format, &medium, true) } {
        // SAFETY: refused, so the block is still ours to free.
        let _ = unsafe { GlobalFree(Some(names)) };
        return Err(e).context("setting the files to drag");
    }
    let state = Rc::new(State {
        drag,
        work,
        effect: Cell::new(DROPEFFECT_NONE),
        error: RefCell::new(None),
    });
    let source: IDropSource = DropSource(state.clone()).into();
    let mut effect = DROPEFFECT_NONE;
    // Copy only: Explorer would otherwise move the files out of the
    // temporary folder on the same disk, which is fine, but a target that
    // moves by deleting after its copy would race the cleanup.
    // SAFETY: both objects outlive the call, which runs on this thread,
    // where OLE is initialized.
    let hr = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
    if let Some(e) = state.error.borrow_mut().take() {
        return Err(e);
    }
    hr.ok().context("dragging the entries")?;
    Ok(())
}

/// The paths as CF_HDROP has them: a DROPFILES header, then each path in
/// UTF-16 ending in a zero, then one more zero.
fn file_list(files: &[PathBuf]) -> Result<HGLOBAL> {
    let mut wide: Vec<u16> = Vec::new();
    for f in files {
        wide.extend(f.as_os_str().encode_wide());
        wide.push(0);
    }
    wide.push(0);
    let header = std::mem::size_of::<DROPFILES>();
    let size = header + wide.len() * 2;
    // SAFETY: a plain allocation, zeroed by GHND.
    let block = unsafe { GlobalAlloc(GHND, size) }.context("allocating the file list")?;
    // SAFETY: `block` is the movable block just allocated.
    let p = unsafe { GlobalLock(block) }.cast::<u8>();
    if p.is_null() {
        // SAFETY: ours, and given to nobody yet.
        let _ = unsafe { GlobalFree(Some(block)) };
        bail!("locking the file list");
    }
    let head = DROPFILES {
        pFiles: header as u32,
        fWide: true.into(),
        ..Default::default()
    };
    // SAFETY: the block holds `size` bytes, the header and then the names;
    // DROPFILES is packed, so it is written unaligned.
    unsafe {
        std::ptr::write_unaligned(p.cast::<DROPFILES>(), head);
        std::ptr::copy_nonoverlapping(wide.as_ptr().cast::<u8>(), p.add(header), wide.len() * 2);
        let _ = GlobalUnlock(block);
    }
    Ok(block)
}
