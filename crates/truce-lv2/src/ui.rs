//! LV2 UI — supports `X11UI` on Linux, `CocoaUI` on macOS, and
//! `WindowsUI` on Windows.
//!
//! All three UI types share an identical LV2 C ABI: `instantiate`,
//! `port_event`, `cleanup`, plus the `ui:parent` feature. Only the
//! semantics of the parent handle differ — an `xcb_window_t` on X11,
//! an `NSView*` on Cocoa, an `HWND` on Win32. `parse_parent_feature`
//! returns the raw pointer and the `instantiate_ui` caller reinterprets
//! it per platform before handing it to the editor via `RawWindowHandle`.
//!
//! LV2 UIs do not share memory with the plugin. All communication goes
//! through two function pointers the host provides:
//!
//! - `write_function` (UI → host): "Set port N to V" — host forwards to plugin.
//! - `port_event` (host → UI): "Port N changed to V" — so the UI can update.
//!
//! We implement this by keeping a shadow `Params` instance that mirrors the
//! plugin's state from the UI side. The existing `Editor` trait expects a
//! `EditorContext` of closures over a live plugin; we satisfy it by giving
//! those closures access to the shadow params + write_function.
//!
//! # Scope
//!
//! Milestone 1 supports knob/slider manipulation end-to-end. Meters,
//! `get_state`/`set_state`, and `begin_edit`/`end_edit` gestures are no-ops.
//! Widget out-parameter currently returns the host-provided PARENT
//! (pragmatic for Ardour/Jalv which accept it; stricter X11UI / CocoaUI
//! hosts may want the actual child window / view — follow-up).

use std::ffi::{c_char, c_void, CStr, CString};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use truce_core::editor::{Editor, EditorContext, RawWindowHandle};
use truce_core::events::TransportInfo;
use truce_core::export::PluginExport;
use truce_core::TransportSlot;
use truce_params::Params;

use crate::atom::AtomSequenceReader;
use crate::types::LV2Feature;
use crate::urid::UridMap;

pub type Lv2UiHandle = *mut c_void;
pub type Lv2UiController = *mut c_void;

pub type Lv2UiWriteFn = unsafe extern "C" fn(
    controller: Lv2UiController,
    port_index: u32,
    buffer_size: u32,
    port_protocol: u32,
    buffer: *const c_void,
);

pub type Lv2UiInstantiateFn = unsafe extern "C" fn(
    descriptor: *const Lv2UiDescriptor,
    plugin_uri: *const c_char,
    bundle_path: *const c_char,
    write_function: Lv2UiWriteFn,
    controller: Lv2UiController,
    widget: *mut *mut c_void,
    features: *const *const LV2Feature,
) -> Lv2UiHandle;

pub type Lv2UiCleanupFn = unsafe extern "C" fn(handle: Lv2UiHandle);
pub type Lv2UiPortEventFn = unsafe extern "C" fn(
    handle: Lv2UiHandle,
    port_index: u32,
    buffer_size: u32,
    format: u32,
    buffer: *const c_void,
);
pub type Lv2UiExtensionDataFn = unsafe extern "C" fn(uri: *const c_char) -> *const c_void;

#[repr(C)]
pub struct Lv2UiDescriptor {
    pub uri: *const c_char,
    pub instantiate: Lv2UiInstantiateFn,
    pub cleanup: Lv2UiCleanupFn,
    pub port_event: Lv2UiPortEventFn,
    pub extension_data: Lv2UiExtensionDataFn,
}

unsafe impl Send for Lv2UiDescriptor {}
unsafe impl Sync for Lv2UiDescriptor {}

pub const LV2_UI__PARENT: &str = "http://lv2plug.in/ns/extensions/ui#parent";
pub const LV2_UI__RESIZE: &str = "http://lv2plug.in/ns/extensions/ui#resize";

/// Layout of the host-provided `ui:resize` feature. Data pointer in the
/// `LV2_Feature` is an `&LV2UI_Resize`. The UI calls `ui_resize` with
/// its desired width × height and the host resizes its container
/// accordingly.
#[repr(C)]
pub struct Lv2UiResize {
    pub handle: *mut c_void,
    pub ui_resize:
        Option<unsafe extern "C" fn(handle: *mut c_void, width: i32, height: i32) -> i32>,
}

// ---------------------------------------------------------------------------
// UI state
// ---------------------------------------------------------------------------

/// Owned UI instance. `Box::into_raw`'d and returned to the host as
/// `LV2UI_Handle`.
pub struct Lv2UiInstance<P: PluginExport> {
    /// Shadow plugin kept alive so the editor's internal refs stay valid.
    /// Not dropped until cleanup_ui().
    _plugin: Box<P>,
    /// Arc into `_plugin`'s params — used by EditorContext closures.
    params: Arc<P::Params>,
    /// Param metadata (id → port index, range for denormalization).
    param_slots: Vec<ParamSlot>,
    /// Meter metadata (id, port index, shared latest value). Cloned into
    /// the `get_meter` closure so editor widgets can read the current
    /// reading without a trip to the plugin.
    meter_slots: Arc<Vec<MeterSlot>>,
    /// The plugin's editor — drives rendering. The host's
    /// `write_function` + controller are captured by `EditorContext`'s
    /// closures and don't need to live on the struct itself.
    editor: Option<Box<dyn Editor>>,
    /// Set once open() has run so cleanup can be idempotent.
    opened: AtomicBool,
    /// Host-interned URIDs. Needed to recognize the notify-out atom
    /// event format and decode its `time:Position` object.
    urid_map: UridMap,
    /// Port index the host uses when delivering atom events to
    /// `port_event`. Pre-computed so the event callback does no lookups.
    notify_port_index: u32,
    /// URID for `atom:eventTransfer`. `port_event`'s `format` argument
    /// equals this value when the buffer is an LV2 atom.
    atom_event_transfer_urid: crate::urid::Urid,
    /// Shared transport state. Written from `port_event` (main thread in
    /// practice), read by the editor's `transport` closure.
    transport_slot: Arc<TransportSlot>,
    _phantom: PhantomData<P>,
}

struct ParamSlot {
    id: u32,
    port_index: u32,
    range: truce_params::ParamRange,
}

/// UI-side mirror of a DSP meter output. `value` holds the latest reading
/// the host forwarded via `port_event` (stored as `f32::to_bits` so the
/// value is lock-free readable from the editor's paint thread).
struct MeterSlot {
    id: u32,
    port_index: u32,
    value: AtomicU32,
}

/// # Safety
/// Called by the LV2 UI host at UI instantiation. See LV2 spec for contract.
pub unsafe fn instantiate_ui<P: PluginExport>(
    _descriptor: *const Lv2UiDescriptor,
    _plugin_uri: *const c_char,
    _bundle_path: *const c_char,
    write_function: Lv2UiWriteFn,
    controller: Lv2UiController,
    widget: *mut *mut c_void,
    features: *const *const LV2Feature,
) -> Lv2UiHandle {
    // Locate PARENT feature — on X11 the host passes the window id the UI
    // should embed in; on macOS it passes an `NSView*` from Cocoa. Both
    // arrive as `feature.data: *mut c_void` and we reinterpret per
    // platform.
    let parent_ptr = parse_parent_feature(features);
    let Some(parent_ptr) = parent_ptr else {
        return std::ptr::null_mut();
    };

    // Build a shadow plugin instance. It stays alive for the UI's lifetime
    // so editors that hold internal references to the plugin's params (e.g.
    // via Arc clones) remain valid.
    let mut plugin = Box::new(P::create());
    let params_arc = plugin.params_arc();
    let param_infos = plugin.params().param_infos();

    let layout = crate::derive_port_layout::<P>();
    let control_start = layout.control_start();

    let param_slots: Vec<ParamSlot> = param_infos
        .iter()
        .enumerate()
        .map(|(i, pi)| ParamSlot {
            id: pi.id,
            port_index: control_start + i as u32,
            range: pi.range.clone(),
        })
        .collect();

    // Mirror the DSP-side `#[meter]` declaration order onto the
    // corresponding output control-port range so `port_event` can map an
    // incoming port update back to the meter's declared ID.
    let meter_ids = plugin.params().meter_ids();
    let meter_start = layout.meter_start();
    let meter_slots: Arc<Vec<MeterSlot>> = Arc::new(
        meter_ids
            .iter()
            .enumerate()
            .map(|(i, &id)| MeterSlot {
                id,
                port_index: meter_start + i as u32,
                value: AtomicU32::new(0),
            })
            .collect(),
    );

    let Some(mut editor) = plugin.editor() else {
        return std::ptr::null_mut();
    };

    // Resolve host URIDs for atom-event decoding on the UI side.
    let urid_map = UridMap::from_features(features);
    let atom_event_transfer_urid = urid_map.intern("http://lv2plug.in/ns/ext/atom#eventTransfer");
    let transport_slot = TransportSlot::new();

    // Build EditorContext closures driven by write_function / shadow params.
    let ctx = build_editor_context::<P>(
        params_arc.clone(),
        &param_slots,
        meter_slots.clone(),
        write_function,
        controller,
        transport_slot.clone(),
    );

    // Record the editor's preferred size BEFORE `open()` — hosts that
    // pre-size their container based on the widget's initial bounds
    // (Reaper's LV2 runner, for one) need us to hand back a correctly-
    // sized parent before the first repaint.
    //
    // On Windows and X11 the host works in physical pixels, so we scale
    // logical-point `editor.size()` by the editor's DPI. On macOS the
    // native view coordinate system is logical points — no scaling.
    let (pref_w, pref_h) = editor.size();
    // LV2 hosts on X11 conventionally expect pixel sizes, but we have
    // no host-provided scale channel today; report logical points and
    // let the host resize accordingly. macOS CocoaUI handles Retina
    // backing automatically.

    #[cfg(target_os = "macos")]
    let handle = RawWindowHandle::AppKit(parent_ptr);
    #[cfg(target_os = "windows")]
    let handle = RawWindowHandle::Win32(parent_ptr);
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let handle = RawWindowHandle::X11(parent_ptr as u64);
    editor.open(handle, ctx);

    // Ask the host to match our preferred size via the `ui:resize`
    // extension (optional — not every host provides it, but Reaper
    // honors it, and without it the UI floats inside a default-sized
    // window with large empty margins).
    if let Some(resize) = parse_resize_feature(features) {
        if let Some(func) = resize.ui_resize {
            func(resize.handle, pref_w as i32, pref_h as i32);
        }
    }

    // On macOS we also resize the host-supplied parent NSView directly,
    // as a belt-and-braces backup for hosts that don't honor
    // `ui:resize`. Reaper on macOS reads the parent's frame after
    // `instantiate` returns.
    #[cfg(target_os = "macos")]
    resize_ns_view(parent_ptr, pref_w, pref_h);

    // `editor.open()` just added baseview's child NSView under the
    // host's parent. Install a `cursorUpdate:` handler on that child so
    // macOS shows an arrow cursor over our editor instead of inheriting
    // whatever the host set last (Reaper leaves a crosshair behind when
    // dragging from the FX chain).
    #[cfg(target_os = "macos")]
    install_child_cursor_update(parent_ptr);

    // Windows belt-and-braces: resize the host-supplied parent HWND to
    // match baseview's child client area. Reaper on Windows doesn't
    // honor `ui:resize` reliably at instantiate time; without this, the
    // parent stays at Reaper's default size and the child renders
    // inside a too-large frame that overflows the FX-slot chrome. We
    // deliberately do NOT resize baseview's child — it already rendered
    // itself at the correct physical extent, and a second `SetWindowPos`
    // makes wgpu stretch the previously-rendered surface.
    #[cfg(target_os = "windows")]
    fit_win32_parent_to_child(parent_ptr);

    // Set widget out-param. Strict X11UI / CocoaUI hosts want the child
    // window / view we created; pragmatic ones (Ardour, Jalv, Reaper)
    // accept the parent. See the module comment for follow-up.
    if !widget.is_null() {
        *widget = parent_ptr;
    }

    let ui = Box::new(Lv2UiInstance::<P> {
        _plugin: plugin,
        params: params_arc,
        param_slots,
        meter_slots,
        editor: Some(editor),
        opened: AtomicBool::new(true),
        urid_map,
        notify_port_index: layout.notify_out_port(),
        atom_event_transfer_urid,
        transport_slot,
        _phantom: PhantomData,
    });
    Box::into_raw(ui) as Lv2UiHandle
}

/// # Safety
/// `handle` must be a valid UI instance pointer previously returned from
/// `instantiate_ui`.
pub unsafe fn cleanup_ui<P: PluginExport>(handle: Lv2UiHandle) {
    if handle.is_null() {
        return;
    }
    let mut ui = Box::from_raw(handle as *mut Lv2UiInstance<P>);
    if ui.opened.swap(false, Ordering::AcqRel) {
        if let Some(mut ed) = ui.editor.take() {
            ed.close();
        }
    }
    drop(ui);
}

/// Port value update from host.
///
/// Handles two formats:
/// - `LV2_UI_FLOAT_PROTOCOL` (format = 0): buffer is an `f32` for a
///   control port. We update the shadow params so the UI reads the new
///   value.
/// - `atom:eventTransfer` (format = URID): buffer is an `LV2_Atom`. When
///   the port is the notify-out and the atom is `time:Position`, we
///   update the shared transport slot.
///
/// # Safety
/// All pointers must be valid for the caller-declared `buffer_size`.
pub unsafe fn port_event<P: PluginExport>(
    handle: Lv2UiHandle,
    port_index: u32,
    buffer_size: u32,
    format: u32,
    buffer: *const c_void,
) {
    if handle.is_null() || buffer.is_null() {
        return;
    }
    let ui = &*(handle as *const Lv2UiInstance<P>);

    // Control-port float update.
    if format == 0 {
        if buffer_size < core::mem::size_of::<f32>() as u32 {
            return;
        }
        let value = *(buffer as *const f32);
        if !value.is_finite() {
            return;
        }
        if let Some(slot) = ui.param_slots.iter().find(|s| s.port_index == port_index) {
            ui.params.set_plain(slot.id, value as f64);
            return;
        }
        // Meter output: shadow the latest reading so the editor's
        // `get_meter` closure can hand it back without touching the DSP.
        if let Some(meter) = ui.meter_slots.iter().find(|m| m.port_index == port_index) {
            meter.value.store(value.to_bits(), Ordering::Relaxed);
        }
        return;
    }

    // Atom event on the notify-out port — look for time:Position.
    if port_index == ui.notify_port_index
        && ui.atom_event_transfer_urid != 0
        && format == ui.atom_event_transfer_urid
    {
        decode_notify_atom::<P>(ui, buffer, buffer_size);
    }
}

/// Decode an atom delivered via `atom:eventTransfer` to the notify-out
/// port. We reuse `AtomSequenceReader::read_time_position` by wrapping
/// the single atom in a tiny synthetic sequence on the stack.
///
/// # Safety
/// `buffer` must point to at least `buffer_size` bytes of a valid
/// `LV2_Atom` (header + body).
unsafe fn decode_notify_atom<P: PluginExport>(
    ui: &Lv2UiInstance<P>,
    buffer: *const c_void,
    buffer_size: u32,
) {
    use crate::atom::{Atom, AtomSequence, AtomSequenceBody};

    let header_size = core::mem::size_of::<Atom>();
    if (buffer_size as usize) < header_size {
        return;
    }
    let atom_hdr = *(buffer as *const Atom);
    if atom_hdr.type_ != ui.urid_map.atom_object && atom_hdr.type_ != ui.urid_map.atom_blank {
        return;
    }
    let body_ptr = (buffer as *const u8).add(header_size);
    let body_size = atom_hdr.size as usize;
    if header_size + body_size > buffer_size as usize {
        return;
    }

    // Stack-allocate a one-event sequence pointing at the delivered atom.
    // The reader walks events, so wrap: AtomSequence { seq header, event
    // header, body... }. Cheaper than reconstructing atom parsing here.
    #[repr(C)]
    struct OneEvent {
        seq_header: AtomSequence,
        event_time: i64,
        event_body: Atom,
    }
    let mut scratch = vec![0u8; core::mem::size_of::<OneEvent>() + body_size + 8];
    let one = scratch.as_mut_ptr() as *mut OneEvent;
    (*one).seq_header.atom.type_ = ui.urid_map.atom_sequence;
    (*one).seq_header.atom.size = (core::mem::size_of::<AtomSequenceBody>()
        + core::mem::size_of::<i64>()
        + core::mem::size_of::<Atom>()
        + body_size) as u32;
    (*one).seq_header.body.unit = 0;
    (*one).seq_header.body.pad = 0;
    (*one).event_time = 0;
    (*one).event_body = atom_hdr;
    let ev_body_dest = scratch.as_mut_ptr().add(core::mem::size_of::<OneEvent>());
    core::ptr::copy_nonoverlapping(body_ptr, ev_body_dest, body_size);

    let mut info = TransportInfo::default();
    let reader = AtomSequenceReader::new(scratch.as_ptr() as *const AtomSequence, &ui.urid_map);
    if reader.apply_time_position(&mut info) {
        ui.transport_slot.write(&info);
    }
}

/// # Safety
/// `uri` must be null or a valid null-terminated C string.
pub unsafe fn ui_extension_data(_uri: *const c_char) -> *const c_void {
    // Nothing additional (Idle / Show / Resize extensions TBD).
    std::ptr::null()
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Locate the host-supplied `ui:parent` feature. The returned pointer is
/// semantically an `NSView*` under CocoaUI and an `xcb_window_t` under
/// X11UI; callers reinterpret it per platform.
unsafe fn parse_parent_feature(features: *const *const LV2Feature) -> Option<*mut c_void> {
    find_feature(features, LV2_UI__PARENT).map(|f| f.data)
}

/// Locate the host-supplied `ui:resize` feature. When present, the UI
/// may call `ui_resize(handle, w, h)` to ask the host to resize the
/// embedding container.
unsafe fn parse_resize_feature(features: *const *const LV2Feature) -> Option<&'static Lv2UiResize> {
    let feat = find_feature(features, LV2_UI__RESIZE)?;
    if feat.data.is_null() {
        return None;
    }
    Some(&*(feat.data as *const Lv2UiResize))
}

unsafe fn find_feature(
    features: *const *const LV2Feature,
    uri: &str,
) -> Option<&'static LV2Feature> {
    if features.is_null() {
        return None;
    }
    let target = CString::new(uri).ok()?;
    let mut i = 0usize;
    loop {
        let feat_ptr = *features.add(i);
        if feat_ptr.is_null() {
            return None;
        }
        let feat = &*feat_ptr;
        if !feat.uri.is_null() && CStr::from_ptr(feat.uri) == target.as_c_str() {
            return Some(feat);
        }
        i += 1;
    }
}

/// Resize the host-supplied parent HWND so its client area exactly
/// matches baseview's child HWND. Win32 analogue of `resize_ns_view`.
///
/// Reaper on Windows doesn't resize its LV2 UI container from
/// `ui:resize` at instantiate time, so Reaper's parent HWND stays at
/// its default extent and the child — which baseview already sized to
/// the editor's preferred dimensions — renders inside a too-large
/// frame that overflows the FX-slot chrome.
///
/// We intentionally *only* resize the parent, never the child: baseview
/// has already configured its wgpu/GL surface at the child's current
/// extent, and a `SetWindowPos` on the child after the fact makes the
/// rendered content stretch rather than re-layout.
#[cfg(target_os = "windows")]
unsafe fn fit_win32_parent_to_child(parent: *mut c_void) {
    if parent.is_null() {
        return;
    }

    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const GW_CHILD: u32 = 5;

    #[repr(C)]
    struct RECT {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    extern "system" {
        fn SetWindowPos(
            hwnd: *mut c_void,
            hwnd_insert_after: *mut c_void,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
        fn GetWindow(hwnd: *mut c_void, cmd: u32) -> *mut c_void;
        fn GetClientRect(hwnd: *mut c_void, rect: *mut RECT) -> i32;
    }

    let child = GetWindow(parent, GW_CHILD);
    if child.is_null() {
        return;
    }

    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if GetClientRect(child, &mut rect) == 0 {
        return;
    }
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 0 || h <= 0 {
        return;
    }

    let _ = SetWindowPos(
        parent,
        std::ptr::null_mut(),
        0,
        0,
        w,
        h,
        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

/// Set the frame of a Cocoa view to (width, height) preserving its
/// origin. Used as a fallback when the host doesn't honor `ui:resize`.
#[cfg(target_os = "macos")]
unsafe fn resize_ns_view(view: *mut c_void, width: u32, height: u32) {
    use objc::{class, msg_send, sel, sel_impl};
    if view.is_null() {
        return;
    }
    // Objective-C `setFrameSize:` takes an NSSize (two doubles on
    // 64-bit platforms). We avoid linking AppKit directly by using
    // `msg_send!` on a known responder.
    #[repr(C)]
    struct NSSize {
        width: f64,
        height: f64,
    }
    let size = NSSize {
        width: width as f64,
        height: height as f64,
    };
    let _: () = msg_send![view as *mut objc::runtime::Object, setFrameSize: size];
    // Tell AppKit the view's intrinsic content size has changed so any
    // surrounding layout is invalidated.
    let _: () = msg_send![
        view as *mut objc::runtime::Object,
        invalidateIntrinsicContentSize
    ];
    let _ = class!(NSView); // touch the class symbol so the linker keeps it
}

/// Patch baseview's child NSView so macOS resets the cursor to arrow
/// whenever the mouse enters our editor area.
///
/// baseview creates an NSView with a tracking area that has the
/// `NSTrackingCursorUpdate` option set, but it does not implement
/// `-[NSView cursorUpdate:]`. Without that handler the system falls
/// back to whatever cursor the containing window last set — in Reaper
/// that's often a crosshair from the track-list drag. We add a tiny
/// `cursorUpdate:` method to the child view's class at runtime that
/// pushes the arrow cursor.
///
/// Each baseview window gets a fresh `BaseviewNSView_<uuid>` class, so
/// re-registering the method on every UI instantiation is a no-op for
/// fresh classes and a silent fail on the unlikely duplicate.
#[cfg(target_os = "macos")]
unsafe fn install_child_cursor_update(parent: *mut c_void) {
    use objc::runtime::{class_addMethod, Class, Object, Sel};
    use objc::{class, msg_send, sel, sel_impl};

    if parent.is_null() {
        return;
    }

    extern "C" fn cursor_update(_this: &Object, _sel: Sel, _event: *mut Object) {
        unsafe {
            let cursor: *mut Object = msg_send![class!(NSCursor), arrowCursor];
            let _: () = msg_send![cursor, set];
        }
    }

    let subviews: *mut Object = msg_send![parent as *mut Object, subviews];
    if subviews.is_null() {
        return;
    }
    let count: usize = msg_send![subviews, count];
    // The editor's child view is the most recent subview — iterate all
    // baseview-owned subviews to be safe against hosts that wrap the
    // parent with their own helper views.
    for i in 0..count {
        let child: *mut Object = msg_send![subviews, objectAtIndex: i];
        if child.is_null() {
            continue;
        }
        let class_ptr: *mut Class = msg_send![child, class];
        let selector = sel!(cursorUpdate:);
        // `v@:@` → void (id self, SEL _cmd, id event).
        let type_encoding = c"v@:@".as_ptr();
        // objc's `class_addMethod` takes an untyped function pointer.
        // Transmute through an intermediate `extern "C" fn()` to keep
        // the ABI intact while satisfying the cast.
        type ImpFn = unsafe extern "C" fn();
        let imp: ImpFn =
            core::mem::transmute::<extern "C" fn(&Object, Sel, *mut Object), ImpFn>(cursor_update);
        class_addMethod(class_ptr, selector, imp, type_encoding);
    }
}

fn build_editor_context<P: PluginExport>(
    params: Arc<P::Params>,
    slots: &[ParamSlot],
    meter_slots: Arc<Vec<MeterSlot>>,
    write_function: Lv2UiWriteFn,
    controller: Lv2UiController,
    transport_slot: Arc<TransportSlot>,
) -> EditorContext {
    // Clone slot metadata into each closure — small vec, cheap.
    let slots_for_set: Vec<(u32, u32, truce_params::ParamRange)> = slots
        .iter()
        .map(|s| (s.id, s.port_index, s.range.clone()))
        .collect();
    let controller_raw = controller as usize;

    let params_get = params.clone();
    let params_get_plain = params.clone();
    let params_format = params.clone();
    let params_set = params.clone();
    let meter_slots_for_get = meter_slots.clone();

    // The write_function is a plain extern "C" fn — bitcast-safe to move
    // across closure boundaries. We keep controller as usize to sidestep
    // raw-pointer Send issues.
    let write_set = write_function;

    EditorContext {
        begin_edit: Arc::new(|_id: u32| {}),
        end_edit: Arc::new(|_id: u32| {}),
        request_resize: Arc::new(|_w: u32, _h: u32| false),
        set_param: Arc::new(move |id: u32, normalized: f64| {
            let Some((_, port_index, range)) = slots_for_set.iter().find(|(pid, _, _)| *pid == id)
            else {
                return;
            };
            let plain = range.denormalize(normalized) as f32;
            params_set.set_normalized(id, normalized);
            unsafe {
                let value = plain;
                write_set(
                    controller_raw as Lv2UiController,
                    *port_index,
                    core::mem::size_of::<f32>() as u32,
                    0, // LV2_UI_FLOAT_PROTOCOL = 0 (control ports)
                    &value as *const f32 as *const c_void,
                );
            }
        }),
        get_param: Arc::new(move |id: u32| params_get.get_normalized(id).unwrap_or(0.0)),
        get_param_plain: Arc::new(move |id: u32| params_get_plain.get_plain(id).unwrap_or(0.0)),
        format_param: Arc::new(move |id: u32| {
            let v = params_format.get_plain(id).unwrap_or(0.0);
            params_format.format_value(id, v).unwrap_or_default()
        }),
        get_meter: Arc::new(move |id: u32| {
            meter_slots_for_get
                .iter()
                .find(|m| m.id == id)
                .map(|m| f32::from_bits(m.value.load(Ordering::Relaxed)))
                .unwrap_or(0.0)
        }),
        get_state: Arc::new(Vec::new),
        set_state: Arc::new(|_bytes: Vec<u8>| {}),
        // The DSP broadcasts host transport as `time:Position` atoms on
        // the notify-out port. `port_event` decodes them and writes the
        // slot — this closure just reads the latest value.
        transport: Arc::new(move || transport_slot.read()),
    }
}

/// Build a static UI descriptor for this plugin type. Monomorphized per P.
pub fn ui_descriptor<P: PluginExport>(uri: &'static CStr) -> Lv2UiDescriptor {
    Lv2UiDescriptor {
        uri: uri.as_ptr(),
        instantiate: instantiate_ui_tramp::<P>,
        cleanup: cleanup_ui_tramp::<P>,
        port_event: port_event_tramp::<P>,
        extension_data: ui_extension_data_tramp,
    }
}

unsafe extern "C" fn instantiate_ui_tramp<P: PluginExport>(
    descriptor: *const Lv2UiDescriptor,
    plugin_uri: *const c_char,
    bundle_path: *const c_char,
    write_function: Lv2UiWriteFn,
    controller: Lv2UiController,
    widget: *mut *mut c_void,
    features: *const *const LV2Feature,
) -> Lv2UiHandle {
    instantiate_ui::<P>(
        descriptor,
        plugin_uri,
        bundle_path,
        write_function,
        controller,
        widget,
        features,
    )
}

unsafe extern "C" fn cleanup_ui_tramp<P: PluginExport>(handle: Lv2UiHandle) {
    cleanup_ui::<P>(handle);
}

unsafe extern "C" fn port_event_tramp<P: PluginExport>(
    handle: Lv2UiHandle,
    port_index: u32,
    buffer_size: u32,
    format: u32,
    buffer: *const c_void,
) {
    port_event::<P>(handle, port_index, buffer_size, format, buffer);
}

unsafe extern "C" fn ui_extension_data_tramp(uri: *const c_char) -> *const c_void {
    ui_extension_data(uri)
}
