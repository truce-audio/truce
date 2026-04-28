//! macOS native menu bar for the standalone host.
//!
//! Builds an `NSMenu` with two top-level items: an autopopulated
//! App menu (Quit / Hide / About) and a Plugin menu carrying the
//! mic-input toggle (`⌘I`, checkmark when on). Installed via
//! `NSApp.setMainMenu(...)`.
//!
//! Action wiring uses a custom `TruceMenuTarget` Objective-C
//! class declared at runtime. The class has one ivar — a raw
//! pointer to a `MenuState` heap-allocated by Rust — and one
//! selector (`toggleInputAction:`) that dereferences the pointer
//! and routes the click to `InputController::set_enabled`.
//!
//! Menu state (the checkmark) is refreshed on `menuWillOpen:` so
//! the menu always reflects the current `InputController.enabled`
//! AtomicBool, regardless of whether the user toggled via menu,
//! the `I` key, or `--input-enabled on` at launch.

#![cfg(all(target_os = "macos", feature = "gui"))]

use std::ffi::c_void;
use std::sync::Once;

use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel, BOOL, NO, YES};
use objc::{class, msg_send, sel, sel_impl};

use crate::audio::InputController;

/// Heap-allocated state the Objective-C class points at via ivar.
/// Holds the controller; future fields (additional menu items,
/// device-picker state) go here.
struct MenuState {
    input: InputController,
    /// Held weakly via raw pointer for `menuWillOpen:` to refresh
    /// the checkmark. The NSMenuItem lives as long as the menu
    /// itself, which lives as long as NSApp — i.e., the program.
    mic_item: *mut Object,
}

/// Install the native menu bar on the running `NSApplication`.
/// Must be called on the main thread, after `NSApp` has been
/// initialized (baseview does this when its window opens).
///
/// `app_name` is the user-visible app name shown in the menu bar
/// (e.g., the plugin's display name). macOS reads this from the
/// **first menu item's title** in the main menu — *not* from
/// CFBundleName, despite common assumptions. Leaving it empty
/// makes the OS fall back to the bundle directory name (which
/// would render as `Truce Gain.standalone` for a
/// `Truce Gain.standalone.app` bundle), so we set it explicitly.
pub fn install(app_name: &str, input: InputController) {
    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];

        // App menu (top-level). The title here is what shows up in
        // the menu bar. The submenu's items also use the app name
        // for "About <App>" / "Hide <App>" / "Quit <App>" —
        // standard macOS convention.
        let app_menu_item = make_menu_item(app_name);
        let app_menu = make_menu(app_name);
        add_app_menu_items(app_menu, app_name);
        let _: () = msg_send![app_menu_item, setSubmenu: app_menu];

        // Plugin menu (next to it) — holds host-controlled items.
        let plugin_menu_item = make_menu_item("Plugin");
        let plugin_menu = make_menu("Plugin");

        // Build the action target (custom NSObject) and the mic
        // toggle item.
        let target = make_menu_target(input.clone());
        let mic_item = make_toggle_item("Mic Input", "i", target);

        // Stash the mic_item pointer in MenuState so the delegate's
        // menuWillOpen: can refresh the checkmark on each open.
        update_menu_state_mic_item(target, mic_item);

        let _: () = msg_send![plugin_menu, addItem: mic_item];
        let _: () = msg_send![plugin_menu_item, setSubmenu: plugin_menu];

        // Wire the delegate so menuWillOpen: refreshes state.
        let _: () = msg_send![plugin_menu, setDelegate: target];

        // Main menu — the one NSApp draws.
        let main_menu = make_menu("");
        let _: () = msg_send![main_menu, addItem: app_menu_item];
        let _: () = msg_send![main_menu, addItem: plugin_menu_item];
        let _: () = msg_send![app, setMainMenu: main_menu];
    }
}

unsafe fn make_menu(title: &str) -> *mut Object {
    let title = ns_string(title);
    let menu: *mut Object = msg_send![class!(NSMenu), alloc];
    let menu: *mut Object = msg_send![menu, initWithTitle: title];
    menu
}

unsafe fn make_menu_item(title: &str) -> *mut Object {
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

unsafe fn make_toggle_item(title: &str, key_equiv: &str, target: *mut Object) -> *mut Object {
    let title = ns_string(title);
    let key = ns_string(key_equiv);
    let item: *mut Object = msg_send![class!(NSMenuItem), alloc];
    let item: *mut Object = msg_send![
        item,
        initWithTitle: title
        action: sel!(toggleInputAction:)
        keyEquivalent: key
    ];
    let _: () = msg_send![item, setTarget: target];
    item
}

/// Add the standard App-menu items. macOS does NOT auto-fill the
/// app name here — we have to spell out "Quit <App>" ourselves.
unsafe fn add_app_menu_items(menu: *mut Object, app_name: &str) {
    // "Quit <App>" — `terminate:` on NSApp is the conventional
    // selector, fired by Cmd+Q.
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

/// Create an NSString from a Rust `&str`. Returned object is
/// retained by the runtime; let it be autoreleased as usual.
unsafe fn ns_string(s: &str) -> *mut Object {
    let bytes = s.as_bytes();
    let cls = class!(NSString);
    let nsstr: *mut Object = msg_send![cls, alloc];
    let nsstr: *mut Object = msg_send![
        nsstr,
        initWithBytes: bytes.as_ptr() as *const c_void
        length: bytes.len()
        encoding: 4_usize // NSUTF8StringEncoding
    ];
    nsstr
}

// ---------------------------------------------------------------------------
// Custom TruceMenuTarget class
// ---------------------------------------------------------------------------

static REGISTER_CLASS: Once = Once::new();

const STATE_IVAR: &str = "_truce_menu_state";

fn ensure_class() -> &'static Class {
    REGISTER_CLASS.call_once(|| unsafe {
        let superclass = class!(NSObject);
        let mut decl = ClassDecl::new("TruceMenuTarget", superclass)
            .expect("TruceMenuTarget already registered (this should be unreachable — Once gate)");

        // Ivar: raw pointer to MenuState.
        decl.add_ivar::<*mut c_void>(STATE_IVAR);

        // -(void) toggleInputAction:(id)sender — the menu action.
        extern "C" fn toggle_input_action(this: &Object, _: Sel, _sender: *mut Object) {
            unsafe {
                let state_ptr: *mut c_void = *this.get_ivar(STATE_IVAR);
                if state_ptr.is_null() {
                    return;
                }
                let state = &*(state_ptr as *const MenuState);
                let want = !state.input.is_enabled();
                state.input.set_enabled(want);
                eprintln!(
                    "[truce-standalone] mic: {} (request, via menu)",
                    if want { "ON" } else { "OFF" }
                );
                // Optimistically update the checkmark immediately;
                // if the worker fails to open the stream, the
                // AtomicBool will stay false and the next
                // menuWillOpen will correct it.
                let new_state: BOOL = if want { YES } else { NO };
                let _: () = msg_send![_sender, setState: new_state as i64];
            }
        }
        decl.add_method(
            sel!(toggleInputAction:),
            toggle_input_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        // -(void) menuWillOpen:(NSMenu *)menu — sync the menu
        // item state from the controller every time the menu is
        // opened. Runs on the main thread.
        extern "C" fn menu_will_open(this: &Object, _: Sel, _menu: *mut Object) {
            unsafe {
                let state_ptr: *mut c_void = *this.get_ivar(STATE_IVAR);
                if state_ptr.is_null() {
                    return;
                }
                let state = &*(state_ptr as *const MenuState);
                let on = state.input.is_enabled();
                if !state.mic_item.is_null() {
                    let new_state: BOOL = if on { YES } else { NO };
                    let _: () = msg_send![state.mic_item, setState: new_state as i64];
                }
            }
        }
        decl.add_method(
            sel!(menuWillOpen:),
            menu_will_open as extern "C" fn(&Object, Sel, *mut Object),
        );

        // -(void) noopAction:(id)sender — placeholder action so
        // make_menu_item can give items a default selector. Items
        // that need a real action override it (mic toggle does).
        extern "C" fn noop_action(_: &Object, _: Sel, _sender: *mut Object) {}
        decl.add_method(
            sel!(noopAction:),
            noop_action as extern "C" fn(&Object, Sel, *mut Object),
        );

        decl.register();
    });
    Class::get("TruceMenuTarget").unwrap()
}

unsafe fn make_menu_target(input: InputController) -> *mut Object {
    let cls = ensure_class();
    let target: *mut Object = msg_send![cls, alloc];
    let target: *mut Object = msg_send![target, init];
    let state = Box::into_raw(Box::new(MenuState {
        input,
        mic_item: std::ptr::null_mut(),
    }));
    (*target).set_ivar::<*mut c_void>(STATE_IVAR, state as *mut c_void);
    target
}

unsafe fn update_menu_state_mic_item(target: *mut Object, mic_item: *mut Object) {
    let state_ptr: *mut c_void = *(*target).get_ivar(STATE_IVAR);
    if state_ptr.is_null() {
        return;
    }
    let state = &mut *(state_ptr as *mut MenuState);
    state.mic_item = mic_item;
}
