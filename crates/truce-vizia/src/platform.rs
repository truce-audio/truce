//! `truce_core::RawWindowHandle` -> `raw_window_handle 0.5`
//! `HasRawWindowHandle` bridge so `vizia::Application::open_parented`
//! can mount onto whatever child window the DAW handed us.
//!
//! Same shape as `truce_gui::platform::ParentWindow`; reproduced here
//! so `truce-vizia` doesn't have to pull the whole `truce-gui` /
//! `truce-cpu` dep tree just for a 40-line newtype.

use raw_window_handle::{HasRawWindowHandle, RawWindowHandle as RwhRawWindowHandle};
use truce_core::editor::RawWindowHandle;

pub struct ParentWindow(pub RawWindowHandle);

unsafe impl HasRawWindowHandle for ParentWindow {
    fn raw_window_handle(&self) -> RwhRawWindowHandle {
        match self.0 {
            RawWindowHandle::AppKit(ptr) => {
                let mut handle = raw_window_handle::AppKitWindowHandle::empty();
                handle.ns_view = ptr;
                RwhRawWindowHandle::AppKit(handle)
            }
            RawWindowHandle::UiKit(_) => {
                // truce-vizia is `cfg(not(ios))`-gated at the lib
                // root; this arm is unreachable in practice but
                // surfaced so the match stays exhaustive.
                unreachable!("vizia backend is desktop-only")
            }
            RawWindowHandle::Win32(ptr) => {
                let mut handle = raw_window_handle::Win32WindowHandle::empty();
                handle.hwnd = ptr;
                RwhRawWindowHandle::Win32(handle)
            }
            RawWindowHandle::X11(window_id) => {
                let mut handle = raw_window_handle::XlibWindowHandle::empty();
                // rwh 0.5 `window` field is `c_ulong` (u64 on
                // Linux/macOS, u32 on Windows). The Windows
                // narrowing path is dead since X11 doesn't run
                // there, but the cast keeps the assignment portable.
                #[allow(clippy::cast_possible_truncation)]
                {
                    handle.window = window_id as _;
                }
                RwhRawWindowHandle::Xlib(handle)
            }
        }
    }
}
