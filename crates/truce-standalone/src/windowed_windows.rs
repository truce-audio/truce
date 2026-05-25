//! Windows helpers that tidy up the outer baseview window: lock it to
//! a fixed-size, close-only frame and give it the app's title-bar /
//! taskbar icon.
//!
//! Plugin editors don't currently expose a resize protocol, so
//! maximizing or dragging the parent window just stretches the
//! editor's child surface without re-laying-out content. Clearing
//! `WS_SIZEBOX` makes the window non-resizable; clearing both
//! `WS_MAXIMIZEBOX` and `WS_MINIMIZEBOX` hides the maximize and
//! minimize buttons entirely (Win32 renders both glyphs - one
//! greyed - whenever *either* box style is set, so the maximize
//! button can only be hidden by dropping minimize too). What's left
//! is the close button from baseview's `WS_SYSMENU`. The Linux side
//! does the size-locking equivalent by pinning WM size hints
//! (`windowed_x11::pin_size`).
//!
//! baseview registers its window class without an icon, so the title
//! bar defaults to the generic application glyph. [`set_window_icon`]
//! loads the icon `cargo truce package` embedded in the standalone
//! `.exe` and attaches it to the window.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetSystemMetrics, GetWindowLongW, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTSIZE,
    LR_SHARED, LoadImageW, SM_CXSMICON, SM_CYSMICON, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SendMessageW, SetWindowLongW, SetWindowPos, WM_SETICON, WS_MAXIMIZEBOX,
    WS_MINIMIZEBOX, WS_SIZEBOX,
};

/// Resource name of the embedded app-icon group. `cargo truce`'s
/// `embed_icon` writes the `RT_GROUP_ICON` under numeric name `1`.
const APP_ICON_ID: u16 = 1;

/// Strip the sizing border and the maximize / minimize buttons from
/// `hwnd`, leaving a fixed-size window with only a close button.
/// No-op if the handle is null or the styles are already clear.
///
/// Must run on the thread that owns the baseview window (the main
/// thread) - `SetWindowLongW` / `SetWindowPos` target a window the
/// event loop also services.
// Why: `... as i32` on the style flags - they're `u32` bit flags
// (the OR is 0x0007_0000) that fit in `i32` without wrapping; the cast
// is just matching `GetWindowLongW`'s `LONG` return type for the mask.
#[allow(clippy::cast_possible_wrap)]
pub fn lock_window(hwnd: *mut c_void) {
    if hwnd.is_null() {
        return;
    }
    let hwnd = hwnd as HWND;

    // SAFETY: `hwnd` is the live baseview window handle and we're on
    // the event-loop thread. `GetWindowLongW` / `SetWindowLongW`
    // read and write the window's style word; `SetWindowPos` with
    // `SWP_FRAMECHANGED` forces the non-client area to repaint so the
    // dropped buttons / border show immediately.
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        let cleared = style & !((WS_MAXIMIZEBOX | WS_MINIMIZEBOX | WS_SIZEBOX) as i32);
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

/// Give `hwnd` the app icon embedded in the running `.exe`.
///
/// `cargo truce package` embeds the plugin's `[[plugin]].windows_icon`
/// as the executable's `RT_GROUP_ICON` (name `APP_ICON_ID`); this
/// loads that group and hands the large (taskbar / Alt-Tab) and small
/// (title-bar) variants to the window via `WM_SETICON`. No-op when the
/// handle is null or the resource isn't present - dev `cargo run`
/// builds don't embed it, and there the title bar keeps the default
/// glyph.
///
/// Must run on the window's thread (the main thread).
pub fn set_window_icon(hwnd: *mut c_void) {
    if hwnd.is_null() {
        return;
    }
    let hwnd = hwnd as HWND;

    // SAFETY: all handles come from / target the live window on its
    // own thread. `GetModuleHandleW(null)` returns the running .exe's
    // base; `LoadImageW` reads its embedded icon resources (returns
    // null if absent, which we skip); `SendMessageW(WM_SETICON)` hands
    // the loaded `HICON` to the window. `LR_SHARED` lets the system own
    // each icon's lifetime, so we never destroy them.
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());

        // Large icon: taskbar / Alt-Tab. `LR_DEFAULTSIZE` picks the
        // system large-icon metric (`SM_CXICON`).
        let big = LoadImageW(
            hinst,
            make_int_resource(APP_ICON_ID),
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE | LR_SHARED,
        );
        if !big.is_null() {
            SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, big as isize);
        }

        // Small icon: the title-bar glyph, sized to `SM_CXSMICON`.
        let small = LoadImageW(
            hinst,
            make_int_resource(APP_ICON_ID),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_SHARED,
        );
        if !small.is_null() {
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, small as isize);
        }
    }
}

/// `MAKEINTRESOURCEW`: Win32 reads pointer-sized integers below
/// 0x10000 as numeric resource IDs rather than wide-string pointers.
/// `without_provenance` is the strict-provenance-clean "address but no
/// allocation" form - the same shape `cargo truce`'s `embed_icon` uses
/// to write the resource we're loading here.
fn make_int_resource(id: u16) -> *const u16 {
    std::ptr::without_provenance(id as usize)
}
