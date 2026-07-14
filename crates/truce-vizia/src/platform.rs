//! `truce_core::RawWindowHandle` -> `raw_window_handle 0.6`
//! `HasWindowHandle` bridge so `vizia::Application::open_parented`
//! can mount onto whatever child window the DAW handed us.
//!
//! Same shape as `truce_gui::platform::ParentWindow`; reproduced here
//! so `truce-vizia` doesn't have to pull the whole `truce-gui` /
//! `truce-cpu` dep tree just for a 40-line newtype.

use std::num::NonZeroIsize;
use std::ptr::NonNull;

use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle as RwhRawWindowHandle, WindowHandle};
use truce_core::editor::RawWindowHandle;

pub struct ParentWindow(pub RawWindowHandle);

impl HasWindowHandle for ParentWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = match self.0 {
            RawWindowHandle::AppKit(ptr) => {
                let ns_view = NonNull::new(ptr).ok_or(HandleError::Unavailable)?;
                RwhRawWindowHandle::AppKit(raw_window_handle::AppKitWindowHandle::new(ns_view))
            }
            RawWindowHandle::UiKit(_) => {
                // truce-vizia is `cfg(not(ios))`-gated at the lib
                // root; this arm is unreachable in practice but
                // surfaced so the match stays exhaustive.
                unreachable!("vizia backend is desktop-only")
            }
            RawWindowHandle::Win32(ptr) => {
                let hwnd = NonZeroIsize::new(ptr as isize).ok_or(HandleError::Unavailable)?;
                RwhRawWindowHandle::Win32(raw_window_handle::Win32WindowHandle::new(hwnd))
            }
            RawWindowHandle::X11(window_id) => {
                RwhRawWindowHandle::Xlib(raw_window_handle::XlibWindowHandle::new(window_id as _))
            }
        };
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}
