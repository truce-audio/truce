//! Windows native menu bar for the standalone host.
//!
//! Builds a Win32 `HMENU` with one top-level "Plugin" popup
//! containing:
//!
//! - **Mic Input** (checkable, `Ctrl+I` shown as the accelerator hint;
//!   effect plugins only)
//! - **Audio Output** (checkable mute toggle, `Ctrl+O`)
//! - **Input Device** submenu — repopulated from cpal on each open
//!   (effect plugins only)
//! - **Output Device** submenu — same for outputs
//!
//! Attached to the baseview window via `SetMenu`. Routes clicks
//! back to `InputController` / `OutputController` through a
//! `SetWindowSubclass` non-destructive subclass that intercepts
//! `WM_COMMAND` (item dispatch) and `WM_INITMENUPOPUP` (refresh
//! checkmarks + repopulate device lists).
//!
//! Differences from the macOS bridge (`menu_macos.rs`):
//!
//! - Windows menu bars sit *inside* the window's non-client area,
//!   not at the top of the screen. Adding the menu shrinks the
//!   client rect by `SM_CYMENU`, so we grow the parent window by
//!   the same amount before the editor child opens — the plugin
//!   keeps the size it asked for.
//! - There's no auto-populated "App" menu like Cocoa's. The
//!   window's `[X]` close button covers Quit; we ship just the
//!   Plugin menu.
//! - Cocoa's `Cmd+I` accelerator is wired by the menu item itself.
//!   Win32 needs a separate `HACCEL` table + `TranslateAccelerator`
//!   in the message loop, which baseview doesn't expose. The menu
//!   text shows `Ctrl+I` as a hint; the actual key is dispatched
//!   by the keyboard handler in `windowed.rs`.
//! - WM_COMMAND only carries the command ID, not the item's
//!   string. We use `GetMenuStringW` to look up the device name
//!   for the clicked ID rather than maintaining a parallel map.

#![cfg(all(target_os = "windows", feature = "gui"))]

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuItem, CreateMenu, CreatePopupMenu, DeleteMenu, DrawMenuBar,
    GetMenuItemCount, GetMenuStringW, GetSystemMetrics, GetWindowRect, HMENU, MF_BYCOMMAND,
    MF_BYPOSITION, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_STRING, MF_UNCHECKED, SM_CYMENU, SWP_NOMOVE,
    SWP_NOZORDER, SetMenu, SetWindowPos, WM_COMMAND, WM_INITMENUPOPUP, WM_NCDESTROY,
};

use crate::audio::{self, InputController, OutputController};
use crate::vlog;

/// Command ID for the mic-input toggle.
const MENU_CMD_MIC: u16 = 0xC001;
/// Command ID for the output (mute) toggle.
const MENU_CMD_OUTPUT: u16 = 0xC002;

/// Reserved command-ID ranges for dynamically-built device items.
/// 256 slots per side is more than any sane system would expose.
const MENU_CMD_INPUT_DEVICE_BASE: u16 = 0xC100;
const MENU_CMD_INPUT_DEVICE_END: u16 = 0xC1FF;
const MENU_CMD_OUTPUT_DEVICE_BASE: u16 = 0xC200;
const MENU_CMD_OUTPUT_DEVICE_END: u16 = 0xC2FF;

/// Subclass cookie. Picked to be visually distinct in a debugger;
/// any `usize` works as long as it's unique within the window.
const SUBCLASS_ID: usize = 0x7472_7563; // 'truc'

struct MenuState {
    input: InputController,
    output: OutputController,
    /// The Plugin popup itself — needed for `CheckMenuItem` on the
    /// toggles (whose commands live in the Plugin popup).
    hmenu_plugin: HMENU,
    /// True if the mic-input item is in the menu (effect plugins
    /// only). Gates `WM_COMMAND` dispatch and skips the
    /// checkmark refresh on `WM_INITMENUPOPUP` for instruments.
    has_mic_item: bool,
    /// `null` for instrument plugins (input device picker not built).
    hmenu_input_devices: HMENU,
    hmenu_output_devices: HMENU,
}

/// Install the native menu bar.
///
/// `is_effect` controls whether mic-input and input-device items
/// appear — input-side controls are useless for instruments and
/// analyzers since the runner feeds them silence.
pub fn install(
    hwnd: *mut c_void,
    _app_name: &str,
    is_effect: bool,
    input: InputController,
    output: OutputController,
) {
    if hwnd.is_null() {
        return;
    }
    let hwnd = hwnd as HWND;

    unsafe {
        let menu_bar = CreateMenu();
        let plugin_menu = CreatePopupMenu();
        let output_dev_menu = CreatePopupMenu();
        if menu_bar.is_null() || plugin_menu.is_null() || output_dev_menu.is_null() {
            return;
        }

        let input_dev_menu = if is_effect {
            let m = CreatePopupMenu();
            if m.is_null() {
                return;
            }
            m
        } else {
            std::ptr::null_mut()
        };

        // Mic-input item (effects only). `\t` separates the label
        // from the accelerator hint; Windows right-aligns the hint
        // in the popup. The hint is cosmetic — actual `Ctrl+I`
        // dispatch happens in the baseview keyboard handler.
        if is_effect {
            let item_text = wide("Mic Input\tCtrl+I");
            AppendMenuW(
                plugin_menu,
                MF_STRING,
                MENU_CMD_MIC as usize,
                item_text.as_ptr(),
            );
        }

        // Audio output mute toggle. Applies to every plugin
        // category. Initial state checkmark is set after install
        // via WM_INITMENUPOPUP.
        let output_text = wide("Audio Output\tCtrl+O");
        AppendMenuW(
            plugin_menu,
            MF_STRING,
            MENU_CMD_OUTPUT as usize,
            output_text.as_ptr(),
        );

        // Separator before the device pickers.
        AppendMenuW(
            plugin_menu,
            windows_sys::Win32::UI::WindowsAndMessaging::MF_SEPARATOR,
            0,
            std::ptr::null(),
        );

        // Input Device submenu — empty at install; repopulated on
        // WM_INITMENUPOPUP so hot-plug just works. Effects only.
        if is_effect {
            let input_label = wide("Input Device");
            AppendMenuW(
                plugin_menu,
                MF_POPUP,
                input_dev_menu as usize,
                input_label.as_ptr(),
            );
        }

        // Output Device submenu.
        let output_label = wide("Output Device");
        AppendMenuW(
            plugin_menu,
            MF_POPUP,
            output_dev_menu as usize,
            output_label.as_ptr(),
        );

        // Attach the Plugin popup to the menu bar.
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
            output,
            hmenu_plugin: plugin_menu,
            has_mic_item: is_effect,
            hmenu_input_devices: input_dev_menu,
            hmenu_output_devices: output_dev_menu,
        }));
        SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, state as usize);
    }
}

unsafe fn grow_window_for_menu(hwnd: HWND) {
    unsafe {
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
}

/// Subclassed window procedure. Handles WM_COMMAND for our menu
/// item (mic toggle + dynamic device items), refreshes the
/// checkmark / repopulates submenus on WM_INITMENUPOPUP, and
/// tears down the boxed state on WM_NCDESTROY.
unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _uid: usize,
    dwrefdata: usize,
) -> LRESULT {
    unsafe {
        let state_ptr = dwrefdata as *mut MenuState;

        match msg {
            WM_COMMAND => {
                if state_ptr.is_null() {
                    return DefSubclassProc(hwnd, msg, wparam, lparam);
                }
                let state = &*state_ptr;
                let cmd_id = (wparam & 0xFFFF) as u16;

                if cmd_id == MENU_CMD_MIC && state.has_mic_item {
                    let want = !state.input.is_enabled();
                    state.input.set_enabled(want);
                    vlog!(
                        "mic: {} (request, via menu)",
                        if want { "ON" } else { "OFF" }
                    );
                    let flag = if want { MF_CHECKED } else { MF_UNCHECKED };
                    CheckMenuItem(state.hmenu_plugin, MENU_CMD_MIC as u32, MF_BYCOMMAND | flag);
                    return 0;
                }

                if cmd_id == MENU_CMD_OUTPUT {
                    let want = !state.output.is_enabled();
                    state.output.set_enabled(want);
                    vlog!(
                        "output: {} (request, via menu)",
                        if want { "ON" } else { "OFF" }
                    );
                    let flag = if want { MF_CHECKED } else { MF_UNCHECKED };
                    CheckMenuItem(
                        state.hmenu_plugin,
                        MENU_CMD_OUTPUT as u32,
                        MF_BYCOMMAND | flag,
                    );
                    return 0;
                }

                if !state.hmenu_input_devices.is_null()
                    && (MENU_CMD_INPUT_DEVICE_BASE..=MENU_CMD_INPUT_DEVICE_END).contains(&cmd_id)
                {
                    if let Some(name) = get_menu_string(state.hmenu_input_devices, cmd_id as u32) {
                        vlog!("input device: {name}");
                        state.input.set_device(Some(name));
                    }
                    return 0;
                }

                if (MENU_CMD_OUTPUT_DEVICE_BASE..=MENU_CMD_OUTPUT_DEVICE_END).contains(&cmd_id) {
                    if let Some(name) = get_menu_string(state.hmenu_output_devices, cmd_id as u32) {
                        vlog!("output device: {name}");
                        state.output.set_device(Some(name));
                    }
                    return 0;
                }
            }
            WM_INITMENUPOPUP => {
                if state_ptr.is_null() {
                    return DefSubclassProc(hwnd, msg, wparam, lparam);
                }
                let state = &*state_ptr;
                let popup = wparam as HMENU;

                if !state.hmenu_input_devices.is_null() && popup == state.hmenu_input_devices {
                    let (_, names) = audio::list_input_devices();
                    let current = state.input.current_name();
                    repopulate_device_menu(
                        popup,
                        &names,
                        current.as_deref(),
                        MENU_CMD_INPUT_DEVICE_BASE,
                    );
                } else if popup == state.hmenu_output_devices {
                    let (_, names) = audio::list_output_devices();
                    let current = state.output.current_name();
                    repopulate_device_menu(
                        popup,
                        &names,
                        current.as_deref(),
                        MENU_CMD_OUTPUT_DEVICE_BASE,
                    );
                } else if popup == state.hmenu_plugin {
                    if state.has_mic_item {
                        let on = state.input.is_enabled();
                        let flag = if on { MF_CHECKED } else { MF_UNCHECKED };
                        CheckMenuItem(state.hmenu_plugin, MENU_CMD_MIC as u32, MF_BYCOMMAND | flag);
                    }
                    let out_on = state.output.is_enabled();
                    let out_flag = if out_on { MF_CHECKED } else { MF_UNCHECKED };
                    CheckMenuItem(
                        state.hmenu_plugin,
                        MENU_CMD_OUTPUT as u32,
                        MF_BYCOMMAND | out_flag,
                    );
                }
            }
            WM_NCDESTROY => {
                if !state_ptr.is_null() {
                    drop(Box::from_raw(state_ptr));
                }
                RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
            }
            _ => {}
        }

        DefSubclassProc(hwnd, msg, wparam, lparam)
    }
}

/// Replace all items in `popup` with one entry per device. Items
/// fire command IDs in `[cmd_base .. cmd_base + devices.len())`;
/// the matching device gets `MF_CHECKED`.
unsafe fn repopulate_device_menu(
    popup: HMENU,
    devices: &[String],
    current: Option<&str>,
    cmd_base: u16,
) {
    unsafe {
        // Remove all existing items. Always delete by position 0 since
        // the menu shrinks under us as we delete.
        let count = GetMenuItemCount(popup);
        for _ in 0..count {
            DeleteMenu(popup, 0, MF_BYPOSITION);
        }

        if devices.is_empty() {
            let text = wide("(no devices)");
            AppendMenuW(popup, MF_STRING | MF_GRAYED, 0, text.as_ptr());
            return;
        }

        for (i, name) in devices.iter().enumerate() {
            // Don't blow past the reserved range — a system with >256
            // devices on one side would silently drop the rest.
            if i >= 256 {
                break;
            }
            let text = wide(name);
            let cmd_id = cmd_base + i as u16;
            let mut flags = MF_STRING;
            if current.map(|c| c == name.as_str()).unwrap_or(false) {
                flags |= MF_CHECKED;
            }
            AppendMenuW(popup, flags, cmd_id as usize, text.as_ptr());
        }
    }
}

/// Look up a menu item's display string by command ID. Returns
/// `None` if the ID isn't in the menu (or `GetMenuStringW` fails).
unsafe fn get_menu_string(hmenu: HMENU, cmd_id: u32) -> Option<String> {
    unsafe {
        // First call with a null buffer to get the required length.
        let len = GetMenuStringW(hmenu, cmd_id, std::ptr::null_mut(), 0, MF_BYCOMMAND);
        if len <= 0 {
            return None;
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let written = GetMenuStringW(
            hmenu,
            cmd_id,
            buf.as_mut_ptr(),
            buf.len() as i32,
            MF_BYCOMMAND,
        );
        if written <= 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..written as usize]))
    }
}

/// UTF-8 → null-terminated UTF-16 (Win32's `W` APIs).
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
