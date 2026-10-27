//! An in-process COM server with one class, an `IExplorerCommand` for the
//! "Extract to" entry. The registry names the class on that entry's verb
//! key (`ExplorerCommandHandler`), so Explorer creates it through
//! `DllGetClassObject`, asks `GetTitle` for the text each time the menu
//! opens, and calls `Invoke` when it is chosen.
//!
//! All of this runs inside explorer.exe. A panic that unwound out of an
//! `extern "system"` function would abort it, so every call Explorer makes
//! goes through `guard`.

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use hatchzip_shell::explorer::{HANDLER_CLSID, extract_to_title};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_NOTIMPL, E_POINTER,
    HMODULE, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx, IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows::Win32::UI::Shell::{
    ECF_DEFAULT, ECS_ENABLED, ECS_HIDDEN, IEnumExplorerCommand, IExplorerCommand,
    IExplorerCommand_Impl, IShellItemArray, SHStrDupW, SIGDN_FILESYSPATH,
};
use windows::core::{BOOL, Error, GUID, HRESULT, IUnknown, Interface, PCWSTR, PWSTR, Ref, Result};
use windows_core::implement;

const CLSID: GUID = GUID::from_u128(HANDLER_CLSID);

/// The program the entry runs, next to this DLL, as the installers and the
/// portable zip put it.
const GUI: &str = "hatchzip-gui.exe";

/// Objects alive plus `LockServer` locks. Explorer frees the DLL only once
/// `DllCanUnloadNow` finds this at zero.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// Counts its owner in [`LIVE`] for as long as it exists.
struct Live;

impl Live {
    fn new() -> Self {
        LIVE.fetch_add(1, Ordering::SeqCst);
        Live
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Runs `f`, turning a panic into an error Explorer can cope with.
fn guard<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(E_FAIL.into()))
}

#[implement(IExplorerCommand)]
struct ExtractTo {
    _live: Live,
}

impl IExplorerCommand_Impl for ExtractTo_Impl {
    fn GetTitle(&self, items: Ref<IShellItemArray>) -> Result<PWSTR> {
        guard(|| {
            let (count, first) = match items.as_ref() {
                Some(items) => {
                    let count = unsafe { items.GetCount() }.unwrap_or(0) as usize;
                    // Only one name is ever shown, so a large selection
                    // reads none.
                    let path = if count == 1 {
                        path_at(items, 0).ok()
                    } else {
                        None
                    };
                    let first = path
                        .as_deref()
                        .and_then(Path::file_name)
                        .map(|n| n.to_string_lossy().into_owned());
                    (count, first)
                }
                None => (0, None),
            };
            co_task_string(&extract_to_title(count, first.as_deref()))
        })
    }

    fn GetIcon(&self, _items: Ref<IShellItemArray>) -> Result<PWSTR> {
        // The submenu's other entries have no icon either.
        Err(E_NOTIMPL.into())
    }

    fn GetToolTip(&self, _items: Ref<IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<GUID> {
        Err(E_NOTIMPL.into())
    }

    fn GetState(&self, items: Ref<IShellItemArray>, _ok_to_be_slow: BOOL) -> Result<u32> {
        guard(|| {
            // The GUI opens files on disk only. An archive inside a zip
            // folder, or on a phone, has no path to give it. One item is
            // enough to tell, and keeps a large selection quick.
            let on_disk = items.as_ref().is_none_or(|items| path_at(items, 0).is_ok());
            let state = if on_disk { ECS_ENABLED } else { ECS_HIDDEN };
            Ok(state.0 as u32)
        })
    }

    fn Invoke(&self, items: Ref<IShellItemArray>, _bind: Ref<IBindCtx>) -> Result<()> {
        guard(|| {
            let items = items.ok()?;
            let count = unsafe { items.GetCount()? };
            let paths = (0..count)
                .map(|i| path_at(items, i))
                .collect::<Result<Vec<_>>>()?;
            if paths.is_empty() {
                return Err(E_INVALIDARG.into());
            }
            // One process for the whole selection, which the GUI extracts
            // one archive after another. It is never waited for: Explorer
            // must not block on it, and dropping the `Child` only closes
            // its handles on Windows.
            #[allow(clippy::zombie_processes)]
            let gui = Command::new(gui_path()?)
                .arg("--extract-to")
                .args(&paths)
                .spawn()?;
            drop(gui);
            Ok(())
        })
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_DEFAULT.0 as u32)
    }

    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

/// The file system path of item `i`, or an error for an item that has none.
fn path_at(items: &IShellItemArray, i: u32) -> Result<PathBuf> {
    let item = unsafe { items.GetItemAt(i)? };
    let name = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH)? };
    let path = unsafe { OsString::from_wide(name.as_wide()) };
    unsafe { CoTaskMemFree(Some(name.as_ptr() as *const c_void)) };
    Ok(PathBuf::from(path))
}

/// A copy of `s` that Explorer frees with `CoTaskMemFree`, as the interface
/// requires.
fn co_task_string(s: &str) -> Result<PWSTR> {
    let wide: Vec<u16> = s.encode_utf16().chain([0]).collect();
    unsafe { SHStrDupW(PCWSTR(wide.as_ptr())) }
}

/// `hatchzip-gui.exe` in the folder this DLL was loaded from. The registry
/// holds the DLL's path, so this follows wherever it was installed.
fn gui_path() -> Result<PathBuf> {
    let dll = own_path()?;
    let dir = dll.parent().unwrap_or(Path::new(""));
    Ok(dir.join(GUI))
}

fn own_path() -> Result<PathBuf> {
    let mut module = HMODULE::default();
    // Any address inside this DLL finds it, and `LIVE` is one. No reference
    // is taken, so nothing has to be released.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(std::ptr::addr_of!(LIVE).cast()),
            &mut module,
        )?;
    }
    // MAX_PATH first, then larger, up to the longest path Windows allows.
    let mut buf = vec![0u16; 260];
    loop {
        let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
        if n == 0 {
            return Err(Error::from_thread());
        }
        if n < buf.len() {
            buf.truncate(n);
            return Ok(PathBuf::from(OsString::from_wide(&buf)));
        }
        if buf.len() >= 32_768 {
            return Err(E_FAIL.into());
        }
        buf.resize(buf.len() * 2, 0);
    }
}

#[implement(IClassFactory)]
struct Factory {
    _live: Live,
}

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<IUnknown>,
        iid: *const GUID,
        object: *mut *mut c_void,
    ) -> Result<()> {
        guard(|| {
            if iid.is_null() || object.is_null() {
                return Err(E_POINTER.into());
            }
            unsafe { *object = std::ptr::null_mut() };
            if !outer.is_null() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            let command: IExplorerCommand = ExtractTo { _live: Live::new() }.into();
            unsafe { command.query(iid, object) }.ok()
        })
    }

    fn LockServer(&self, lock: BOOL) -> Result<()> {
        if lock.as_bool() {
            LIVE.fetch_add(1, Ordering::SeqCst);
        } else {
            // An unlock without a lock would wrap the count and keep the DLL
            // loaded for good.
            let _ = LIVE.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        }
        Ok(())
    }
}

/// Hands COM the class factory for [`CLSID`], the only class here.
///
/// # Safety
///
/// COM calls this with valid pointers: two GUIDs to read and a slot to
/// write the factory to.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if clsid.is_null() || iid.is_null() || out.is_null() {
        return E_POINTER;
    }
    unsafe { *out = std::ptr::null_mut() };
    if unsafe { *clsid } != CLSID {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let factory: IClassFactory = Factory { _live: Live::new() }.into();
        unsafe { factory.query(iid, out) }
    }))
    .unwrap_or(E_FAIL)
}

/// Lets COM free the DLL once nothing from it is alive.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if LIVE.load(Ordering::SeqCst) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}
