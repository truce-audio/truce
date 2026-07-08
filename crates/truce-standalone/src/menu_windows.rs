//! Windows native menu bar for the standalone host.
//!
//! Builds a Win32 `HMENU` with one top-level "Settings" popup
//! carrying both the audio and MIDI controls:
//!
//! - **Mic Input** (checkable, `Ctrl+I` shown as the accelerator hint;
//!   effect plugins only)
//! - **Audio Output** (checkable mute toggle, `Ctrl+O`)
//! - **Input Device** submenu - repopulated from cpal on each open
//!   (effect plugins only)
//! - **Output Device** submenu - same for outputs
//! - **Input / Output Channels** submenus - channel routing (when the
//!   device exposes >= 2 channels)
//! - **MIDI Input** submenu(s) - one per plugin MIDI input port
//!   ("MIDI Input - Port k" when the plugin declares more than one,
//!   up to `MIDI_MENU_MAX_PORTS` in the menu), each listing MIDI
//!   devices (repopulated on open)
//! - **MIDI Channel** submenu - Omni / channel 1-16 filter
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
//!   the same amount before the editor child opens - the plugin
//!   keeps the size it asked for.
//! - There's no auto-populated "App" menu like Cocoa's. The
//!   window's `[X]` close button covers Quit; we ship just the
//!   Settings menu.
//! - Cocoa's `Cmd+I` accelerator is wired by the menu item itself.
//!   Win32 needs a separate `HACCEL` table + `TranslateAccelerator`
//!   in the message loop, which baseview doesn't expose. The menu
//!   text shows `Ctrl+I` as a hint; the actual key is dispatched
//!   by the keyboard handler in `windowed.rs`.
//! - `WM_COMMAND` only carries the command ID, not the item's
//!   string. We use `GetMenuStringW` to look up the device name
//!   for the clicked ID rather than maintaining a parallel map.

#![cfg(all(target_os = "windows", feature = "gui"))]

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuItem, CreateMenu, CreatePopupMenu, DeleteMenu, DrawMenuBar,
    EnableMenuItem, GetMenuItemCount, GetMenuStringW, GetSystemMetrics, GetWindowRect, HMENU,
    MF_BYCOMMAND, MF_BYPOSITION, MF_CHECKED, MF_ENABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, MF_UNCHECKED, ModifyMenuW, SM_CYMENU, SWP_NOMOVE, SWP_NOZORDER, SetMenu,
    SetWindowPos, WM_COMMAND, WM_INITMENUPOPUP, WM_NCDESTROY,
};

use crate::audio::{self, ChannelRoute, InputController, OutputController};
use crate::midi::{self, MIDI_MENU_MAX_PORTS, MidiChannel, MidiController};
use crate::presets::PresetController;
use crate::vlog;

/// Command ID for the mic-input toggle.
const MENU_CMD_MIC: u16 = 0xC001;
/// Command ID for the output (mute) toggle.
const MENU_CMD_OUTPUT: u16 = 0xC002;
/// Command ID for the computer-keyboard-to-MIDI toggle.
const MENU_CMD_KEYBOARD: u16 = 0xC003;

/// Reserved command-ID ranges for dynamically-built device items.
/// 256 slots per side is more than any sane system would expose.
const MENU_CMD_INPUT_DEVICE_BASE: u16 = 0xC100;
const MENU_CMD_INPUT_DEVICE_END: u16 = 0xC1FF;
const MENU_CMD_OUTPUT_DEVICE_BASE: u16 = 0xC200;
const MENU_CMD_OUTPUT_DEVICE_END: u16 = 0xC2FF;

/// Channel-routing item ranges. The command ID carries the
/// [`ChannelRoute`] directly: `cmd == base + route.encode()`, so a
/// click decodes back to the route with no name lookup. `encode()`
/// stays small (`<= 2 * channels`), well within 256 slots.
const MENU_CMD_INPUT_CHANNELS_BASE: u16 = 0xC300;
const MENU_CMD_INPUT_CHANNELS_END: u16 = 0xC3FF;
const MENU_CMD_OUTPUT_CHANNELS_BASE: u16 = 0xC400;
const MENU_CMD_OUTPUT_CHANNELS_END: u16 = 0xC4FF;

/// MIDI-input device items, one strided block per plugin MIDI input
/// port inside `[0xC900, 0xCCFF]`. Port `p`'s block starts at
/// `MENU_CMD_MIDI_INPUT_BASE + p * MENU_CMD_MIDI_PORT_STRIDE`: the
/// first ID disconnects that port ("None"), the rest are one device
/// each (name recovered via `GetMenuStringW`, like the audio device
/// menus). The 0xC900..=0xCCFF window holds stride 64 x
/// `MIDI_MENU_MAX_PORTS` (16) command ids; deeper multi-port plugins
/// still route via `--midi-input` on the CLI, just without a menu row.
const MENU_CMD_MIDI_INPUT_BASE: u16 = 0xC900;
const MENU_CMD_MIDI_INPUT_END: u16 = 0xCCFF;
const MENU_CMD_MIDI_PORT_STRIDE: u16 = 64;
const MENU_CMD_MIDI_MAX_PORTS: usize = MIDI_MENU_MAX_PORTS;

/// MIDI channel items: `cmd == base + MidiChannel::encode()` (Omni
/// encodes to 0xFF, channels to 0-15).
const MENU_CMD_MIDI_CHANNEL_BASE: u16 = 0xC600;
const MENU_CMD_MIDI_CHANNEL_END: u16 = 0xC6FF;

/// Preset commands. Previous / Next / Save are fixed; the Load
/// submenu items occupy a range, command ID `BASE + index` into
/// `PresetController::entries` (libraries beyond the range are
/// truncated in the menu, logged at build).
const MENU_CMD_PRESET_PREV: u16 = 0xC701;
const MENU_CMD_PRESET_NEXT: u16 = 0xC702;
const MENU_CMD_PRESET_SAVE: u16 = 0xC703;
const MENU_CMD_PRESET_SAVE_AS: u16 = 0xC704;
const MENU_CMD_PRESET_LOAD_BASE: u16 = 0xC800;
const MENU_CMD_PRESET_LOAD_END: u16 = 0xC8FF;

/// Subclass cookie. Picked to be visually distinct in a debugger;
/// any `usize` works as long as it's unique within the window.
const SUBCLASS_ID: usize = 0x7472_7563; // 'truc'

struct MenuState {
    input: InputController,
    output: OutputController,
    /// QWERTY-keyboard-to-MIDI flag, shared with the key handler and
    /// the Ctrl+K shortcut. The menu item toggles it.
    keyboard: Arc<AtomicBool>,
    /// The Plugin popup itself - needed for `CheckMenuItem` on the
    /// toggles (whose commands live in the Plugin popup).
    hmenu_plugin: HMENU,
    /// True if the mic-input item is in the menu (effect plugins
    /// only). Gates `WM_COMMAND` dispatch and skips the
    /// checkmark refresh on `WM_INITMENUPOPUP` for instruments.
    has_mic_item: bool,
    /// `null` for instrument plugins (input device picker not built).
    hmenu_input_devices: HMENU,
    hmenu_output_devices: HMENU,
    /// Channel-routing submenus. `null` when the device exposes fewer
    /// than two channels (nothing to pick) or, for input, on
    /// instruments. Repopulated on `WM_INITMENUPOPUP`.
    hmenu_input_channels: HMENU,
    hmenu_output_channels: HMENU,
    /// Device channel count, used to build the channel submenus.
    channels: usize,
    /// MIDI device / channel control + its submenus, repopulated on
    /// `WM_INITMENUPOPUP`.
    midi: MidiController,
    /// One MIDI-input device popup per plugin MIDI input port, indexed
    /// by port. Length is `min(port_count, MENU_CMD_MIDI_MAX_PORTS)`.
    hmenu_midi_inputs: Vec<HMENU>,
    hmenu_midi_channel: HMENU,
    /// Preset library handle backing the Presets menu.
    presets: PresetController,
    /// The Presets popup - matched on `WM_INITMENUPOPUP` to refresh
    /// its children.
    hmenu_presets: HMENU,
    /// The Presets > Load submenu - re-enumerated on each open so
    /// saves appear without a relaunch; also read by `get_menu_string`
    /// to dispatch a Load click by its label.
    hmenu_preset_load: HMENU,
}

/// Install the native menu bar.
///
/// `is_effect` controls whether mic-input and input-device items
/// appear - input-side controls are useless for instruments and
/// analyzers since the runner feeds them silence.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn install(
    hwnd: *mut c_void,
    _app_name: &str,
    is_effect: bool,
    channels: usize,
    input: InputController,
    output: OutputController,
    midi: MidiController,
    presets: PresetController,
    qwerty: Arc<AtomicBool>,
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
        // in the popup. The hint is cosmetic - actual `Ctrl+I`
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

        // Computer-keyboard-to-MIDI toggle. Off by default; the
        // checkmark is set from the shared flag on WM_INITMENUPOPUP.
        let keyboard_text = wide("Computer Keyboard\tCtrl+K");
        AppendMenuW(
            plugin_menu,
            MF_STRING,
            MENU_CMD_KEYBOARD as usize,
            keyboard_text.as_ptr(),
        );

        // Separator before the device pickers.
        AppendMenuW(
            plugin_menu,
            windows_sys::Win32::UI::WindowsAndMessaging::MF_SEPARATOR,
            0,
            std::ptr::null(),
        );

        // Input Device submenu - empty at install; repopulated on
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

        // Channel-routing submenus. Only worth showing when the device
        // has >= 2 channels. Empty at install; repopulated (with the
        // current selection checked) on WM_INITMENUPOPUP.
        let (input_ch_menu, output_ch_menu) = if channels >= 2 {
            AppendMenuW(plugin_menu, MF_SEPARATOR, 0, std::ptr::null());

            let in_ch = if is_effect {
                let m = CreatePopupMenu();
                if !m.is_null() {
                    let label = wide("Input Channels");
                    AppendMenuW(plugin_menu, MF_POPUP, m as usize, label.as_ptr());
                }
                m
            } else {
                std::ptr::null_mut()
            };

            let out_ch = CreatePopupMenu();
            if !out_ch.is_null() {
                let label = wide("Output Channels");
                AppendMenuW(plugin_menu, MF_POPUP, out_ch as usize, label.as_ptr());
            }
            (in_ch, out_ch)
        } else {
            (std::ptr::null_mut(), std::ptr::null_mut())
        };

        // MIDI section, appended into the same Settings popup behind a
        // separator: one input-device picker per plugin MIDI input
        // port + a channel filter. Submenus are empty here; repopulated
        // on open. Built for every plugin (any can receive MIDI).
        let midi_ports = midi.port_count().max(1);
        let menu_ports = midi_ports.min(MENU_CMD_MIDI_MAX_PORTS);
        if midi_ports > MENU_CMD_MIDI_MAX_PORTS {
            vlog!(
                "MIDI menu shows the first {MENU_CMD_MIDI_MAX_PORTS} of {midi_ports} input ports; \
                 route the rest with --midi-input"
            );
        }
        let midi_channel_menu = CreatePopupMenu();
        let mut hmenu_midi_inputs: Vec<HMENU> = Vec::with_capacity(menu_ports);
        if !midi_channel_menu.is_null() {
            AppendMenuW(plugin_menu, MF_SEPARATOR, 0, std::ptr::null());
            for port in 0..menu_ports {
                let popup = CreatePopupMenu();
                if popup.is_null() {
                    break;
                }
                let label = if midi_ports == 1 {
                    "MIDI Input".to_string()
                } else {
                    format!("MIDI Input - Port {}", port + 1)
                };
                let in_label = wide(&label);
                AppendMenuW(plugin_menu, MF_POPUP, popup as usize, in_label.as_ptr());
                hmenu_midi_inputs.push(popup);
            }
            let ch_label = wide("MIDI Channel");
            AppendMenuW(
                plugin_menu,
                MF_POPUP,
                midi_channel_menu as usize,
                ch_label.as_ptr(),
            );
        }

        // Attach the Settings popup (audio + MIDI) to the menu bar.
        let plugin_label = wide("Settings");
        AppendMenuW(
            menu_bar,
            MF_POPUP,
            plugin_menu as usize,
            plugin_label.as_ptr(),
        );

        let (hmenu_presets, hmenu_preset_load) = build_presets_menu(menu_bar, &presets);

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
            keyboard: qwerty,
            hmenu_plugin: plugin_menu,
            has_mic_item: is_effect,
            hmenu_input_devices: input_dev_menu,
            hmenu_output_devices: output_dev_menu,
            hmenu_input_channels: input_ch_menu,
            hmenu_output_channels: output_ch_menu,
            channels,
            midi,
            hmenu_midi_inputs,
            hmenu_midi_channel: midi_channel_menu,
            presets,
            hmenu_presets,
            hmenu_preset_load,
        }));
        SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, state as usize);
    }
}

/// Build the Presets popup (Load submenu + Previous / Next / Save /
/// Save As) and attach it to `menu_bar`. Returns
/// `(presets_popup, load_submenu)` so the subclass proc can refresh
/// them on open. Both are null if creation failed.
unsafe fn build_presets_menu(menu_bar: HMENU, presets: &PresetController) -> (HMENU, HMENU) {
    unsafe {
        let presets_menu = CreatePopupMenu();
        if presets_menu.is_null() {
            return (std::ptr::null_mut(), std::ptr::null_mut());
        }
        let load_menu = CreatePopupMenu();
        if !load_menu.is_null() {
            populate_preset_load(load_menu, &presets.entries());
            let load_label = wide("Load");
            AppendMenuW(
                presets_menu,
                MF_POPUP,
                load_menu as usize,
                load_label.as_ptr(),
            );
        }
        AppendMenuW(presets_menu, MF_SEPARATOR, 0, std::ptr::null());
        let prev_label = wide("Previous Preset");
        AppendMenuW(
            presets_menu,
            MF_STRING,
            MENU_CMD_PRESET_PREV as usize,
            prev_label.as_ptr(),
        );
        let next_label = wide("Next Preset");
        AppendMenuW(
            presets_menu,
            MF_STRING,
            MENU_CMD_PRESET_NEXT as usize,
            next_label.as_ptr(),
        );
        AppendMenuW(presets_menu, MF_SEPARATOR, 0, std::ptr::null());
        // No accelerator hint: Ctrl+S / Ctrl+Shift+S are dispatched by
        // the window's own key handler. The Save title is set on open.
        let save_label = wide("Save Preset");
        AppendMenuW(
            presets_menu,
            MF_STRING,
            MENU_CMD_PRESET_SAVE as usize,
            save_label.as_ptr(),
        );
        let save_as_label = wide("Save Preset As...");
        AppendMenuW(
            presets_menu,
            MF_STRING,
            MENU_CMD_PRESET_SAVE_AS as usize,
            save_as_label.as_ptr(),
        );
        let presets_label = wide("Presets");
        AppendMenuW(
            menu_bar,
            MF_POPUP,
            presets_menu as usize,
            presets_label.as_ptr(),
        );
        (presets_menu, load_menu)
    }
}

/// Fill (or refill) the Load submenu, one item per entry titled with
/// its display label and command ID `BASE + index`. Dispatch reads
/// the clicked item's label, so the index only has to stay unique.
/// Entries beyond the command-ID range are dropped (logged).
unsafe fn populate_preset_load(load_menu: HMENU, entries: &[crate::presets::PresetMenuEntry]) {
    unsafe {
        let count = GetMenuItemCount(load_menu);
        for _ in 0..count {
            DeleteMenu(load_menu, 0, MF_BYPOSITION);
        }
        let cap = usize::from(MENU_CMD_PRESET_LOAD_END - MENU_CMD_PRESET_LOAD_BASE) + 1;
        if entries.is_empty() {
            let none = wide("(no presets)");
            AppendMenuW(load_menu, MF_STRING | MF_GRAYED, 0, none.as_ptr());
            return;
        }
        if entries.len() > cap {
            vlog!(
                "presets: showing first {cap} of {} in the Load menu",
                entries.len()
            );
        }
        for (i, entry) in entries.iter().take(cap).enumerate() {
            let label = wide(&entry.label);
            AppendMenuW(
                load_menu,
                MF_STRING,
                MENU_CMD_PRESET_LOAD_BASE as usize + i,
                label.as_ptr(),
            );
        }
    }
}

unsafe fn grow_window_for_menu(hwnd: HWND) {
    unsafe {
        let menu_h = GetSystemMetrics(SM_CYMENU);
        if menu_h <= 0 {
            return;
        }
        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &raw mut rect) == 0 {
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

/// Subclassed window procedure. Handles `WM_COMMAND` for our menu
/// item (mic toggle + dynamic device items), refreshes the
/// checkmark / repopulates submenus on `WM_INITMENUPOPUP`, and
/// tears down the boxed state on `WM_NCDESTROY`.
// Why: `(wparam & 0xFFFF) as u16` is the canonical Win32 LOWORD shape -
// the high bits of WPARAM are reserved/zero on WM_COMMAND, so the
// truncation is the contract.
#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
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
                    CheckMenuItem(
                        state.hmenu_plugin,
                        u32::from(MENU_CMD_MIC),
                        MF_BYCOMMAND | flag,
                    );
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
                        u32::from(MENU_CMD_OUTPUT),
                        MF_BYCOMMAND | flag,
                    );
                    return 0;
                }

                if cmd_id == MENU_CMD_KEYBOARD {
                    let want = !state.keyboard.load(Ordering::Relaxed);
                    state.keyboard.store(want, Ordering::Relaxed);
                    vlog!(
                        "computer keyboard: {} (request, via menu)",
                        if want { "ON" } else { "OFF" }
                    );
                    let flag = if want { MF_CHECKED } else { MF_UNCHECKED };
                    CheckMenuItem(
                        state.hmenu_plugin,
                        u32::from(MENU_CMD_KEYBOARD),
                        MF_BYCOMMAND | flag,
                    );
                    return 0;
                }

                // Preset Load item: dispatch by the clicked item's
                // label (re-enumeration may have shifted indices).
                if (MENU_CMD_PRESET_LOAD_BASE..=MENU_CMD_PRESET_LOAD_END).contains(&cmd_id) {
                    if let Some(label) = get_menu_string(state.hmenu_preset_load, u32::from(cmd_id))
                    {
                        state.presets.load_by_label(&label);
                    }
                    return 0;
                }
                if cmd_id == MENU_CMD_PRESET_PREV {
                    state.presets.step(-1);
                    return 0;
                }
                if cmd_id == MENU_CMD_PRESET_NEXT {
                    state.presets.step(1);
                    return 0;
                }
                if cmd_id == MENU_CMD_PRESET_SAVE {
                    state.presets.save();
                    return 0;
                }
                if cmd_id == MENU_CMD_PRESET_SAVE_AS {
                    state.presets.save_as();
                    return 0;
                }

                if !state.hmenu_input_devices.is_null()
                    && (MENU_CMD_INPUT_DEVICE_BASE..=MENU_CMD_INPUT_DEVICE_END).contains(&cmd_id)
                {
                    if let Some(name) =
                        get_menu_string(state.hmenu_input_devices, u32::from(cmd_id))
                    {
                        vlog!("input device: {name}");
                        state.input.set_device(Some(name));
                    }
                    return 0;
                }

                if (MENU_CMD_OUTPUT_DEVICE_BASE..=MENU_CMD_OUTPUT_DEVICE_END).contains(&cmd_id) {
                    if let Some(name) =
                        get_menu_string(state.hmenu_output_devices, u32::from(cmd_id))
                    {
                        vlog!("output device: {name}");
                        state.output.set_device(Some(name));
                    }
                    return 0;
                }

                if !state.hmenu_input_channels.is_null()
                    && (MENU_CMD_INPUT_CHANNELS_BASE..=MENU_CMD_INPUT_CHANNELS_END)
                        .contains(&cmd_id)
                {
                    let route =
                        ChannelRoute::decode(usize::from(cmd_id - MENU_CMD_INPUT_CHANNELS_BASE));
                    vlog!("input channels: {route:?}");
                    state.input.set_channel_route(route);
                    return 0;
                }

                if !state.hmenu_output_channels.is_null()
                    && (MENU_CMD_OUTPUT_CHANNELS_BASE..=MENU_CMD_OUTPUT_CHANNELS_END)
                        .contains(&cmd_id)
                {
                    let route =
                        ChannelRoute::decode(usize::from(cmd_id - MENU_CMD_OUTPUT_CHANNELS_BASE));
                    vlog!("output channels: {route:?}");
                    state.output.set_channel_route(route);
                    return 0;
                }

                if (MENU_CMD_MIDI_INPUT_BASE..=MENU_CMD_MIDI_INPUT_END).contains(&cmd_id) {
                    // Decode the strided block: which plugin port, and
                    // whether it's that port's "None" (disconnect) row.
                    let rel = cmd_id - MENU_CMD_MIDI_INPUT_BASE;
                    let port = (rel / MENU_CMD_MIDI_PORT_STRIDE) as u8;
                    let is_none = rel.is_multiple_of(MENU_CMD_MIDI_PORT_STRIDE);
                    if is_none {
                        vlog!("midi input (port {port}): none");
                        state.midi.set_device_on(port, None);
                    } else if let Some(popup) = state.hmenu_midi_inputs.get(usize::from(port))
                        && let Some(name) = get_menu_string(*popup, u32::from(cmd_id))
                    {
                        vlog!("midi input (port {port}): {name}");
                        state.midi.set_device_on(port, Some(name));
                    }
                    return 0;
                }

                if (MENU_CMD_MIDI_CHANNEL_BASE..=MENU_CMD_MIDI_CHANNEL_END).contains(&cmd_id) {
                    let channel = MidiChannel::decode((cmd_id - MENU_CMD_MIDI_CHANNEL_BASE) as u8);
                    vlog!("midi channel: {channel:?}");
                    state.midi.set_channel(channel);
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
                } else if !state.hmenu_input_channels.is_null()
                    && popup == state.hmenu_input_channels
                {
                    repopulate_channel_menu(
                        popup,
                        state.channels,
                        state.input.channel_route(),
                        MENU_CMD_INPUT_CHANNELS_BASE,
                    );
                } else if !state.hmenu_output_channels.is_null()
                    && popup == state.hmenu_output_channels
                {
                    repopulate_channel_menu(
                        popup,
                        state.channels,
                        state.output.channel_route(),
                        MENU_CMD_OUTPUT_CHANNELS_BASE,
                    );
                } else if let Some(port) = state
                    .hmenu_midi_inputs
                    .iter()
                    .position(|&m| !m.is_null() && m == popup)
                {
                    let names = midi::list_midi_devices();
                    // Port index fits u8 (bounded by the plugin's MIDI
                    // input port count, itself capped at
                    // `MIDI_MENU_MAX_PORTS` in the menu).
                    #[allow(clippy::cast_possible_truncation)]
                    let port = port as u8;
                    let current = state.midi.current_name_on(port);
                    repopulate_midi_input_menu(popup, port, &names, current.as_deref());
                } else if !state.hmenu_midi_channel.is_null() && popup == state.hmenu_midi_channel {
                    repopulate_midi_channel_menu(popup, state.midi.channel());
                } else if !state.hmenu_presets.is_null() && popup == state.hmenu_presets {
                    // Re-enumerate the library (saves this session
                    // appear) and retitle Save to its target file.
                    if !state.hmenu_preset_load.is_null() {
                        populate_preset_load(state.hmenu_preset_load, &state.presets.entries());
                    }
                    let save_title = wide(&state.presets.save_menu_title());
                    ModifyMenuW(
                        state.hmenu_presets,
                        u32::from(MENU_CMD_PRESET_SAVE),
                        MF_BYCOMMAND | MF_STRING,
                        MENU_CMD_PRESET_SAVE as usize,
                        save_title.as_ptr(),
                    );
                    // Gray Save out unless an editable preset is
                    // loaded; Save As stays the way forward.
                    let save_flag = if state.presets.save_enabled() {
                        MF_ENABLED
                    } else {
                        MF_GRAYED
                    };
                    EnableMenuItem(
                        state.hmenu_presets,
                        u32::from(MENU_CMD_PRESET_SAVE),
                        MF_BYCOMMAND | save_flag,
                    );
                } else if popup == state.hmenu_plugin {
                    if state.has_mic_item {
                        let on = state.input.is_enabled();
                        let flag = if on { MF_CHECKED } else { MF_UNCHECKED };
                        CheckMenuItem(
                            state.hmenu_plugin,
                            u32::from(MENU_CMD_MIC),
                            MF_BYCOMMAND | flag,
                        );
                    }
                    let out_on = state.output.is_enabled();
                    let out_flag = if out_on { MF_CHECKED } else { MF_UNCHECKED };
                    CheckMenuItem(
                        state.hmenu_plugin,
                        u32::from(MENU_CMD_OUTPUT),
                        MF_BYCOMMAND | out_flag,
                    );
                    let kbd_on = state.keyboard.load(Ordering::Relaxed);
                    let kbd_flag = if kbd_on { MF_CHECKED } else { MF_UNCHECKED };
                    CheckMenuItem(
                        state.hmenu_plugin,
                        u32::from(MENU_CMD_KEYBOARD),
                        MF_BYCOMMAND | kbd_flag,
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
// Why: `i as u16` for the menu command ID - the loop body breaks at
// `i >= 256` so the cast is bounded well below `u16::MAX`.
#[allow(clippy::cast_possible_truncation)]
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
            // Don't blow past the reserved range - a system with >256
            // devices on one side would silently drop the rest.
            if i >= 256 {
                break;
            }
            let text = wide(name);
            let cmd_id = cmd_base + i as u16;
            let mut flags = MF_STRING;
            if current.is_some_and(|c| c == name.as_str()) {
                flags |= MF_CHECKED;
            }
            AppendMenuW(popup, flags, cmd_id as usize, text.as_ptr());
        }
    }
}

/// Rebuild a channel-routing popup: a "direct" default, then one item
/// per stereo pair, then one per mono channel. Each item's command ID
/// is `cmd_base + route.encode()`; the item matching `current` gets
/// `MF_CHECKED`.
unsafe fn repopulate_channel_menu(
    popup: HMENU,
    channels: usize,
    current: ChannelRoute,
    cmd_base: u16,
) {
    unsafe {
        let count = GetMenuItemCount(popup);
        for _ in 0..count {
            DeleteMenu(popup, 0, MF_BYPOSITION);
        }

        let cur = current.encode();
        append_channel_item(
            popup,
            cmd_base,
            ChannelRoute::Direct,
            "All channels (direct)",
            cur,
        );

        if channels >= 2 {
            AppendMenuW(popup, MF_SEPARATOR, 0, std::ptr::null());
            let mut base = 0;
            while base + 1 < channels {
                let label = format!("Channels {} & {}", base + 1, base + 2);
                append_channel_item(popup, cmd_base, ChannelRoute::Stereo { base }, &label, cur);
                base += 2;
            }
        }

        AppendMenuW(popup, MF_SEPARATOR, 0, std::ptr::null());
        for c in 0..channels {
            let label = format!("Channel {} (mono)", c + 1);
            append_channel_item(popup, cmd_base, ChannelRoute::Mono { base: c }, &label, cur);
        }
    }
}

/// Append one channel-routing item, checked when its encoded route is
/// the active one.
unsafe fn append_channel_item(
    popup: HMENU,
    cmd_base: u16,
    route: ChannelRoute,
    label: &str,
    current_encoded: usize,
) {
    unsafe {
        let encoded = route.encode();
        let cmd = cmd_base.wrapping_add(u16::try_from(encoded).unwrap_or(0));
        let mut flags = MF_STRING;
        if encoded == current_encoded {
            flags |= MF_CHECKED;
        }
        let text = wide(label);
        AppendMenuW(popup, flags, cmd as usize, text.as_ptr());
    }
}

/// Rebuild the MIDI-input popup: a "None" row (disconnect) then one
/// row per port. The active device (or "None") is checked. Device rows
/// fire IDs in `[MENU_CMD_MIDI_INPUT_BASE, ..)`; their names are
/// recovered with `GetMenuStringW`, like the audio device menus.
unsafe fn repopulate_midi_input_menu(
    popup: HMENU,
    port: u8,
    devices: &[String],
    current: Option<&str>,
) {
    unsafe {
        let count = GetMenuItemCount(popup);
        for _ in 0..count {
            DeleteMenu(popup, 0, MF_BYPOSITION);
        }

        // Port `port`'s command block: `base` disconnects it, `base +
        // 1 + i` selects device `i` (bounded by the stride).
        let base = MENU_CMD_MIDI_INPUT_BASE + u16::from(port) * MENU_CMD_MIDI_PORT_STRIDE;

        let mut none_flags = MF_STRING;
        if current.is_none() {
            none_flags |= MF_CHECKED;
        }
        let none = wide("None");
        AppendMenuW(popup, none_flags, base as usize, none.as_ptr());

        if devices.is_empty() {
            return;
        }
        AppendMenuW(popup, MF_SEPARATOR, 0, std::ptr::null());
        for (i, name) in devices.iter().enumerate() {
            // One device ID per slot after `base`, capped by the stride.
            let Ok(offset) = u16::try_from(i + 1) else {
                break;
            };
            if offset >= MENU_CMD_MIDI_PORT_STRIDE {
                break;
            }
            let mut flags = MF_STRING;
            if current.is_some_and(|c| c == name.as_str()) {
                flags |= MF_CHECKED;
            }
            let text = wide(name);
            let cmd_id = base + offset;
            AppendMenuW(popup, flags, cmd_id as usize, text.as_ptr());
        }
    }
}

/// Rebuild the MIDI-channel popup: an "Omni" row then channels 1-16,
/// each firing `MENU_CMD_MIDI_CHANNEL_BASE + MidiChannel::encode()`.
/// The active channel is checked.
unsafe fn repopulate_midi_channel_menu(popup: HMENU, current: MidiChannel) {
    unsafe {
        let count = GetMenuItemCount(popup);
        for _ in 0..count {
            DeleteMenu(popup, 0, MF_BYPOSITION);
        }

        let cur = current.encode();
        append_midi_channel_item(popup, MidiChannel::Omni, "Omni (all channels)", cur);
        AppendMenuW(popup, MF_SEPARATOR, 0, std::ptr::null());
        for n in 0..16u8 {
            let label = format!("Channel {}", n + 1);
            append_midi_channel_item(popup, MidiChannel::Channel(n), &label, cur);
        }
    }
}

/// Append one MIDI-channel item, checked when it's the active channel.
unsafe fn append_midi_channel_item(
    popup: HMENU,
    channel: MidiChannel,
    label: &str,
    current_encoded: u8,
) {
    unsafe {
        let encoded = channel.encode();
        let cmd = MENU_CMD_MIDI_CHANNEL_BASE.wrapping_add(u16::from(encoded));
        let mut flags = MF_STRING;
        if encoded == current_encoded {
            flags |= MF_CHECKED;
        }
        let text = wide(label);
        AppendMenuW(popup, flags, cmd as usize, text.as_ptr());
    }
}

/// Look up a menu item's display string by command ID. Returns
/// `None` if the ID isn't in the menu (or `GetMenuStringW` fails).
// Why: `GetMenuStringW` returns the char count as `int` (always
// non-negative on success - we early-return on `len <= 0`); the buffer
// is sized from that count and stays well below `i32::MAX`. Casts here
// are FFI-bounded by Win32's own API contract.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
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
