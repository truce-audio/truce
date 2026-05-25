//! Windows helper that strips the maximize affordance from the outer
//! baseview window.
//!
//! Plugin editors don't currently expose a resize protocol, so
//! maximizing the parent window just stretches the editor's child
//! surface without re-laying-out content. Clearing `WS_MAXIMIZEBOX`
//! greys out the title-bar maximize button and disables the other
//! routes to the same state (double-clicking the title bar, the
//! `Win`+`Up` snap). The Linux side does the equivalent by pinning
//! WM size hints (`windowed_x11::pin_size`).

use std::ffi::c_void;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetWindowLongW, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SetWindowLongW, SetWindowPos, WS_MAXIMIZEBOX,
};

/// Clear `WS_MAXIMIZEBOX` on `hwnd` so the window can no longer be
/// maximized. No-op if the handle is null or the style is already
/// clear.
///
/// Must run on the thread that owns the baseview window (the main
/// thread) - `SetWindowLongW` / `SetWindowPos` target a window the
/// event loop also services.
// Why: `WS_MAXIMIZEBOX as i32` - the style is a `u32` bit flag
// (0x0001_0000) that fits in `i32` without wrapping; the cast is just
// matching `GetWindowLongW`'s `LONG` return type for the bitwise mask.
#[allow(clippy::cast_possible_wrap)]
pub fn disable_maximize(hwnd: *mut c_void) {
    if hwnd.is_null() {
        return;
    }
    let hwnd = hwnd as HWND;

    // SAFETY: `hwnd` is the live baseview window handle and we're on
    // the event-loop thread. `GetWindowLongW` / `SetWindowLongW`
    // read and write the window's style word; `SetWindowPos` with
    // `SWP_FRAMECHANGED` forces the non-client area to repaint so the
    // greyed-out button shows immediately.
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        let cleared = style & !(WS_MAXIMIZEBOX as i32);
        if cleared == style {
            return;
        }
        SetWindowLongW(hwnd, GWL_STYLE, cleared);
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    }
}
