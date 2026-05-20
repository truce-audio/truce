//! Linux/X11 helper that locks the outer baseview window to a fixed
//! size so the window manager hides resize grips and rejects resize
//! requests.
//!
//! Plugin editors don't currently expose a resize protocol, so a
//! resize on the parent baseview window just stretches the editor's
//! child surface without re-laying-out content. Until the editor side
//! grows resize support, pin the WM size hints so the user can't get
//! into that state.

use std::os::raw::{c_int, c_uint};

use raw_window_handle::{RawDisplayHandle, XlibWindowHandle};
use x11_dl::xlib;

/// Pin WM min/max size to the window's current geometry. No-op if the
/// display handle is not Xlib, if libX11 fails to load, or if the
/// server doesn't return a valid geometry.
///
/// Must be called on the thread that owns the baseview event loop
/// (the main thread); Xlib calls are not thread-safe on a display the
/// loop also uses.
pub fn pin_size(display_handle: RawDisplayHandle, window_handle: &XlibWindowHandle) {
    let RawDisplayHandle::Xlib(display) = display_handle else {
        return;
    };
    let display_ptr = display.display.cast::<xlib::Display>();
    if display_ptr.is_null() {
        return;
    }
    let Ok(lib) = xlib::Xlib::open() else {
        return;
    };
    let window_id = xlib::Window::from(window_handle.window);

    // SAFETY: `display_ptr` and `window_id` come from baseview, which
    // owns the X connection and window for the lifetime of this
    // closure. All xlib calls below take pointers / IDs owned by the
    // baseview event loop, and we're running on its thread.
    unsafe {
        let mut root: xlib::Window = 0;
        let mut x: c_int = 0;
        let mut y: c_int = 0;
        let mut width: c_uint = 0;
        let mut height: c_uint = 0;
        let mut border: c_uint = 0;
        let mut depth: c_uint = 0;
        let ok = (lib.XGetGeometry)(
            display_ptr,
            window_id,
            &raw mut root,
            &raw mut x,
            &raw mut y,
            &raw mut width,
            &raw mut height,
            &raw mut border,
            &raw mut depth,
        );
        if ok == 0 || width == 0 || height == 0 {
            return;
        }

        // Window geometry from the X server fits in `c_int` for every
        // real display, but the protocol field is `u32`. Clamp on the
        // off chance a misbehaving server returns something silly so
        // we don't UB on the cast.
        let w = c_int::try_from(width).unwrap_or(c_int::MAX);
        let h = c_int::try_from(height).unwrap_or(c_int::MAX);

        let hints = (lib.XAllocSizeHints)();
        if hints.is_null() {
            return;
        }
        (*hints).flags = xlib::PMinSize | xlib::PMaxSize;
        (*hints).min_width = w;
        (*hints).max_width = w;
        (*hints).min_height = h;
        (*hints).max_height = h;
        (lib.XSetWMNormalHints)(display_ptr, window_id, hints);
        (lib.XFree)(hints.cast());
        (lib.XFlush)(display_ptr);
    }
}
