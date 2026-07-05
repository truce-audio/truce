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

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GWL_STYLE, GetMenu, GetSystemMetrics, GetWindowLongW, ICON_BIG, ICON_SMALL,
    IMAGE_ICON, LR_DEFAULTSIZE, LR_SHARED, LoadImageW, MINMAXINFO, SM_CXSMICON, SM_CYMENU,
    SM_CYSMICON, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOSIZE as SWP_NOSIZE_FLAG,
    SWP_NOZORDER, SendMessageW, SetWindowLongW, SetWindowPos, WINDOWPOS, WM_GETMINMAXINFO,
    WM_NCDESTROY, WM_SETICON, WM_SIZING, WM_WINDOWPOSCHANGING, WMSZ_BOTTOM, WMSZ_LEFT, WMSZ_RIGHT,
    WMSZ_TOP, WMSZ_TOPLEFT, WMSZ_TOPRIGHT, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SIZEBOX,
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

/// Disable maximize on `hwnd` while keeping it resizable: clear
/// `WS_MAXIMIZEBOX` but leave `WS_SIZEBOX` (the resize border) and
/// `WS_MINIMIZEBOX` in place. For a resizable editor that opts out of
/// maximize, this stops the window jumping past the editor's
/// `max_size` (via the maximize button or a title-bar double-click)
/// and leaving an unpainted margin around the clamped child surface.
///
/// Because Win32 renders the maximize glyph greyed (not hidden)
/// whenever `WS_MINIMIZEBOX` is still set - the same quirk
/// [`lock_window`] documents - the button stays visible but disabled,
/// which is the intended "maximize off, minimize on" frame for a
/// resizable window. Linux equivalent: `windowed_x11::disable_maximize`
/// (Motif `MWM_FUNC_MAXIMIZE`); macOS: `windowed_macos::disable_zoom`.
/// No-op if the handle is null or `WS_MAXIMIZEBOX` is already clear.
///
/// Must run on the thread that owns the baseview window (the main
/// thread).
// `WS_MAXIMIZEBOX as i32`: a single `u32` bit flag (0x0001_0000) that
// fits in `i32` without wrapping; the cast matches `GetWindowLongW`'s
// `LONG` return type, same as `lock_window`.
#[allow(clippy::cast_possible_wrap)]
pub fn disable_maximize(hwnd: *mut c_void) {
    if hwnd.is_null() {
        return;
    }
    let hwnd = hwnd as HWND;

    // SAFETY: `hwnd` is the live baseview window handle and we're on
    // the event-loop thread. Same style-word read/modify/write +
    // `SWP_FRAMECHANGED` repaint as `lock_window`.
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

/// Editor size limits installed on the outer window by
/// [`install_size_limits`]. Logical points; converted with the
/// window's live DPI on every message so monitor moves stay correct.
struct SizeLimits {
    min_w: u32,
    min_h: u32,
    max_w: u32,
    max_h: u32,
    aspect: Option<(u32, u32)>,
}

/// Subclass id for the size-limits subclass (arbitrary tag, unique
/// within this window).
const SIZE_LIMITS_SUBCLASS: usize = 0x7472_6373; // "trcs"

/// Clamp a physical window extent to `[min, max]`, flooring `max` at
/// `min` first. An editor with inconsistent bounds can report a max
/// below its min; `i32::clamp` panics when its lo > hi, and a panic
/// unwinding out of the `extern "system"` subclass proc aborts the
/// host - so pin a bad editor to its min instead (matches the macOS
/// window subclass).
fn clamp_extent(v: i32, min: i32, max: i32) -> i32 {
    v.clamp(min, max.max(min))
}

/// Enforce the editor's `min_size` / `max_size` (and `aspect_ratio`,
/// when set) on the outer standalone window itself.
///
/// Only backends whose `Editor::set_size` clamps (the built-in grid)
/// kept the outer window in bounds before; egui / iced / Slint accept
/// any size verbatim and letterbox the content, which left the OS
/// window free to shrink below `min_size` (clipping the UI) or grow
/// past `max_size` (a sea of letterbox). A `WM_GETMINMAXINFO`
/// subclass clamps both interactive drags and programmatic
/// `SetWindowPos` calls (`DefWindowProc` consults it from
/// `WM_WINDOWPOSCHANGING`); `WM_SIZING` reshapes interactive drags to
/// the locked aspect ratio.
///
/// Sizes are logical editor points; the non-client frame (border,
/// title bar, menu) is added per-message at the window's current DPI.
/// `u32::MAX` on a max axis means unbounded. Must run on the window's
/// thread. No-op on a null handle.
pub fn install_size_limits(
    hwnd: *mut c_void,
    min: (u32, u32),
    max: (u32, u32),
    aspect: Option<(u32, u32)>,
) {
    if hwnd.is_null() {
        return;
    }
    let limits = Box::new(SizeLimits {
        min_w: min.0,
        min_h: min.1,
        max_w: max.0,
        max_h: max.1,
        aspect,
    });
    // SAFETY: `hwnd` is the live baseview window on its own thread.
    // The box rides along as the subclass reference data and is
    // reclaimed in the `WM_NCDESTROY` arm of `size_limits_proc`.
    unsafe {
        SetWindowSubclass(
            hwnd as HWND,
            Some(size_limits_proc),
            SIZE_LIMITS_SUBCLASS,
            Box::into_raw(limits) as usize,
        );
    }
}

/// Physical pixels the window's non-client frame (border + caption +
/// menu bar) adds around the client area at `dpi`.
fn chrome_extents(hwnd: HWND, dpi: u32) -> (i32, i32) {
    // SAFETY: read-only queries against the live window plus a pure
    // rect computation.
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE).cast_unsigned();
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE).cast_unsigned();
        let has_menu = i32::from(!GetMenu(hwnd).is_null());
        let mut r = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        AdjustWindowRectExForDpi(&raw mut r, style, has_menu, ex_style, dpi);
        (r.right - r.left, r.bottom - r.top)
    }
}

/// Logical editor points → physical pixels at `dpi`.
// Editor sizes stay far below i32::MAX at any real DPI.
#[allow(clippy::cast_possible_truncation)]
fn to_phys(logical: u32, dpi: u32) -> i32 {
    (f64::from(logical) * f64::from(dpi) / 96.0).round() as i32
}

/// The size-limits subclass procedure - see [`install_size_limits`].
// Win32 callback: the pointer casts on `lparam` follow the message
// contracts (MINMAXINFO* for WM_GETMINMAXINFO, RECT* for WM_SIZING).
unsafe extern "system" fn size_limits_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    refdata: usize,
) -> LRESULT {
    unsafe {
        match msg {
            WM_GETMINMAXINFO => {
                let limits = &*(refdata as *const SizeLimits);
                let dpi = match GetDpiForWindow(hwnd) {
                    0 => 96,
                    d => d,
                };
                let (chrome_w, chrome_h) = chrome_extents(hwnd, dpi);
                let mmi = &mut *(lparam as *mut MINMAXINFO);
                mmi.ptMinTrackSize.x = to_phys(limits.min_w, dpi) + chrome_w;
                mmi.ptMinTrackSize.y = to_phys(limits.min_h, dpi) + chrome_h;
                if limits.max_w != u32::MAX {
                    mmi.ptMaxTrackSize.x = to_phys(limits.max_w, dpi) + chrome_w;
                }
                if limits.max_h != u32::MAX {
                    mmi.ptMaxTrackSize.y = to_phys(limits.max_h, dpi) + chrome_h;
                }
                0
            }
            WM_WINDOWPOSCHANGING => {
                // Belt-and-braces clamp for programmatic resizes.
                // `DefWindowProc` only honors WM_GETMINMAXINFO track
                // sizes for windows with a sizing frame, so a
                // `lock_window`-pinned (fixed-size) window would
                // still follow any `SetWindowPos` verbatim.
                let limits = &*(refdata as *const SizeLimits);
                let wp = &mut *(lparam as *mut WINDOWPOS);
                if wp.flags & SWP_NOSIZE_FLAG == 0 {
                    let dpi = match GetDpiForWindow(hwnd) {
                        0 => 96,
                        d => d,
                    };
                    let (chrome_w, chrome_h) = chrome_extents(hwnd, dpi);
                    let max_px = |v: u32, chrome: i32| {
                        if v == u32::MAX {
                            i32::MAX
                        } else {
                            to_phys(v, dpi) + chrome
                        }
                    };
                    wp.cx = clamp_extent(
                        wp.cx,
                        to_phys(limits.min_w, dpi) + chrome_w,
                        max_px(limits.max_w, chrome_w),
                    );
                    wp.cy = clamp_extent(
                        wp.cy,
                        to_phys(limits.min_h, dpi) + chrome_h,
                        max_px(limits.max_h, chrome_h),
                    );
                }
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            WM_SIZING => {
                let limits = &*(refdata as *const SizeLimits);
                let Some((aw, ah)) = limits.aspect else {
                    return DefSubclassProc(hwnd, msg, wparam, lparam);
                };
                let dpi = match GetDpiForWindow(hwnd) {
                    0 => 96,
                    d => d,
                };
                let (chrome_w, chrome_h) = chrome_extents(hwnd, dpi);
                let r = &mut *(lparam as *mut RECT);
                let client_w = (r.right - r.left) - chrome_w;
                let client_h = (r.bottom - r.top) - chrome_h;
                // Derive the axis the user isn't dragging from the one
                // they are (corners derive height from width), clamped
                // to the limits so the derived edge can't escape them.
                let clamp = |v: i32, lo: u32, hi: u32| -> i32 {
                    let lo = to_phys(lo, dpi);
                    let hi = if hi == u32::MAX {
                        i32::MAX
                    } else {
                        to_phys(hi, dpi)
                    };
                    // Floors max at min - see `clamp_extent`; inconsistent
                    // editor bounds must not panic this `extern "system"`
                    // proc and abort the host.
                    clamp_extent(v, lo, hi)
                };
                // WMSZ_* edge codes are tiny (1..=8); the truncation
                // is nominal.
                #[allow(clippy::cast_possible_truncation)]
                let edge = wparam as u32;
                // Aspect numerator / denominator are small ratio
                // terms (e.g. 2:3), nowhere near the sign bit.
                let (aw, ah) = (aw.cast_signed(), ah.cast_signed());
                if edge == WMSZ_TOP || edge == WMSZ_BOTTOM {
                    let w = clamp(client_h * aw / ah, limits.min_w, limits.max_w);
                    r.right = r.left + w + chrome_w;
                } else {
                    let h = clamp(client_w * ah / aw, limits.min_h, limits.max_h);
                    if edge == WMSZ_TOPLEFT || edge == WMSZ_TOPRIGHT || edge == WMSZ_TOP {
                        r.top = r.bottom - h - chrome_h;
                    } else {
                        r.bottom = r.top + h + chrome_h;
                    }
                }
                let _ = (WMSZ_LEFT, WMSZ_RIGHT);
                1
            }
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, Some(size_limits_proc), SIZE_LIMITS_SUBCLASS);
                drop(Box::from_raw(refdata as *mut SizeLimits));
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
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

/// Extra **logical** height to add to a requested window height so
/// baseview's `Window::resize` leaves room for the menu bar. Returns
/// `0` when the window has no menu.
///
/// baseview computes the outer window size with `AdjustWindowRectEx(..,
/// bMenu = FALSE, ..)`, so it never reserves the `SM_CYMENU` band the
/// menu bar steals from the client area - undoing the one-shot
/// reservation [`crate::menu_windows`] installs at startup and
/// clipping the editor child by the menu-bar height on every
/// programmatic resize. We can't `SetWindowPos` ourselves from inside
/// an event handler (baseview already holds its handler `RefCell`
/// borrow, and the synchronous `WM_SIZE` would re-enter and panic -
/// the very reason baseview defers its own resizes). Instead we pad
/// the height handed to baseview's deferred resize by the menu band:
/// the menu eats the padding, leaving the intended client area.
///
/// The padding is the physical `SM_CYMENU` (matching
/// `menu_windows::grow_window_for_menu`) converted back to logical
/// points, since baseview re-multiplies by the window scale.
///
/// Must run on the window's thread (the main thread).
// `GetSystemMetrics` returns a non-negative menu height; the ceil'd
// logical value stays tiny.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
pub fn menu_reserve_logical(hwnd: *mut c_void) -> u32 {
    if hwnd.is_null() {
        return 0;
    }
    let hwnd = hwnd as HWND;

    // SAFETY: `hwnd` is the live baseview window on its own thread;
    // these are read-only metric / state queries with no re-entrancy.
    unsafe {
        if GetMenu(hwnd).is_null() {
            return 0;
        }
        let menu_px = GetSystemMetrics(SM_CYMENU);
        if menu_px <= 0 {
            return 0;
        }
        let dpi = GetDpiForWindow(hwnd);
        let scale = if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 };
        (f64::from(menu_px) / scale).ceil() as u32
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

#[cfg(test)]
mod tests {
    use super::clamp_extent;

    #[test]
    fn clamp_extent_floors_max_at_min_instead_of_panicking() {
        // Inconsistent bounds (max < min): the raw `clamp(min, max)`
        // would panic and abort the host; we pin to min instead.
        assert_eq!(clamp_extent(50, 100, 20), 100);
        // Normal bounds still clamp both ways.
        assert_eq!(clamp_extent(50, 10, 100), 50);
        assert_eq!(clamp_extent(5, 10, 100), 10);
        assert_eq!(clamp_extent(500, 10, 100), 100);
    }
}
