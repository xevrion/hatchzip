//! Choosing how to draw, and files dropped on the window.
//!
//! Linux and macOS start with FemtoVG (OpenGL). Windows starts with the
//! software renderer: OpenGL setup can succeed there even when the driver
//! fails to present, leaving a black window and bypassing the fallback.
//!
//! On Linux, a broken driver fails while creating the window, and Slint 1.18
//! keeps running with no window. Watching the first frame catches this and
//! restarts the app with the software renderer.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::winit::event_loop::ActiveEventLoop;
use slint::winit_030::winit::window::{Icon, Window as WinitWindow, WindowId};
use slint::winit_030::{CustomApplicationHandler, EventResult};
use slint::{RenderingState, Timer};

/// Generous for a slow machine drawing OpenGL on the CPU; a working driver
/// shows the first frame in well under a second.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(4);

/// Slint reads this itself; we only set it for the restart.
const BACKEND_VAR: &str = "SLINT_BACKEND";

type Slot<T> = RefCell<Option<Box<dyn Fn(T)>>>;

/// Where files dropped on a window go. Filled in once the window exists,
/// since the handler has to be installed before any window is created.
#[derive(Default)]
pub struct Drops {
    pub on_drop: Slot<PathBuf>,
    pub on_hover: Slot<bool>,
}

struct Handler(Rc<Drops>);

impl CustomApplicationHandler for Handler {
    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _winit_window: Option<&WinitWindow>,
        _slint_window: Option<&slint::Window>,
        event: &WindowEvent,
    ) -> EventResult {
        // winit 0.30 reports dropped files on Windows, macOS and X11 only;
        // on Wayland nothing arrives here and the Open button is the way in.
        match event {
            WindowEvent::DroppedFile(path) => {
                if let Some(f) = &*self.0.on_hover.borrow() {
                    f(false);
                }
                if let Some(f) = &*self.0.on_drop.borrow() {
                    f(path.clone());
                }
            }
            WindowEvent::HoveredFile(_) => {
                if let Some(f) = &*self.0.on_hover.borrow() {
                    f(true);
                }
            }
            WindowEvent::HoveredFileCancelled => {
                if let Some(f) = &*self.0.on_hover.borrow() {
                    f(false);
                }
            }
            _ => {}
        }
        EventResult::Propagate
    }
}

pub fn select(drops: Rc<Drops>) -> Result<()> {
    let icon = window_icon();
    let opened = drops.clone();
    let mut selector = slint::BackendSelector::new().backend_name("winit".into());
    if let Some(renderer) = crate::platform::DEFAULT_RENDERER
        && std::env::var_os(BACKEND_VAR).is_none()
    {
        selector = selector.renderer_name(renderer.into());
    }
    selector
        .with_winit_custom_application_handler(Handler(drops))
        // With the `.slint` files' `icon:` set to an embedded PNG, Slint
        // 1.18 left X11 with an empty _NET_WM_ICON (checked with xprop), so
        // the windows set no icon there and winit gets it here, for every
        // window. Wayland and macOS take the icon from the desktop file or
        // the app bundle instead.
        .with_winit_window_attributes_hook(move |attrs| attrs.with_window_icon(icon.clone()))
        .select()
        .context("starting the window system")?;
    // An archive the system opens with the running app arrives the way a
    // dropped one does.
    crate::platform::forward_opened_files(move |path| {
        if let Some(f) = &*opened.on_drop.borrow() {
            f(path);
        }
    });
    Ok(())
}

fn window_icon() -> Option<Icon> {
    let png = include_bytes!("../../../assets/icon/png/hatchzip-64.png");
    let mut reader = png::Decoder::new(std::io::Cursor::new(&png[..]))
        .read_info()
        .ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        _ => return None,
    };
    Icon::from_rgba(rgba, info.width, info.height).ok()
}

/// Runs `start` once it is safe to touch the archive: after the first frame
/// with OpenGL, or right away with the software renderer. A restart into
/// software drawing must never leave a half-started extraction behind in the
/// process that gives up.
pub fn when_drawn(window: &slint::Window, start: impl FnOnce() + 'static) {
    let start = Rc::new(RefCell::new(Some(Box::new(start) as Box<dyn FnOnce()>)));
    let drawn = Rc::new(Cell::new(false));
    let (s, d) = (start.clone(), drawn.clone());
    let notifier = window.set_rendering_notifier(move |state, _| {
        if matches!(state, RenderingState::RenderingSetup) && !d.replace(true) {
            // Not from inside the renderer: starting work changes properties.
            let s = s.clone();
            Timer::single_shot(Duration::ZERO, move || {
                if let Some(f) = s.take() {
                    f();
                }
            });
        }
    });
    match notifier {
        // Only OpenGL renderers take a notifier; this one can fail late.
        Ok(()) => {
            if std::env::var_os(BACKEND_VAR).is_none() {
                Timer::single_shot(FIRST_FRAME_TIMEOUT, move || {
                    if !drawn.get() {
                        restart_in_software();
                    }
                });
            }
        }
        Err(_) => {
            if let Some(f) = start.take() {
                f();
            }
        }
    }
}

fn restart_in_software() -> ! {
    eprintln!("hatchzip-gui: OpenGL is not available; using the software renderer");
    let spawned = std::env::current_exe().and_then(|exe| {
        Command::new(exe)
            .args(std::env::args_os().skip(1))
            .env(BACKEND_VAR, "winit-software")
            .spawn()
    });
    match spawned {
        Ok(_) => std::process::exit(0),
        Err(e) => {
            eprintln!("hatchzip-gui: unable to restart: {e}");
            std::process::exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_window_icon_decodes() {
        assert!(super::window_icon().is_some());
    }
}
