//! Windows native menu bar for the standalone host.
//!
//! Builds a Win32 `HMENU` with one top-level "Plugin" popup
//! carrying a checkable mic-input item, attaches it to the
//! baseview window via `SetMenu`, and routes clicks back to
//! `InputController` through a `WM_COMMAND` window subclass.
//!
//! Differences from the macOS bridge (`menu_macos.rs`):
//!
//! - Windows menu bars sit *inside* the window's non-client area,
//!   not at the top of the screen. Adding the menu shrinks the
//!   client rect by `SM_CYMENU`, so we grow the parent window by
//!   the same amount before the editor child opens — the plugin
//!   keeps the size it asked for.
//! - There's no auto-populated "App" menu like Cocoa's. Standard
//!   Windows convention puts Quit / Exit on a "File" menu, but
//!   for a single-plugin standalone the window's `[X]` close
//!   button covers it. We ship just the Plugin menu for now.
//! - Cocoa's `Cmd+I` accelerator is wired by the menu item itself.
//!   Win32 needs a separate `HACCEL` table + `TranslateAccelerator`
//!   in the message loop, which baseview doesn't expose. The menu
//!   text shows `Ctrl+I` as a hint; the actual key is dispatched
//!   by the keyboard handler in `windowed.rs` (which already does
//!   bare `I`, extended to also accept `Ctrl+I` on Windows).

#![cfg(all(target_os = "windows", feature = "gui"))]

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuItem, CreateMenu, CreatePopupMenu, DrawMenuBar, GetSystemMetrics,
    GetWindowRect, SetMenu, SetWindowPos, HMENU, MF_BYCOMMAND, MF_CHECKED, MF_POPUP, MF_STRING,
    MF_UNCHECKED, SM_CYMENU, SWP_NOMOVE, SWP_NOZORDER, WM_COMMAND, WM_INITMENUPOPUP, WM_NCDESTROY,
};

use crate::audio::InputController;

/// Command ID for the mic-input menu item. Arbitrary value in the
/// app-private 0xC000–0xFFFF range so it doesn't collide with
/// any system-defined ID.
const MENU_CMD_MIC: u16 = 0xC001;

/// Subclass cookie. Picked to be visually distinct in a debugger;
/// any `usize` works as long as it's unique within the window.
const SUBCLASS_ID: usize = 0x7472_7563; // 'truc'

/// Heap-allocated state pinned by the subclass. Lives until
/// `WM_NCDESTROY`, which frees it via `Box::from_raw`.
struct MenuState {
    input: InputController,
    /// Plugin submenu — the one carrying the mic item. Stored so
    /// `CheckMenuItem` can target the right popup directly
    /// without walking the menu bar each time.
    hmenu_plugin: HMENU,
}

/// Install the native menu bar on the given baseview window.
///
/// Must be called on the UI thread. `hwnd` is the window handle
/// from baseview's raw-window-handle (`RwhHandle::Win32::hwnd`).
/// The window is grown by the menu's height so the editor child
/// (opened immediately after) keeps its requested size.
pub fn install(hwnd: *mut c_void, _app_name: &str, input: InputController) {
    if hwnd.is_null() {
        return;
    }
    let hwnd = hwnd as HWND;

    unsafe {
        let menu_bar = CreateMenu();
        let plugin_menu = CreatePopupMenu();
        if menu_bar.is_null() || plugin_menu.is_null() {
            return;
        }

        // Mic-input item. `\t` separates the label from the
        // accelerator hint; Windows right-aligns the hint in the
        // popup. The hint is cosmetic — actual `Ctrl+I` dispatch
        // happens in the baseview keyboard handler.
        let item_text = wide("Mic Input\tCtrl+I");
        AppendMenuW(
            plugin_menu,
            MF_STRING,
            MENU_CMD_MIC as usize,
            item_text.as_ptr(),
        );

        // Attach the popup to the menu bar.
        let plugin_label = wide("Plugin");
        AppendMenuW(
            menu_bar,
            MF_POPUP,
            plugin_menu as usize,
            plugin_label.as_ptr(),
        );

        SetMenu(hwnd, menu_bar);
        DrawMenuBar(hwnd);

        // Adding a menu shrinks the client area by SM_CYMENU.
        // Grow the window by the same amount so the editor child
        // keeps the dimensions it was sized for.
        grow_window_for_menu(hwnd);

        // Pin state + install subclass for WM_COMMAND routing.
        let state = Box::into_raw(Box::new(MenuState {
            input,
            hmenu_plugin: plugin_menu,
        }));
        SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, state as usize);
    }
}

unsafe fn grow_window_for_menu(hwnd: HWND) {
    let menu_h = GetSystemMetrics(SM_CYMENU);
    if menu_h <= 0 {
        return;
    }
    let mut rect: RECT = std::mem::zeroed();
    if GetWindowRect(hwnd, &mut rect) == 0 {
        return;
    }
    let w = rect.right - rect.left;
    let h = (rect.bottom - rect.top) + menu_h;
    SetWindowPos(
        hwnd,
        std::ptr::null_mut(),
        0,
        0,
        w,
        h,
        SWP_NOMOVE | SWP_NOZORDER,
    );
}

/// Subclassed window procedure. Handles WM_COMMAND for our menu
/// item, refreshes the checkmark on WM_INITMENUPOPUP (so opening
/// the menu always reflects the current `enabled` AtomicBool —
/// regardless of whether the last toggle came from menu, key,
/// or CLI), and tears down the boxed state on WM_NCDESTROY.
unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _uid: usize,
    dwrefdata: usize,
) -> LRESULT {
    let state_ptr = dwrefdata as *mut MenuState;

    match msg {
        WM_COMMAND => {
            let cmd_id = (wparam & 0xFFFF) as u16;
            if cmd_id == MENU_CMD_MIC && !state_ptr.is_null() {
                let state = &*state_ptr;
                let want = !state.input.is_enabled();
                state.input.set_enabled(want);
                eprintln!(
                    "[truce-standalone] mic: {} (request, via menu)",
                    if want { "ON" } else { "OFF" }
                );
                let flag = if want { MF_CHECKED } else { MF_UNCHECKED };
                CheckMenuItem(state.hmenu_plugin, MENU_CMD_MIC as u32, MF_BYCOMMAND | flag);
                return 0;
            }
        }
        WM_INITMENUPOPUP => {
            if !state_ptr.is_null() {
                let state = &*state_ptr;
                let on = state.input.is_enabled();
                let flag = if on { MF_CHECKED } else { MF_UNCHECKED };
                CheckMenuItem(state.hmenu_plugin, MENU_CMD_MIC as u32, MF_BYCOMMAND | flag);
            }
        }
        WM_NCDESTROY => {
            // Reclaim the boxed state and remove ourselves from
            // the subclass chain. WM_NCDESTROY is the last
            // message a window receives, so this is the only
            // place we can guarantee no further dispatches.
            if !state_ptr.is_null() {
                drop(Box::from_raw(state_ptr));
            }
            RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
        }
        _ => {}
    }

    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// UTF-8 → null-terminated UTF-16 (Win32's `W` APIs).
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
