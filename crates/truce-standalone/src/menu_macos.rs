//! macOS native menu bar for the standalone host.
//!
//! Builds an `NSMenu` with two top-level items: an autopopulated
//! App menu (Quit / Hide / About) and a single "Settings" menu
//! carrying both the audio and MIDI controls:
//!
//! - **Mic Input** (toggle, ⌘I, checkmark when on; effect plugins only)
//! - **Audio Output** (toggle, ⌘O, checkmark when unmuted)
//! - **Input Device** submenu - lists cpal-visible inputs (effects only)
//! - **Output Device** submenu - same for outputs
//! - **Input / Output Channels** submenus - channel routing (when the
//!   device exposes >= 2 channels)
//! - **MIDI Input** submenu - lists MIDI ports (repopulated on open)
//! - **MIDI Channel** submenu - Omni / channel 1-16 filter
//!
//! Installed via `NSApp.setMainMenu(...)`.
//!
//! Action wiring uses a custom `TruceMenuTarget` Objective-C
//! class declared at runtime. The class has one ivar - a raw
//! pointer to a `MenuState` heap-allocated by Rust - and four
//! action selectors (`toggleInputAction:`, `toggleOutputAction:`,
//! `selectInputDeviceAction:`, `selectOutputDeviceAction:`) that
//! dereference the pointer and route the click to the matching
//! `InputController` / `OutputController` method.
//!
//! Menu state (the mic + output checkmarks + the active-device
//! checkmark in each device submenu) is refreshed on
//! `menuWillOpen:`. Device submenus are also *repopulated* on each
//! open from cpal's live device list, so hot-plug is reflected
//! without restarting.

#![cfg(all(target_os = "macos", feature = "gui"))]

use std::ffi::c_void;
use std::sync::Once;

use objc::declare::ClassDecl;
use objc::runtime::{BOOL, Class, NO, Object, Sel, YES};
use objc::{class, msg_send, sel, sel_impl};

use crate::audio::{ChannelRoute, DeviceCache, InputController, OutputController};
use crate::midi::{MidiChannel, MidiController};
use crate::presets::PresetController;
use crate::vlog;

/// Heap-allocated state the Objective-C class points at via ivar.
struct MenuState {
    input: InputController,
    output: OutputController,
    /// Preset library handle backing the Presets menu.
    presets: PresetController,
    /// Mic-toggle item - checkmark refreshed on Plugin-menu open.
    /// Null for instrument plugins (item not added).
    mic_item: *mut Object,
    /// Output mute-toggle item - checkmark refreshed on
    /// Plugin-menu open.
    output_item: *mut Object,
    /// Input device submenu - repopulated on open from cpal.
    /// Null for instrument plugins (submenu not added).
    input_device_menu: *mut Object,
    /// Output device submenu - repopulated on open from cpal.
    output_device_menu: *mut Object,
    /// Pointer to the action-target object itself, for re-targeting
    /// the device items repopulated each open.
    target: *mut Object,
    /// Background-refreshed device-name cache. Read on menu open
    /// (instant) instead of enumerating cpal synchronously on the
    /// GUI thread.
    device_cache: DeviceCache,
    /// MIDI device / channel control. `None`-as-feature: the MIDI
    /// submenus are only built for plugins that take note input.
    midi: MidiController,
    /// MIDI-input device submenu - repopulated on open. Null when the
    /// MIDI menu isn't built.
    midi_input_menu: *mut Object,
    /// The Presets menu - identified on open to refresh its children.
    presets_menu: *mut Object,
    /// Presets > Load submenu - re-enumerated on each open so saves
    /// appear without a relaunch.
    preset_load_menu: *mut Object,
    /// The "Save Preset" item - its title is rewritten on open to
    /// show the file Save will write (or `Save Preset...` when Save
    /// will open a dialog).
    preset_save_item: *mut Object,
}

/// Install the native menu bar.
///
/// `is_effect` controls whether mic-input and input-device items
/// appear - input-side controls are useless for instruments and
/// analyzers since the runner feeds them silence.
pub fn install(
    app_name: &str,
    is_effect: bool,
    channels: usize,
    input: &InputController,
    output: &OutputController,
    midi: &MidiController,
    presets: &PresetController,
) {
    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];

        // App menu - "About <App>" / "Hide <App>" / "Quit <App>".
        let app_menu_item = make_menu_item(app_name);
        let app_menu = make_menu(app_name);
        add_app_menu_items(app_menu, app_name);
        let _: () = msg_send![app_menu_item, setSubmenu: app_menu];

        // Settings menu (audio + MIDI) and its action target.
        let plugin_menu_item = make_menu_item("Settings");
        let plugin_menu = make_menu("Settings");
        let target = make_menu_target(input.clone(), output.clone(), midi.clone(), presets.clone());

        // Mic toggle (⌘I) - only meaningful for effects.
        let mic_item = if is_effect {
            let item = make_toggle_item("Mic Input", "i", sel!(toggleInputAction:), target);
            let _: () = msg_send![plugin_menu, addItem: item];
            item
        } else {
            std::ptr::null_mut()
        };

        // Output toggle (⌘O) - applies to every plugin category.
        let output_item = make_toggle_item("Audio Output", "o", sel!(toggleOutputAction:), target);
        let _: () = msg_send![plugin_menu, addItem: output_item];

        // Separator before device pickers.
        let sep: *mut Object = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![plugin_menu, addItem: sep];

        // Input device submenu - only useful for effects (instrument
        // runners don't read from the input ring). Empty at install;
        // repopulated on open.
        //
        // The parent item is built with `noopAction:` (see
        // `make_menu_item`). A submenu parent that carries an action
        // but no target gets auto-disabled by AppKit's menu validation
        // - it can't find anything in the responder chain that handles
        // `noopAction:`, so the item grays out even though it has a
        // populated submenu. (Top-level menu-bar items like "Audio"
        // dodge this because AppKit always enables those for
        // navigation.) Point the item at `target`, which implements
        // `noopAction:`, so validation passes and it stays clickable.
        let input_dev_menu = if is_effect {
            let input_dev_item = make_menu_item("Input Device");
            let _: () = msg_send![input_dev_item, setTarget: target];
            let menu = make_menu("Input Device");
            let _: () = msg_send![input_dev_item, setSubmenu: menu];
            let _: () = msg_send![plugin_menu, addItem: input_dev_item];
            menu
        } else {
            std::ptr::null_mut()
        };

        // Output device submenu - every plugin needs this. Same target
        // requirement as the input submenu above.
        let output_dev_item = make_menu_item("Output Device");
        let _: () = msg_send![output_dev_item, setTarget: target];
        let output_dev_menu = make_menu("Output Device");
        let _: () = msg_send![output_dev_item, setSubmenu: output_dev_menu];
        let _: () = msg_send![plugin_menu, addItem: output_dev_item];

        // Channel-routing submenus. Pointless on a mono device (nothing
        // to choose), so only shown when the device has >= 2 channels.
        // The channel count is fixed for the session, so these are
        // populated once here; the select action updates the checkmark.
        // Parent items need a target for the same anti-graying reason
        // as the device submenus above.
        if channels >= 2 {
            let sep2: *mut Object = msg_send![class!(NSMenuItem), separatorItem];
            let _: () = msg_send![plugin_menu, addItem: sep2];

            if is_effect {
                let in_ch_item = make_menu_item("Input Channels");
                let _: () = msg_send![in_ch_item, setTarget: target];
                let in_ch_menu = make_menu("Input Channels");
                populate_channel_menu(
                    in_ch_menu,
                    target,
                    channels,
                    input.channel_route(),
                    sel!(selectInputChannelsAction:),
                );
                let _: () = msg_send![in_ch_item, setSubmenu: in_ch_menu];
                let _: () = msg_send![plugin_menu, addItem: in_ch_item];
            }

            let out_ch_item = make_menu_item("Output Channels");
            let _: () = msg_send![out_ch_item, setTarget: target];
            let out_ch_menu = make_menu("Output Channels");
            populate_channel_menu(
                out_ch_menu,
                target,
                channels,
                output.channel_route(),
                sel!(selectOutputChannelsAction:),
            );
            let _: () = msg_send![out_ch_item, setSubmenu: out_ch_menu];
            let _: () = msg_send![plugin_menu, addItem: out_ch_item];
        }

        // MIDI section, appended into the same Settings menu: an
        // input-device picker + channel filter, fronted by a separator
        // dividing it from the audio controls above. Built for every
        // plugin (any can receive MIDI CC; instruments need note
        // input). The input submenu is repopulated on open; the channel
        // submenu is static and its checkmark is moved by the action.
        let midi_sep: *mut Object = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![plugin_menu, addItem: midi_sep];

        let midi_input_item = make_menu_item("MIDI Input");
        let _: () = msg_send![midi_input_item, setTarget: target];
        let midi_input_menu = make_menu("MIDI Input");
        let _: () = msg_send![midi_input_item, setSubmenu: midi_input_menu];
        let _: () = msg_send![plugin_menu, addItem: midi_input_item];

        let midi_chan_item = make_menu_item("MIDI Channel");
        let _: () = msg_send![midi_chan_item, setTarget: target];
        let midi_chan_menu = make_menu("MIDI Channel");
        populate_midi_channel_menu(midi_chan_menu, target, midi.channel());
        let _: () = msg_send![midi_chan_item, setSubmenu: midi_chan_menu];
        let _: () = msg_send![plugin_menu, addItem: midi_chan_item];

        // Stash pointers in MenuState so menu-open delegates can
        // address the right submenu.
        update_menu_state(
            target,
            mic_item,
            output_item,
            input_dev_menu,
            output_dev_menu,
            midi_input_menu,
            target,
        );

        // Wire menuWillOpen on the Settings menu (toggle checkmarks)
        // and both device submenus (repopulate + checkmark). Input
        // submenu may be null for instruments - only delegate if
        // we actually built it.
        let _: () = msg_send![plugin_menu, setDelegate: target];
        if !input_dev_menu.is_null() {
            let _: () = msg_send![input_dev_menu, setDelegate: target];
        }
        let _: () = msg_send![output_dev_menu, setDelegate: target];
        // The MIDI-input submenu repopulates its device list on open.
        let _: () = msg_send![midi_input_menu, setDelegate: target];

        let _: () = msg_send![plugin_menu_item, setSubmenu: plugin_menu];

        // Presets menu - Load submenu + Previous / Next / Save /
        // Save As. The Load list and the Save title are refreshed on
        // open (`menuWillOpen`); items act through the same target.
        let presets_menu_item = make_menu_item("Presets");
        let presets_menu = make_menu("Presets");
        // Delegate so `menuWillOpen:` fires for this menu.
        let _: () = msg_send![presets_menu, setDelegate: target];

        let load_item = make_menu_item("Load");
        let _: () = msg_send![load_item, setTarget: target];
        let load_menu = make_menu("Load");
        populate_preset_load_menu(load_menu, target, &presets.entries());
        let _: () = msg_send![load_item, setSubmenu: load_menu];
        let _: () = msg_send![presets_menu, addItem: load_item];

        let nav_sep: *mut Object = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![presets_menu, addItem: nav_sep];
        let prev_item = make_toggle_item("Previous Preset", "[", sel!(prevPresetAction:), target);
        let _: () = msg_send![presets_menu, addItem: prev_item];
        let next_item = make_toggle_item("Next Preset", "]", sel!(nextPresetAction:), target);
        let _: () = msg_send![presets_menu, addItem: next_item];

        let save_sep: *mut Object = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![presets_menu, addItem: save_sep];
        // No key equivalents: Cmd-S / Cmd-Shift-S are handled by the
        // window's own key handler; a menu item with the same
        // shortcut would shadow it. The title is set on open.
        let save_item = make_toggle_item("Save Preset", "", sel!(savePresetAction:), target);
        let _: () = msg_send![presets_menu, addItem: save_item];
        let save_as_item =
            make_toggle_item("Save Preset As...", "", sel!(saveAsPresetAction:), target);
        let _: () = msg_send![presets_menu, addItem: save_as_item];

        let _: () = msg_send![presets_menu_item, setSubmenu: presets_menu];
        set_preset_menu(target, presets_menu, load_menu, save_item);

        // Main menu - the one NSApp draws.
        let main_menu = make_menu("");
        let _: () = msg_send![main_menu, addItem: app_menu_item];
        let _: () = msg_send![main_menu, addItem: plugin_menu_item];
        let _: () = msg_send![main_menu, addItem: presets_menu_item];
        let _: () = msg_send![app, setMainMenu: main_menu];
    }
}

unsafe fn make_menu(title: &str) -> *mut Object {
    unsafe {
        let title = ns_string(title);
        let menu: *mut Object = msg_send![class!(NSMenu), alloc];
        let menu: *mut Object = msg_send![menu, initWithTitle: title];
        menu
    }
}

unsafe fn make_menu_item(title: &str) -> *mut Object {
    unsafe {
        let title = ns_string(title);
        let empty = ns_string("");
        let item: *mut Object = msg_send![class!(NSMenuItem), alloc];
        let item: *mut Object = msg_send![
            item,
            initWithTitle: title
            action: sel!(noopAction:)
            keyEquivalent: empty
        ];
        item
    }
}

unsafe fn make_toggle_item(
    title: &str,
    key_equiv: &str,
    action: Sel,
    target: *mut Object,
) -> *mut Object {
    unsafe {
        let title = ns_string(title);
        let key = ns_string(key_equiv);
        let item: *mut Object = msg_send![class!(NSMenuItem), alloc];
        let item: *mut Object = msg_send![
            item,
            initWithTitle: title
            action: action
            keyEquivalent: key
        ];
        let _: () = msg_send![item, setTarget: target];
        item
    }
}

/// Add the standard App-menu items. macOS does NOT auto-fill the
/// app name here - we have to spell out `Quit <App>` ourselves.
unsafe fn add_app_menu_items(menu: *mut Object, app_name: &str) {
    unsafe {
        let title = ns_string(&format!("Quit {app_name}"));
        let key = ns_string("q");
        let quit_item: *mut Object = msg_send![class!(NSMenuItem), alloc];
        let quit_item: *mut Object = msg_send![
            quit_item,
            initWithTitle: title
            action: sel!(terminate:)
            keyEquivalent: key
        ];
        let _: () = msg_send![menu, addItem: quit_item];
    }
}

/// Replace the contents of `menu` with a fresh device list. Items
/// fire `action` on `target`; the chosen item gets a checkmark
/// when its title matches `current`.
/// Fill the Presets > Load submenu, one item per entry titled with
/// its display label; `loadPresetAction:` dispatches by that title.
/// Empty libraries get a disabled placeholder.
unsafe fn populate_preset_load_menu(
    menu: *mut Object,
    target: *mut Object,
    entries: &[crate::presets::PresetMenuEntry],
) {
    let empty = unsafe { ns_string("") };
    if entries.is_empty() {
        let title = unsafe { ns_string("(no presets)") };
        let item: *mut Object = msg_send![class!(NSMenuItem), alloc];
        let item: *mut Object = msg_send![
            item,
            initWithTitle: title
            action: sel!(noopAction:)
            keyEquivalent: empty
        ];
        let _: () = msg_send![item, setEnabled: NO];
        let _: () = msg_send![menu, addItem: item];
        return;
    }
    for entry in entries {
        let title = unsafe { ns_string(&entry.label) };
        let item: *mut Object = msg_send![class!(NSMenuItem), alloc];
        let item: *mut Object = msg_send![
            item,
            initWithTitle: title
            action: sel!(loadPresetAction:)
            keyEquivalent: empty
        ];
        let _: () = msg_send![item, setTarget: target];
        let _: () = msg_send![menu, addItem: item];
    }
}

unsafe fn populate_device_menu(
    menu: *mut Object,
    target: *mut Object,
    devices: &[String],
    current: Option<&str>,
    action: Sel,
) {
    let _: () = msg_send![menu, removeAllItems];

    if devices.is_empty() {
        let title = unsafe { ns_string("(no devices)") };
        let empty = unsafe { ns_string("") };
        let item: *mut Object = msg_send![class!(NSMenuItem), alloc];
        let item: *mut Object = msg_send![
            item,
            initWithTitle: title
            action: sel!(noopAction:)
            keyEquivalent: empty
        ];
        let _: () = msg_send![item, setEnabled: NO];
        let _: () = msg_send![menu, addItem: item];
        return;
    }

    for name in devices {
        let title = unsafe { ns_string(name) };
        let empty = unsafe { ns_string("") };
        let item: *mut Object = msg_send![class!(NSMenuItem), alloc];
        let item: *mut Object = msg_send![
            item,
            initWithTitle: title
            action: action
            keyEquivalent: empty
        ];
        let _: () = msg_send![item, setTarget: target];
        let is_current = current.is_some_and(|c| c == name.as_str());
        let mark: BOOL = if is_current { YES } else { NO };
        let _: () = msg_send![item, setState: i64::from(mark)];
        let _: () = msg_send![menu, addItem: item];
    }
}

/// Fill a channel-routing submenu: a "direct" default, then one item
/// per stereo pair, then one per mono channel. Each item carries its
/// [`ChannelRoute`] encoded in the `NSMenuItem` `tag`; the checkmark
/// lands on the item matching `current`.
unsafe fn populate_channel_menu(
    menu: *mut Object,
    target: *mut Object,
    channels: usize,
    current: ChannelRoute,
    action: Sel,
) {
    unsafe {
        let _: () = msg_send![menu, removeAllItems];
        let current_tag = encoded_tag(current);

        add_tagged_item(
            menu,
            target,
            action,
            "All channels (direct)",
            encoded_tag(ChannelRoute::Direct),
            current_tag,
        );

        // Stereo pairs (1 & 2, 3 & 4, ...). Only emit a pair when both
        // of its channels exist.
        if channels >= 2 {
            let sep: *mut Object = msg_send![class!(NSMenuItem), separatorItem];
            let _: () = msg_send![menu, addItem: sep];
            let mut base = 0;
            while base + 1 < channels {
                let label = format!("Channels {} & {}", base + 1, base + 2);
                add_tagged_item(
                    menu,
                    target,
                    action,
                    &label,
                    encoded_tag(ChannelRoute::Stereo { base }),
                    current_tag,
                );
                base += 2;
            }
        }

        // Mono channels (1, 2, 3, ...).
        let sep: *mut Object = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: sep];
        for c in 0..channels {
            let label = format!("Channel {} (mono)", c + 1);
            add_tagged_item(
                menu,
                target,
                action,
                &label,
                encoded_tag(ChannelRoute::Mono { base: c }),
                current_tag,
            );
        }
    }
}

/// `ChannelRoute` packed into an `NSMenuItem` `tag` (`NSInteger` = i64).
fn encoded_tag(route: ChannelRoute) -> i64 {
    i64::try_from(route.encode()).unwrap_or(0)
}

/// Add one tagged, targeted menu item, checkmarked when its tag is the
/// active one.
unsafe fn add_tagged_item(
    menu: *mut Object,
    target: *mut Object,
    action: Sel,
    title: &str,
    tag: i64,
    current_tag: i64,
) {
    unsafe {
        let t = ns_string(title);
        let empty = ns_string("");
        let item: *mut Object = msg_send![class!(NSMenuItem), alloc];
        let item: *mut Object = msg_send![
            item,
            initWithTitle: t
            action: action
            keyEquivalent: empty
        ];
        let _: () = msg_send![item, setTarget: target];
        let _: () = msg_send![item, setTag: tag];
        let on: BOOL = if tag == current_tag { YES } else { NO };
        let _: () = msg_send![item, setState: i64::from(on)];
        let _: () = msg_send![menu, addItem: item];
    }
}

/// Move the checkmark in a channel submenu to the item whose tag is
/// `selected_tag`. Skips separators (whose default tag is 0 and would
/// otherwise collide with the "direct" item).
unsafe fn update_channel_checkmarks(menu: *mut Object, selected_tag: i64) {
    unsafe {
        if menu.is_null() {
            return;
        }
        let count: i64 = msg_send![menu, numberOfItems];
        for i in 0..count {
            let item: *mut Object = msg_send![menu, itemAtIndex: i];
            if item.is_null() {
                continue;
            }
            let is_sep: BOOL = msg_send![item, isSeparatorItem];
            if is_sep == YES {
                continue;
            }
            let tag: i64 = msg_send![item, tag];
            let on: BOOL = if tag == selected_tag { YES } else { NO };
            let _: () = msg_send![item, setState: i64::from(on)];
        }
    }
}

/// Tag for the "None" row in the MIDI-input submenu. Distinct from the
/// device rows (which use the default tag 0 and select by title).
const MIDI_INPUT_NONE_TAG: i64 = -1;

/// Rebuild the MIDI-input submenu: a "None" row (disconnect) followed
/// by one row per available port. The active device (or "None") is
/// checkmarked. Device rows select by title; "None" by its tag.
unsafe fn populate_midi_input_menu(
    menu: *mut Object,
    target: *mut Object,
    devices: &[String],
    current: Option<&str>,
) {
    unsafe {
        let _: () = msg_send![menu, removeAllItems];

        let none = ns_string("None");
        let empty = ns_string("");
        let item: *mut Object = msg_send![class!(NSMenuItem), alloc];
        let item: *mut Object = msg_send![
            item,
            initWithTitle: none
            action: sel!(selectMidiInputAction:)
            keyEquivalent: empty
        ];
        let _: () = msg_send![item, setTarget: target];
        let _: () = msg_send![item, setTag: MIDI_INPUT_NONE_TAG];
        let on: BOOL = if current.is_none() { YES } else { NO };
        let _: () = msg_send![item, setState: i64::from(on)];
        let _: () = msg_send![menu, addItem: item];

        if devices.is_empty() {
            return;
        }
        let sep: *mut Object = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: sep];
        for name in devices {
            let title = ns_string(name);
            let empty = ns_string("");
            let item: *mut Object = msg_send![class!(NSMenuItem), alloc];
            let item: *mut Object = msg_send![
                item,
                initWithTitle: title
                action: sel!(selectMidiInputAction:)
                keyEquivalent: empty
            ];
            let _: () = msg_send![item, setTarget: target];
            let on: BOOL = if current.is_some_and(|c| c == name.as_str()) {
                YES
            } else {
                NO
            };
            let _: () = msg_send![item, setState: i64::from(on)];
            let _: () = msg_send![menu, addItem: item];
        }
    }
}

/// Fill the MIDI-channel submenu: an "Omni" default then channels
/// 1-16, each tagged with its `MidiChannel::encode()`. Checkmark on
/// `current`.
unsafe fn populate_midi_channel_menu(menu: *mut Object, target: *mut Object, current: MidiChannel) {
    unsafe {
        let _: () = msg_send![menu, removeAllItems];
        let cur = i64::from(current.encode());
        let action = sel!(selectMidiChannelAction:);

        add_tagged_item(
            menu,
            target,
            action,
            "Omni (all channels)",
            i64::from(MidiChannel::Omni.encode()),
            cur,
        );
        let sep: *mut Object = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: sep];
        for n in 0..16u8 {
            let label = format!("Channel {}", n + 1);
            add_tagged_item(
                menu,
                target,
                action,
                &label,
                i64::from(MidiChannel::Channel(n).encode()),
                cur,
            );
        }
    }
}

unsafe fn ns_string(s: &str) -> *mut Object {
    let bytes = s.as_bytes();
    let cls = class!(NSString);
    let nsstr: *mut Object = msg_send![cls, alloc];
    let nsstr: *mut Object = msg_send![
        nsstr,
        initWithBytes: bytes.as_ptr().cast::<c_void>()
        length: bytes.len()
        encoding: 4_usize // NSUTF8StringEncoding
    ];
    nsstr
}

/// Read an `NSMenuItem`'s title back as a Rust String.
unsafe fn item_title(item: *mut Object) -> Option<String> {
    if item.is_null() {
        return None;
    }
    let nsstr: *mut Object = msg_send![item, title];
    if nsstr.is_null() {
        return None;
    }
    let cstr: *const std::os::raw::c_char = msg_send![nsstr, UTF8String];
    if cstr.is_null() {
        return None;
    }
    // SAFETY: `UTF8String` returns a NUL-terminated buffer owned by
    // the autoreleased NSString - valid for the duration of this call.
    Some(unsafe {
        std::ffi::CStr::from_ptr(cstr)
            .to_string_lossy()
            .into_owned()
    })
}

// ---------------------------------------------------------------------------
// Custom TruceMenuTarget class
// ---------------------------------------------------------------------------

static REGISTER_CLASS: Once = Once::new();

const STATE_IVAR: &str = "_truce_menu_state";

// `extern "C" fn` action callbacks are declared inside `call_once`
// so they live in scope where `decl.add_method` can take their
// address; hoisting them out loses access to the surrounding
// `add_method` registration pattern. Hence the function-level
// `items_after_statements` allow.
#[allow(clippy::items_after_statements, clippy::too_many_lines)]
fn ensure_class() -> &'static Class {
    REGISTER_CLASS.call_once(|| unsafe {
        let superclass = class!(NSObject);
        let mut decl = ClassDecl::new("TruceMenuTarget", superclass)
            .expect("TruceMenuTarget already registered (this should be unreachable - Once gate)");

        decl.add_ivar::<*mut c_void>(STATE_IVAR);

        // Mic toggle.
        extern "C" fn toggle_input_action(this: &Object, _: Sel, sender: *mut Object) {
            unsafe {
                let Some(state) = state_from(this) else {
                    return;
                };
                let want = !state.input.is_enabled();
                state.input.set_enabled(want);
                vlog!(
                    "mic: {} (request, via menu)",
                    if want { "ON" } else { "OFF" }
                );
                let new_state: BOOL = if want { YES } else { NO };
                let _: () = msg_send![sender, setState: i64::from(new_state)];
            }
        }
        decl.add_method(
            sel!(toggleInputAction:),
            toggle_input_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        // Output mute toggle.
        extern "C" fn toggle_output_action(this: &Object, _: Sel, sender: *mut Object) {
            unsafe {
                let Some(state) = state_from(this) else {
                    return;
                };
                let want = !state.output.is_enabled();
                state.output.set_enabled(want);
                vlog!(
                    "output: {} (request, via menu)",
                    if want { "ON" } else { "OFF" }
                );
                let new_state: BOOL = if want { YES } else { NO };
                let _: () = msg_send![sender, setState: i64::from(new_state)];
            }
        }
        decl.add_method(
            sel!(toggleOutputAction:),
            toggle_output_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        // Input device chosen.
        extern "C" fn select_input_device_action(this: &Object, _: Sel, sender: *mut Object) {
            unsafe {
                let Some(state) = state_from(this) else {
                    return;
                };
                if let Some(name) = item_title(sender) {
                    vlog!("input device: {name}");
                    state.input.set_device(Some(name));
                }
            }
        }
        decl.add_method(
            sel!(selectInputDeviceAction:),
            select_input_device_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        // Output device chosen.
        extern "C" fn select_output_device_action(this: &Object, _: Sel, sender: *mut Object) {
            unsafe {
                let Some(state) = state_from(this) else {
                    return;
                };
                if let Some(name) = item_title(sender) {
                    vlog!("output device: {name}");
                    state.output.set_device(Some(name));
                }
            }
        }
        decl.add_method(
            sel!(selectOutputDeviceAction:),
            select_output_device_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        // Input channel routing chosen.
        extern "C" fn select_input_channels_action(this: &Object, _: Sel, sender: *mut Object) {
            unsafe {
                let Some(state) = state_from(this) else {
                    return;
                };
                let tag: i64 = msg_send![sender, tag];
                let route = ChannelRoute::decode(usize::try_from(tag).unwrap_or(0));
                vlog!("input channels: {route:?}");
                state.input.set_channel_route(route);
                let menu: *mut Object = msg_send![sender, menu];
                update_channel_checkmarks(menu, tag);
            }
        }
        decl.add_method(
            sel!(selectInputChannelsAction:),
            select_input_channels_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        // Output channel routing chosen.
        extern "C" fn select_output_channels_action(this: &Object, _: Sel, sender: *mut Object) {
            unsafe {
                let Some(state) = state_from(this) else {
                    return;
                };
                let tag: i64 = msg_send![sender, tag];
                let route = ChannelRoute::decode(usize::try_from(tag).unwrap_or(0));
                vlog!("output channels: {route:?}");
                state.output.set_channel_route(route);
                let menu: *mut Object = msg_send![sender, menu];
                update_channel_checkmarks(menu, tag);
            }
        }
        decl.add_method(
            sel!(selectOutputChannelsAction:),
            select_output_channels_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        // MIDI input device chosen. The "None" row (tag 1) disconnects;
        // every other row carries a device name in its title.
        extern "C" fn select_midi_input_action(this: &Object, _: Sel, sender: *mut Object) {
            unsafe {
                let Some(state) = state_from(this) else {
                    return;
                };
                let tag: i64 = msg_send![sender, tag];
                if tag == MIDI_INPUT_NONE_TAG {
                    vlog!("midi input: none");
                    state.midi.set_device(None);
                } else if let Some(name) = item_title(sender) {
                    vlog!("midi input: {name}");
                    state.midi.set_device(Some(name));
                }
            }
        }
        decl.add_method(
            sel!(selectMidiInputAction:),
            select_midi_input_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        // MIDI channel filter chosen (tag = MidiChannel::encode()).
        extern "C" fn select_midi_channel_action(this: &Object, _: Sel, sender: *mut Object) {
            unsafe {
                let Some(state) = state_from(this) else {
                    return;
                };
                let tag: i64 = msg_send![sender, tag];
                let channel = MidiChannel::decode(u8::try_from(tag).unwrap_or(0xFF));
                vlog!("midi channel: {channel:?}");
                state.midi.set_channel(channel);
                let menu: *mut Object = msg_send![sender, menu];
                update_channel_checkmarks(menu, tag);
            }
        }
        decl.add_method(
            sel!(selectMidiChannelAction:),
            select_midi_channel_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        // -(void) menuWillOpen:(NSMenu *)menu - refresh state for
        // the about-to-open menu. Dispatch by pointer comparison so
        // we know whether to refresh the mic checkmark or
        // repopulate a device submenu.
        extern "C" fn menu_will_open(this: &Object, _: Sel, menu: *mut Object) {
            unsafe {
                let Some(state) = state_from(this) else {
                    return;
                };

                if !state.input_device_menu.is_null() && menu == state.input_device_menu {
                    let names = state.device_cache.inputs();
                    let current = state.input.current_name();
                    populate_device_menu(
                        state.input_device_menu,
                        state.target,
                        &names,
                        current.as_deref(),
                        sel!(selectInputDeviceAction:),
                    );
                    // Refresh off-thread so the next open reflects
                    // hot-plugged / removed devices without blocking
                    // this one.
                    state.device_cache.refresh_async();
                    return;
                }

                if menu == state.output_device_menu {
                    let names = state.device_cache.outputs();
                    let current = state.output.current_name();
                    populate_device_menu(
                        state.output_device_menu,
                        state.target,
                        &names,
                        current.as_deref(),
                        sel!(selectOutputDeviceAction:),
                    );
                    state.device_cache.refresh_async();
                    return;
                }

                if !state.midi_input_menu.is_null() && menu == state.midi_input_menu {
                    let names = crate::midi::list_midi_devices();
                    let current = state.midi.current_name();
                    populate_midi_input_menu(
                        state.midi_input_menu,
                        state.target,
                        &names,
                        current.as_deref(),
                    );
                    return;
                }

                // Presets menu: re-enumerate the library (saves this
                // session appear) and retitle Save to its target file.
                if !state.presets_menu.is_null() && menu == state.presets_menu {
                    if !state.preset_load_menu.is_null() {
                        let _: () = msg_send![state.preset_load_menu, removeAllItems];
                        populate_preset_load_menu(
                            state.preset_load_menu,
                            state.target,
                            &state.presets.entries(),
                        );
                    }
                    if !state.preset_save_item.is_null() {
                        let title = ns_string(&state.presets.save_menu_title());
                        let _: () = msg_send![state.preset_save_item, setTitle: title];
                    }
                    return;
                }

                // Settings menu (any other we delegate) - refresh the
                // toggle checkmarks.
                if !state.mic_item.is_null() {
                    let on = state.input.is_enabled();
                    let new_state: BOOL = if on { YES } else { NO };
                    let _: () = msg_send![state.mic_item, setState: i64::from(new_state)];
                }
                if !state.output_item.is_null() {
                    let on = state.output.is_enabled();
                    let new_state: BOOL = if on { YES } else { NO };
                    let _: () = msg_send![state.output_item, setState: i64::from(new_state)];
                }
            }
        }
        decl.add_method(
            sel!(menuWillOpen:),
            menu_will_open as extern "C" fn(&Object, Sel, *mut Object),
        );

        // Placeholder selector used by non-action items
        // (separators, disabled "(no devices)" rows).
        extern "C" fn noop_action(_: &Object, _: Sel, _sender: *mut Object) {}
        decl.add_method(
            sel!(noopAction:),
            noop_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        // Preset: a Load-submenu item chosen. Dispatch by the item's
        // title (its display label) so re-enumeration can't desync a
        // stale index.
        extern "C" fn load_preset_action(this: &Object, _: Sel, sender: *mut Object) {
            unsafe {
                let Some(state) = state_from(this) else {
                    return;
                };
                if let Some(label) = item_title(sender) {
                    state.presets.load_by_label(&label);
                }
            }
        }
        decl.add_method(
            sel!(loadPresetAction:),
            load_preset_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        // Preset: step to the next entry, wrapping.
        extern "C" fn next_preset_action(this: &Object, _: Sel, _sender: *mut Object) {
            unsafe {
                if let Some(state) = state_from(this) {
                    state.presets.step(1);
                }
            }
        }
        decl.add_method(
            sel!(nextPresetAction:),
            next_preset_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        // Preset: step to the previous entry, wrapping.
        extern "C" fn prev_preset_action(this: &Object, _: Sel, _sender: *mut Object) {
            unsafe {
                if let Some(state) = state_from(this) {
                    state.presets.step(-1);
                }
            }
        }
        decl.add_method(
            sel!(prevPresetAction:),
            prev_preset_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        // Preset: save the current state - overwrites the loaded
        // user preset, or routes to Save As for factory / none.
        extern "C" fn save_preset_action(this: &Object, _: Sel, _sender: *mut Object) {
            unsafe {
                if let Some(state) = state_from(this) {
                    state.presets.save();
                }
            }
        }
        decl.add_method(
            sel!(savePresetAction:),
            save_preset_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        // Preset: save under a chosen name / location.
        extern "C" fn save_as_preset_action(this: &Object, _: Sel, _sender: *mut Object) {
            unsafe {
                if let Some(state) = state_from(this) {
                    state.presets.save_as();
                }
            }
        }
        decl.add_method(
            sel!(saveAsPresetAction:),
            save_as_preset_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        decl.register();
    });
    Class::get("TruceMenuTarget").unwrap()
}

unsafe fn state_from<'a>(this: &Object) -> Option<&'a MenuState> {
    unsafe {
        let state_ptr: *mut c_void = *this.get_ivar(STATE_IVAR);
        if state_ptr.is_null() {
            None
        } else {
            Some(&*(state_ptr as *const MenuState))
        }
    }
}

unsafe fn make_menu_target(
    input: InputController,
    output: OutputController,
    midi: MidiController,
    presets: PresetController,
) -> *mut Object {
    let cls = ensure_class();
    let target: *mut Object = msg_send![cls, alloc];
    let target: *mut Object = msg_send![target, init];
    let state = Box::into_raw(Box::new(MenuState {
        input,
        output,
        presets,
        mic_item: std::ptr::null_mut(),
        output_item: std::ptr::null_mut(),
        input_device_menu: std::ptr::null_mut(),
        output_device_menu: std::ptr::null_mut(),
        target: std::ptr::null_mut(),
        device_cache: DeviceCache::new(),
        midi,
        midi_input_menu: std::ptr::null_mut(),
        presets_menu: std::ptr::null_mut(),
        preset_load_menu: std::ptr::null_mut(),
        preset_save_item: std::ptr::null_mut(),
    }));
    // SAFETY: `target` was just `alloc`+`init`'d above; it's a valid
    // TruceMenuTarget instance whose ivar layout we declared.
    unsafe { (*target).set_ivar::<*mut c_void>(STATE_IVAR, state.cast::<c_void>()) };
    target
}

#[allow(clippy::too_many_arguments)]
unsafe fn update_menu_state(
    target: *mut Object,
    mic_item: *mut Object,
    output_item: *mut Object,
    input_device_menu: *mut Object,
    output_device_menu: *mut Object,
    midi_input_menu: *mut Object,
    target_self: *mut Object,
) {
    unsafe {
        let state_ptr: *mut c_void = *(*target).get_ivar(STATE_IVAR);
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr.cast::<MenuState>();
        state.mic_item = mic_item;
        state.output_item = output_item;
        state.input_device_menu = input_device_menu;
        state.output_device_menu = output_device_menu;
        state.midi_input_menu = midi_input_menu;
        state.target = target_self;
    }
}

/// Record the Presets menu's pointers so `menuWillOpen:` can
/// re-enumerate the Load submenu and refresh the Save title.
unsafe fn set_preset_menu(
    target: *mut Object,
    presets_menu: *mut Object,
    preset_load_menu: *mut Object,
    preset_save_item: *mut Object,
) {
    unsafe {
        let state_ptr: *mut c_void = *(*target).get_ivar(STATE_IVAR);
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr.cast::<MenuState>();
        state.presets_menu = presets_menu;
        state.preset_load_menu = preset_load_menu;
        state.preset_save_item = preset_save_item;
    }
}
