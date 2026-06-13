//! Linux/X11 helpers for the outer baseview window's WM frame.
//!
//! Size hints:
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
//!
//! Frame appearance / functions:
//! - [`set_background_black`] gives the outer window an opaque-black
//!   background so uncovered regions read as black, not glitched.
//! - [`disable_maximize`] clears the Motif `MWM_FUNC_MAXIMIZE` bit so
//!   the WM drops the maximize affordance for resizable editors that
//!   don't opt into it. Best-effort: floating WMs (mutter, kwin,
//!   xfwm) honour `_MOTIF_WM_HINTS`; tiling WMs (i3, sway) ignore it.

use std::os::raw::{c_int, c_uchar, c_uint, c_ulong};

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

/// Give the outer baseview window an opaque-black background so the
/// X server auto-fills any region the editor child doesn't cover.
///
/// baseview-truce creates the outer window on a 32-bit ARGB visual
/// with no `background_pixel` set, so its background is `None`: when a
/// resizable editor is maximized past its own max bounds (the WM
/// ignores max size hints in the maximized state) the uncovered margin
/// around the clamped editor child shows uninitialised server memory -
/// the "glitched outer area". Setting a solid background pixel makes
/// the server clear newly exposed parent regions to that pixel on every
/// future resize, with no per-frame work on our side.
///
/// The pixel is `0xFF00_0000`: alpha `0xFF` (opaque) over RGB `0` on a
/// 32-bit ARGB visual, and the top byte falls outside every channel
/// mask on a 24-bit `TrueColor` visual so it still reads as plain black
/// there. Plain `0` would be *transparent* black under a compositor on
/// the 32-bit visual, which is the glitch we're trying to avoid.
///
/// No-op if the display handle is not Xlib or libX11 fails to load.
/// Must be called on the thread that owns the baseview event loop;
/// Xlib calls are not thread-safe on a display the loop also uses.
pub fn set_background_black(display_handle: RawDisplayHandle, window_handle: &XlibWindowHandle) {
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

    // SAFETY: `display_ptr` / `window_id` come from baseview, which
    // owns the X connection and window for the lifetime of the event
    // loop, and we run on that loop's thread.
    unsafe {
        // Persistent window attribute: once set, the server clears
        // every future expose (including the maximize-driven resize)
        // to this pixel, so one call at window creation is enough.
        (lib.XSetWindowBackground)(display_ptr, window_id, 0xFF00_0000);
        // Repaint the current frame so the background takes effect now
        // rather than only on the next expose. Child windows obscure
        // the parent, so this never blacks out the editor surface.
        (lib.XClearWindow)(display_ptr, window_id);
        (lib.XFlush)(display_ptr);
    }
}

/// Remove the maximize affordance from the outer window via Motif WM
/// hints, leaving move / resize / minimize / close intact.
///
/// Sets `_MOTIF_WM_HINTS` with `MWM_HINTS_FUNCTIONS` and a functions
/// mask of everything *except* `MWM_FUNC_MAXIMIZE`, which tells the WM
/// to drop the maximize button and ignore maximize requests
/// (double-click titlebar, the window menu's "Maximize") while still
/// allowing interactive edge-drag resize. For a resizable editor with
/// a bounded `max_size` this is what stops the window jumping past the
/// editor's max and leaving an unpainted margin; the
/// [`set_resize_hints`] cap handles edge-drags, this handles maximize.
///
/// Best-effort. Floating WMs (mutter, kwin, xfwm, openbox) honour
/// `_MOTIF_WM_HINTS`; tiling WMs (i3, sway, dwm) ignore it and size
/// the window to the tile regardless - there's no portable client-side
/// way to forbid that. No-op if the display handle is not Xlib, if
/// libX11 fails to load, or if interning the atom fails.
///
/// Must be called on the thread that owns the baseview event loop;
/// Xlib calls are not thread-safe on a display the loop also uses.
pub fn disable_maximize(display_handle: RawDisplayHandle, window_handle: &XlibWindowHandle) {
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

    // Motif `PropMotifWmHints` field/bit values (from `Xm/MwmUtil.h`,
    // which we don't link against - they're stable wire constants).
    const MWM_HINTS_FUNCTIONS: c_ulong = 1 << 0;
    const MWM_FUNC_RESIZE: c_ulong = 1 << 1;
    const MWM_FUNC_MOVE: c_ulong = 1 << 2;
    const MWM_FUNC_MINIMIZE: c_ulong = 1 << 3;
    // MWM_FUNC_MAXIMIZE is `1 << 4`; deliberately omitted from the mask.
    const MWM_FUNC_CLOSE: c_ulong = 1 << 5;

    // The property is five longs: flags, functions, decorations,
    // input_mode, status. We touch only `functions` (gated by the
    // `MWM_HINTS_FUNCTIONS` flag); `decorations` is left untouched
    // because we don't set `MWM_HINTS_DECORATIONS`, so the full title
    // bar / border stays.
    let hints: [c_ulong; 5] = [
        MWM_HINTS_FUNCTIONS,
        MWM_FUNC_RESIZE | MWM_FUNC_MOVE | MWM_FUNC_MINIMIZE | MWM_FUNC_CLOSE,
        0,
        0,
        0,
    ];

    // SAFETY: `display_ptr` / `window_id` come from baseview, which
    // owns the X connection and window for the lifetime of the event
    // loop, and we run on that loop's thread. The atom name is a
    // NUL-terminated C string literal; `hints` outlives the
    // `XChangeProperty` call that copies it.
    unsafe {
        let atom = (lib.XInternAtom)(display_ptr, c"_MOTIF_WM_HINTS".as_ptr(), xlib::False);
        if atom == 0 {
            return;
        }
        // Property type is the same atom as the name, format 32, five
        // elements. On 64-bit Xlib a format-32 element is a `long`, so
        // `[c_ulong; 5]` is the correct in-memory shape.
        (lib.XChangeProperty)(
            display_ptr,
            window_id,
            atom,
            atom,
            32,
            xlib::PropModeReplace,
            hints.as_ptr().cast::<c_uchar>(),
            5,
        );
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
