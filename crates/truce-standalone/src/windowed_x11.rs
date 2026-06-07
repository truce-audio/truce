//! Linux/X11 helpers for the outer baseview window's WM size hints.
//!
//! Two modes:
//! - [`pin_size`] locks min == max to the current geometry for
//!   editors that don't support resize, so the WM hides resize grips
//!   and rejects resize requests entirely.
//! - [`set_resize_hints`] sets a min/max *range* plus a resize
//!   *increment* for resizable editors so the WM clamps interactive
//!   edge-drags to the editor's bounds and snaps them to whole cells
//!   (the same `PResizeInc` mechanism terminal emulators use to snap
//!   to character cells). Letting the WM enforce both is the only
//!   re-entrancy-safe way to do it: pushing `configure_window` back
//!   from inside the window's own `ConfigureNotify` handler fights
//!   the WM's resize grab and the window runs away.

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

/// Set the outer window's WM min/max size range and resize
/// increment, all in **physical pixels**, so the window manager
/// clamps interactive edge-drags to the editor's bounds and snaps
/// them to whole cells.
///
/// - A `max_*` of `0` means "unbounded on that axis" and omits the
///   per-axis cap (a large sentinel) while keeping `PMaxSize` set.
/// - An `inc_*` of `0` means "no snap on that axis"; when both are
///   non-zero the snap counts from `min_*` (already cell-aligned)
///   via `PBaseSize` + `PResizeInc`, so every allowed size lands on
///   a cell boundary.
///
/// No-op if the display handle is not Xlib or libX11 fails to load.
/// Must be called on the thread that owns the baseview event loop;
/// Xlib calls are not thread-safe on a display the loop also uses.
// Eight args is a flat list of WM size-hint fields (min/max/inc per
// axis); bundling them into a struct just to satisfy the lint would
// add ceremony without clarity.
#[allow(clippy::too_many_arguments)]
pub fn set_resize_hints(
    display_handle: RawDisplayHandle,
    window_handle: &XlibWindowHandle,
    min_w: u32,
    min_h: u32,
    max_w: u32,
    max_h: u32,
    inc_w: u32,
    inc_h: u32,
) {
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

    // Physical pixel sizes stay well within `c_int` for any real
    // display; clamp on overflow rather than UB on the cast.
    let clamp = |v: u32| c_int::try_from(v).unwrap_or(c_int::MAX);
    let min_width = clamp(min_w.max(1));
    let min_height = clamp(min_h.max(1));

    // SAFETY: `display_ptr` / `window_id` come from baseview, which
    // owns the X connection and window for the lifetime of the event
    // loop, and we run on that loop's thread.
    unsafe {
        let hints = (lib.XAllocSizeHints)();
        if hints.is_null() {
            return;
        }
        let mut flags = xlib::PMinSize | xlib::PMaxSize;
        (*hints).min_width = min_width;
        (*hints).min_height = min_height;
        // An unbounded axis pins its cap to a large sentinel so the
        // WM never reads an uninitialised `max_*` field.
        (*hints).max_width = if max_w > 0 { clamp(max_w) } else { c_int::MAX };
        (*hints).max_height = if max_h > 0 { clamp(max_h) } else { c_int::MAX };
        // Snap edge-drags to whole cells. The base is `min_*` (which
        // is itself cell-aligned), so allowed sizes are
        // `min + i*inc` - always on a boundary.
        if inc_w > 0 && inc_h > 0 {
            flags |= xlib::PResizeInc | xlib::PBaseSize;
            (*hints).width_inc = clamp(inc_w);
            (*hints).height_inc = clamp(inc_h);
            (*hints).base_width = min_width;
            (*hints).base_height = min_height;
        }
        (*hints).flags = flags;
        (lib.XSetWMNormalHints)(display_ptr, window_id, hints);
        (lib.XFree)(hints.cast());
        (lib.XFlush)(display_ptr);
    }
}
