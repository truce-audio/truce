//! LV2 UI - supports `X11UI` on Linux, `CocoaUI` on macOS, and
//! `WindowsUI` on Windows.
//!
//! All three UI types share an identical LV2 C ABI: `instantiate`,
//! `port_event`, `cleanup`, plus the `ui:parent` feature. Only the
//! semantics of the parent handle differ - an `xcb_window_t` on X11,
//! an `NSView*` on Cocoa, an `HWND` on Win32. `parse_parent_feature`
//! returns the raw pointer and the `instantiate_ui` caller reinterprets
//! it per platform before handing it to the editor via `RawWindowHandle`.
//!
//! LV2 UIs do not share memory with the plugin. All communication goes
//! through two function pointers the host provides:
//!
//! - `write_function` (UI → host): "Set port N to V" - host forwards to plugin.
//! - `port_event` (host → UI): "Port N changed to V" - so the UI can update.
//!
//! We implement this by keeping a shadow `Params` instance that mirrors the
//! plugin's state from the UI side. The existing `Editor` trait expects a
//! `PluginContext` of closures over a live plugin; we satisfy it by giving
//! those closures access to the shadow params + `write_function`.
//!
//! # Scope
//!
//! Knob/slider manipulation works end-to-end.
//! `begin_edit`/`end_edit` gestures forward to the host's `ui:touch`
//! feature when present (Ardour, Reaper Linux); hosts without it (jalv)
//! collapse the gestures to no-ops. `get_state`/`set_state` are no-ops.
//! The widget out-parameter returns the host-supplied PARENT, which
//! Ardour/Jalv accept; stricter X11UI / `CocoaUI` hosts may want the
//! actual child window / view instead.

// LV2 atoms / sequences are 8-byte aligned by spec.
#![allow(clippy::cast_ptr_alignment)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use truce_core::Float;
use truce_core::TransportSlot;
use truce_core::cast::{len_u32, size_of_u32};
#[cfg(target_os = "macos")]
use truce_core::editor::fit_size;
use truce_core::editor::{
    ClosureBridge, Editor, PluginContext, RawWindowHandle, SendPtr, fit_logical_size,
};
use truce_core::events::TransportInfo;
use truce_core::export::PluginExport;
use truce_core::tasks::AnyTaskSpawner;
use truce_core::wrapper::log_missing_bus_layout;
use truce_params::Params;

use crate::atom::AtomSequenceReader;
use crate::types::LV2Feature;
use crate::urid::UridMap;

#[cfg(all(unix, not(target_os = "macos")))]
use x11_dl::xlib;

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
pub const LV2_UI__TOUCH: &str = "http://lv2plug.in/ns/extensions/ui#touch";

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

/// Layout of the host-provided `ui:touch` feature. Data pointer in the
/// `LV2_Feature` is an `&LV2UI_Touch`. The UI calls `touch` with
/// `grabbed = true` when the user starts dragging a control (begin
/// gesture) and `grabbed = false` when they release (end gesture).
/// Hosts that record automation use the gesture window to thin samples
/// and group the changes into a single undo step.
#[repr(C)]
pub struct Lv2UiTouch {
    pub handle: *mut c_void,
    pub touch: Option<unsafe extern "C" fn(handle: *mut c_void, port_index: u32, grabbed: bool)>,
}

// ---------------------------------------------------------------------------
// UI state
// ---------------------------------------------------------------------------

/// Owned UI instance. `Box::into_raw`'d and returned to the host as
/// `LV2UI_Handle`.
pub struct Lv2UiInstance<P: PluginExport> {
    /// Shadow plugin kept alive so the editor's internal refs stay valid.
    /// Not dropped until `cleanup_ui()`.
    _plugin: Box<P>,
    /// Arc into `_plugin`'s params - used by `PluginContext` closures.
    params: Arc<P::Params>,
    /// Param metadata (id → port index, range for denormalization).
    param_slots: Vec<ParamSlot>,
    /// Meter metadata (id, port index, shared latest value). Cloned into
    /// the `get_meter` closure so editor widgets can read the current
    /// reading without a trip to the plugin.
    meter_slots: Arc<Vec<MeterSlot>>,
    /// The plugin's editor - drives rendering. The host's
    /// `write_function` + controller are captured by `PluginContext`'s
    /// closures and don't need to live on the struct itself.
    editor: Option<Box<dyn Editor>>,
    /// Set once `open()` has run so cleanup can be idempotent.
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
    /// Reusable scratch buffer for the synthetic `AtomSequence` we
    /// build per notify-port atom. LV2 hosts can deliver `time:Position`
    /// updates 60-180×/sec, so this is reused per event rather than
    /// freshly allocated. `RefCell` because `port_event` only has
    /// `&self` (the LV2 host hands us a `LV2UI_Handle`, which we cast
    /// to `&Lv2UiInstance<P>`) - fine on the UI thread, which hosts
    /// are required to use single-threaded.
    notify_scratch: core::cell::RefCell<Vec<u8>>,
    /// Child `NSView`s registered for the aspect/max fit observer
    /// (`NSViewFrameDidChangeNotification`), stored as raw pointers so
    /// `cleanup_ui` can unregister them before the views are torn down.
    /// Empty unless the editor is resizable with constraints the Cocoa
    /// autoresize mask can't express.
    #[cfg(target_os = "macos")]
    fit_observer_children: Vec<usize>,
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
    unsafe {
        // Locate PARENT feature - on X11 the host passes the window id the UI
        // should embed in; on macOS it passes an `NSView*` from Cocoa. Both
        // arrive as `feature.data: *mut c_void` and we reinterpret per
        // platform.
        // Single-pass walk of the host's feature array. Resolves
        // ui:parent, ui:resize, and urid:map in one O(n) sweep instead
        // of three. Subsequent code reads from the parsed struct.
        let parsed = parse_features(features);
        let Some(parent_ptr) = parsed.parent else {
            return std::ptr::null_mut();
        };

        // Build a shadow plugin instance. It stays alive for the UI's lifetime
        // so editors that hold internal references to the plugin's params (e.g.
        // via Arc clones) remain valid.
        let plugin = Box::new(P::create());
        let params_arc = plugin.params_arc();
        let task_spawner = plugin.task_spawner();
        let param_infos = plugin.params().param_infos();

        let Some(layout) = crate::derive_port_layout::<P>(&plugin) else {
            log_missing_bus_layout::<P>("LV2 UI");
            return std::ptr::null_mut();
        };
        let control_start = layout.control_start();

        let param_slots: Vec<ParamSlot> = param_infos
            .iter()
            .enumerate()
            .map(|(i, pi)| ParamSlot {
                id: pi.id,
                port_index: control_start + len_u32(i),
                range: pi.range,
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
                    port_index: meter_start + len_u32(i),
                    value: AtomicU32::new(0),
                })
                .collect(),
        );

        let Some(mut editor) = plugin.editor_builder()(params_arc.clone()) else {
            return std::ptr::null_mut();
        };

        // Resolve host URIDs for atom-event decoding on the UI side.
        // The host map handle/fn pair was extracted in `parse_features`
        // above; only the intern step happens here.
        let urid_map = UridMap::from_host(parsed.urid_map_handle, parsed.urid_map_fn);
        let atom_event_transfer_urid =
            urid_map.intern("http://lv2plug.in/ns/ext/atom#eventTransfer");
        let transport_slot = TransportSlot::new();

        // Read the host's `ui:scaleFactor` (HiDPI display scale) from the
        // options feature and hand it to the editor before `open()`, so it
        // configures its backing surface at the right physical resolution
        // rather than defaulting to 1.0. Hosts that don't expose the option
        // (or run at 1x) leave the editor at its default scale.
        let host_scale = read_ui_scale_factor(parsed.options, &urid_map);
        if let Some(scale) = host_scale {
            editor.set_scale_factor(scale);
        }

        // Build PluginContext closures driven by write_function / shadow params.
        let ctx = build_editor_context::<P>(
            params_arc.clone(),
            &param_slots,
            meter_slots.clone(),
            write_function,
            controller,
            transport_slot.clone(),
            parsed.touch,
            parsed.resize,
            task_spawner,
        );

        // Record the editor's preferred size BEFORE `open()` - hosts that
        // pre-size their container based on the widget's initial bounds
        // (Reaper's LV2 runner, for one) need us to hand back a correctly-
        // sized parent before the first repaint.
        //
        // `editor.size()` is in logical points. `ui:resize` on X11 / Windows
        // wants *physical* pixels - REAPER sets the container's physical size
        // to the value verbatim (it does not multiply by `ui:scaleFactor`), so
        // we scale by `host_scale` below. macOS Cocoa is logical (Retina via
        // the backing scale), so `resize_ns_view` uses the logical value here.
        let (pref_w, pref_h) = editor.size();

        #[cfg(target_os = "macos")]
        let handle = RawWindowHandle::AppKit(parent_ptr);
        #[cfg(target_os = "windows")]
        let handle = RawWindowHandle::Win32(parent_ptr);
        #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
        let handle = RawWindowHandle::X11(parent_ptr as u64);
        editor.open(handle, ctx);

        // Push our natural size to the host through `ui:resize` so the
        // outer plug-in pane opens at the editor's `editor.size()`
        // rather than at whatever the host's container defaults to.
        // Applies to both fixed-size and resizable editors:
        // - Fixed: host pane sizes to natural, the editor renders 1:1.
        // - Resizable: host pane *starts* at natural; user-drag of
        //   the host pane comes back through `ui_resize_dispatch`
        //   (`ui_resize_iface`) which calls `editor.set_size`. The
        //   host RPC replaces the old "let the autoresize cascade
        //   pull the editor up to host pane size" strategy, which
        //   on hosts that opened a large default pane (REAPER's LV2
        //   runner) made the editor land at `max_size` on open with
        //   the top of the layout clipped off the visible area.
        if let Some(resize) = parsed.resize
            && let Some(func) = resize.ui_resize
        {
            // macOS: logical points. Windows: physical pixels (the host
            // sizes the container to the value verbatim, matching the
            // `logical * scale` child baseview opened). Linux/REAPER (X11):
            // REAPER interprets `ui:resize` as physical then *divides by
            // the scale* to size its 1x pane (observed: 554 -> 277 pane;
            // and even with no call it made a 270 pane from a 540 child).
            // So to land the pane at `logical * scale` physical - matching
            // baseview's child - pre-multiply by `scale^2`.
            #[cfg(target_os = "macos")]
            let (rw, rh) = (pref_w, pref_h);
            #[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let (rw, rh) = {
                let s = host_scale.unwrap_or(1.0);
                (
                    (f64::from(pref_w) * s).round() as u32,
                    (f64::from(pref_h) * s).round() as u32,
                )
            };
            #[cfg(target_os = "linux")]
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let (rw, rh) = {
                let s = host_scale.unwrap_or(1.0);
                (
                    (f64::from(pref_w) * s * s).round() as u32,
                    (f64::from(pref_h) * s * s).round() as u32,
                )
            };
            // LV2 ui:resize takes int32_t; editor dimensions in u32
            // are bounded by display size, well below i32::MAX.
            #[allow(clippy::cast_possible_wrap)]
            let (w, h) = (rw as i32, rh as i32);
            func(resize.handle, w, h);
        }

        #[cfg(target_os = "macos")]
        resize_ns_view(parent_ptr, pref_w, pref_h);

        // baseview attached its child at frame origin `(0, 0)`. NSView
        // is unflipped by default, so `(0, 0)` is the parent's
        // bottom-left, which renders the editor anchored to the bottom
        // of the host's plugin window. Reposition the child so its
        // top edge sits at the parent's top edge, and pick an
        // autoresize mask that matches whether the editor opts into
        // host-driven resize:
        //
        // - fixed-size editors get `MinYMargin | MaxXMargin` so the
        //   child stays pinned at the parent's top-left as the host
        //   resizes around it (and never gets stretched).
        // - resizable editors get `WidthSizable | HeightSizable`
        //   (plus a `(0, 0)` origin so the child fills the parent
        //   from its bottom-left in unflipped coords) so the child
        //   grows in lock-step with the host's parent NSView. The
        //   editor's `Resized` event fires off the resulting frame
        //   change, which is what drives wgpu surface reconfigure.
        //   REAPER's LV2 runner on macOS doesn't call back through
        //   `ui_resize_dispatch` when the user drags the FX window,
        //   so the autoresize cascade is what carries user-driven
        //   resize for now. The new `ui:resize(natural)` push above
        //   ensures the *initial* pane size is the editor's natural,
        //   so the cascade only kicks in once the user actually
        //   resizes - the GUI Zoo no longer opens stretched to the
        //   host's default pane.
        #[cfg(target_os = "macos")]
        let fit_observer_children = anchor_child_for_resize(parent_ptr, editor.as_ref());

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
        // deliberately do NOT resize baseview's child - it already rendered
        // itself at the correct physical extent, and a second `SetWindowPos`
        // makes wgpu stretch the previously-rendered surface.
        #[cfg(target_os = "windows")]
        fit_win32_parent_to_child(parent_ptr);

        // X11: paint the host's parent window black (so any area our
        // editor child doesn't cover - a resizable editor smaller than
        // the host's pane, or regions exposed when the FX window is
        // resized - reads as black, not uninitialised server memory),
        // and for a non-resizable editor pin the parent + child to the
        // editor's natural *physical* size. baseview opens the child at
        // `logical * scale`, so the pin must match that (pinning to the
        // logical value would halve a HiDPI editor). Resizable editors
        // keep host-driven size.
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let (px, py) = {
                let s = host_scale.unwrap_or(1.0);
                (
                    (f64::from(pref_w) * s).round() as u32,
                    (f64::from(pref_h) * s).round() as u32,
                )
            };
            prepare_x11_parent_window(parent_ptr, px, py, !editor.can_resize());
        }

        // Set widget out-param. Strict X11UI / CocoaUI hosts want the
        // child window / view we created; pragmatic ones (Ardour,
        // Jalv, Reaper) accept the parent.
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
            notify_scratch: core::cell::RefCell::new(Vec::new()),
            #[cfg(target_os = "macos")]
            fit_observer_children,
            _phantom: PhantomData,
        });
        Box::into_raw(ui) as Lv2UiHandle
    }
}

/// # Safety
/// `handle` must be a valid UI instance pointer previously returned from
/// `instantiate_ui`.
pub unsafe fn cleanup_ui<P: PluginExport>(handle: Lv2UiHandle) {
    unsafe {
        if handle.is_null() {
            return;
        }
        let mut ui = Box::from_raw(handle.cast::<Lv2UiInstance<P>>());
        if ui.opened.swap(false, Ordering::AcqRel) {
            // Unregister the fit observer before `close()` deallocates the
            // child views: a stale `NSViewFrameDidChangeNotification`
            // registration would send `frameChanged:` to freed memory.
            #[cfg(target_os = "macos")]
            remove_fit_observers(&ui.fit_observer_children);
            if let Some(mut ed) = ui.editor.take() {
                ed.close();
            }
        }
        drop(ui);
    }
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
    unsafe {
        if handle.is_null() || buffer.is_null() {
            return;
        }
        let ui = &*(handle as *const Lv2UiInstance<P>);

        // Control-port float update.
        if format == 0 {
            if buffer_size < size_of_u32::<f32>() {
                return;
            }
            let value = *buffer.cast::<f32>();
            if !value.is_finite() {
                return;
            }
            if let Some(slot) = ui.param_slots.iter().find(|s| s.port_index == port_index) {
                ui.params.set_plain(slot.id, f64::from(value));
                return;
            }
            // Meter output: shadow the latest reading so the editor's
            // `get_meter` closure can hand it back without touching the DSP.
            if let Some(meter) = ui.meter_slots.iter().find(|m| m.port_index == port_index) {
                meter.value.store(value.to_bits(), Ordering::Relaxed);
            }
            return;
        }

        // Atom event on the notify-out port - look for time:Position.
        if port_index == ui.notify_port_index
            && ui.atom_event_transfer_urid != 0
            && format == ui.atom_event_transfer_urid
        {
            decode_notify_atom::<P>(ui, buffer, buffer_size);
        }
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
    unsafe {
        use crate::atom::{Atom, AtomSequence, AtomSequenceBody};

        // Synthetic single-event sequence laid out on the per-instance
        // scratch. Reuse keeps `time:Position` (60–180×/sec) off the
        // UI thread's allocator.
        #[repr(C)]
        struct OneEvent {
            seq_header: AtomSequence,
            event_time: i64,
            event_body: Atom,
        }

        let header_size = core::mem::size_of::<Atom>();
        if (buffer_size as usize) < header_size {
            return;
        }
        let atom_hdr = *buffer.cast::<Atom>();
        if atom_hdr.type_ != ui.urid_map.atom_object && atom_hdr.type_ != ui.urid_map.atom_blank {
            return;
        }
        let body_ptr = buffer.cast::<u8>().add(header_size);
        let body_size = atom_hdr.size as usize;
        if header_size + body_size > buffer_size as usize {
            return;
        }

        let needed = core::mem::size_of::<OneEvent>() + body_size + 8;
        // `try_borrow_mut` instead of `borrow_mut` so a re-entrant
        // `port_event` (which the LV2 spec forbids - UI thread only -
        // but a buggy host could still trigger via a queued
        // synthetic event) drops the call rather than panicking.
        // A panic here would unwind through the FFI boundary into
        // the host's UI loop, which is worse than a dropped event.
        let Ok(mut scratch) = ui.notify_scratch.try_borrow_mut() else {
            return;
        };
        if scratch.len() < needed {
            scratch.resize(needed, 0);
        }
        let one = scratch.as_mut_ptr().cast::<OneEvent>();
        (*one).seq_header.atom.type_ = ui.urid_map.atom_sequence;
        (*one).seq_header.atom.size = len_u32(
            core::mem::size_of::<AtomSequenceBody>()
                + core::mem::size_of::<i64>()
                + core::mem::size_of::<Atom>()
                + body_size,
        );
        (*one).seq_header.body.unit = 0;
        (*one).seq_header.body.pad = 0;
        (*one).event_time = 0;
        (*one).event_body = atom_hdr;
        let ev_body_dest = scratch.as_mut_ptr().add(core::mem::size_of::<OneEvent>());
        core::ptr::copy_nonoverlapping(body_ptr, ev_body_dest, body_size);

        let mut info = TransportInfo::default();
        let reader = AtomSequenceReader::new(scratch.as_ptr().cast::<AtomSequence>(), &ui.urid_map);
        if reader.apply_time_position(&mut info) {
            ui.transport_slot.write(&info);
        }
    }
}

/// # Safety
/// `uri` must be null or a valid null-terminated C string.
pub unsafe fn ui_extension_data<P: PluginExport>(uri: *const c_char) -> *const c_void {
    if uri.is_null() {
        return std::ptr::null();
    }
    let cstr = unsafe { CStr::from_ptr(uri) };
    if cstr.to_bytes() == LV2_UI__RESIZE.as_bytes() {
        // Host → UI direction of `ui:resize`: the host calls the
        // returned `Lv2UiResize::ui_resize` with the new container
        // size whenever its plugin frame is resized. The handle
        // it passes is the `Lv2UiHandle` we returned from
        // `instantiate_ui`, which is a `*mut Lv2UiInstance<P>`.
        return ui_resize_iface::<P>();
    }
    std::ptr::null()
}

/// Per-`P` static `Lv2UiResize` interface returned from
/// `ui_extension_data`. Rust forbids generic statics, so we lazily
/// leak one heap-allocated table per concrete `P` and cache the
/// resulting pointer in a process-wide `TypeId` map. The map is
/// touched at most once per plugin type per process; subsequent
/// `extension_data` lookups read the cached pointer.
fn ui_resize_iface<P: PluginExport>() -> *const c_void {
    use std::any::TypeId;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<TypeId, usize>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().expect("ui_resize cache poisoned");
    let entry = map.entry(TypeId::of::<P>()).or_insert_with(|| {
        let iface = Box::leak(Box::new(Lv2UiResize {
            handle: std::ptr::null_mut(),
            ui_resize: Some(ui_resize_dispatch::<P>),
        }));
        std::ptr::from_ref(iface) as usize
    });
    *entry as *const c_void
}

/// Host → UI resize callback. The host passes the `LV2UI_Handle` it
/// got from `instantiate_ui` (a `*mut Lv2UiInstance<P>`). We forward
/// the new size to the editor after clamping it to the editor's
/// declared min / max / aspect-ratio constraints.
///
/// Returns `0` on success per the LV2 spec; non-zero on error.
unsafe extern "C" fn ui_resize_dispatch<P: PluginExport>(
    handle: *mut c_void,
    width: i32,
    height: i32,
) -> i32 {
    if handle.is_null() || width <= 0 || height <= 0 {
        return 1;
    }
    unsafe {
        let inst = &mut *handle.cast::<Lv2UiInstance<P>>();
        let Some(editor) = inst.editor.as_mut() else {
            return 1;
        };
        #[allow(clippy::cast_sign_loss)]
        let (req_w, req_h) = (width as u32, height as u32);
        let (cw, ch) = fit_logical_size(req_w, req_h, editor.as_ref());
        editor.set_size(cw, ch);
    }
    0
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Layout of the host's `urid:map` feature record - semantically
/// identical to the private struct in `urid.rs` but expressed inline
/// here so `parse_features` can read it without exposing the urid
/// module's internal type.
#[repr(C)]
struct UridMapFeature {
    handle: *mut c_void,
    map: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> crate::urid::Urid>,
}

/// Layout of a single `LV2_Options_Option` from the `options:options`
/// feature. The feature's `data` points at a `[LV2OptionsOption]` array
/// terminated by a zeroed entry (`key == 0`). We only read it for
/// `ui:scaleFactor`, but the fields must match the spec ABI exactly.
#[repr(C)]
struct LV2OptionsOption {
    /// `LV2_Options_Context` - blank/resource/instance/port.
    context: u32,
    /// Subject the option applies to (unused for `ui:scaleFactor`).
    subject: u32,
    /// Interned URID of the option key.
    key: crate::urid::Urid,
    /// Value size in bytes.
    size: u32,
    /// Interned URID of the value's atom type.
    type_: crate::urid::Urid,
    /// Pointer to the value (`size` bytes of `type_`).
    value: *const c_void,
}

/// One-pass parse of the host's null-terminated feature array.
///
/// Replaces three separate walks (`ui:parent`, `ui:resize`,
/// `urid:map`) with a single sweep. Returns the resolved values in
/// one struct so callers stop juggling three independent helpers,
/// and so an early `ui:parent` miss can short-circuit before any
/// further work.
struct ParsedFeatures {
    /// `ui:parent` - `NSView*` (Cocoa) or `xcb_window_t` (X11). `None`
    /// when the host doesn't supply one (no UI to embed; bail out).
    parent: Option<*mut c_void>,
    /// `ui:resize` - optional. `None` when the host doesn't expose it
    /// (Reaper Linux does, Carla doesn't, etc.).
    resize: Option<&'static Lv2UiResize>,
    /// `ui:touch` - optional. `None` when the host doesn't expose it
    /// (Reaper Linux + Ardour do, jalv doesn't). When present, the
    /// editor's `begin_edit` / `end_edit` closures forward to
    /// `touch(handle, port_index, grabbed)` so hosts can group
    /// automation samples into a single gesture.
    touch: Option<&'static Lv2UiTouch>,
    /// Host URID:map handle + map function, or `(null, None)` if the
    /// feature is absent. Threaded into `UridMap::from_host` so the
    /// intern step doesn't re-walk the array.
    urid_map_handle: *mut c_void,
    urid_map_fn: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> crate::urid::Urid>,
    /// Head of the host's `LV2_Options_Option` array (null-terminated by a
    /// zeroed entry), or null when the host doesn't expose `options:options`.
    /// Read after the URID map is resolved so we can match option keys.
    options: *const LV2OptionsOption,
}

unsafe fn parse_features(features: *const *const LV2Feature) -> ParsedFeatures {
    let mut out = ParsedFeatures {
        parent: None,
        resize: None,
        touch: None,
        urid_map_handle: std::ptr::null_mut(),
        urid_map_fn: None,
        options: std::ptr::null(),
    };
    unsafe {
        if features.is_null() {
            return out;
        }
        let parent_uri = CString::new(LV2_UI__PARENT).unwrap();
        let resize_uri = CString::new(LV2_UI__RESIZE).unwrap();
        let touch_uri = CString::new(LV2_UI__TOUCH).unwrap();
        let map_uri = CString::new(crate::types::LV2_URID__MAP).unwrap();
        let options_uri = CString::new(crate::types::LV2_OPTIONS__OPTIONS).unwrap();

        let mut i = 0usize;
        loop {
            let feat_ptr = *features.add(i);
            if feat_ptr.is_null() {
                break;
            }
            let feat = &*feat_ptr;
            if !feat.uri.is_null() {
                let feat_uri = CStr::from_ptr(feat.uri);
                if out.parent.is_none() && feat_uri == parent_uri.as_c_str() {
                    out.parent = Some(feat.data);
                } else if out.resize.is_none()
                    && feat_uri == resize_uri.as_c_str()
                    && !feat.data.is_null()
                {
                    out.resize = Some(&*(feat.data as *const Lv2UiResize));
                } else if out.touch.is_none()
                    && feat_uri == touch_uri.as_c_str()
                    && !feat.data.is_null()
                {
                    out.touch = Some(&*(feat.data as *const Lv2UiTouch));
                } else if out.urid_map_fn.is_none() && feat_uri == map_uri.as_c_str() {
                    let map_feat = feat.data as *const UridMapFeature;
                    if !map_feat.is_null() {
                        out.urid_map_handle = (*map_feat).handle;
                        out.urid_map_fn = (*map_feat).map;
                    }
                } else if out.options.is_null() && feat_uri == options_uri.as_c_str() {
                    out.options = feat.data as *const LV2OptionsOption;
                }
            }
            i += 1;
        }
    }
    out
}

/// Read the host's `ui:scaleFactor` (an `atom:Float`) from the options
/// array, or `None` when the option is absent / malformed / non-positive.
///
/// # Safety
/// `options` must be null or point at a spec-conformant, zero-terminated
/// `LV2_Options_Option` array whose `value` pointers are valid for their
/// declared `size`.
unsafe fn read_ui_scale_factor(
    options: *const LV2OptionsOption,
    urid_map: &UridMap,
) -> Option<f64> {
    if options.is_null() {
        return None;
    }
    let scale_key = urid_map.intern(crate::types::LV2_UI__SCALE_FACTOR);
    // `intern` returns 0 only if the host map failed; nothing to match then.
    if scale_key == 0 {
        return None;
    }
    unsafe {
        let mut opt = options;
        // The array is terminated by a zeroed option (`key == 0`).
        while (*opt).key != 0 {
            let o = &*opt;
            if o.key == scale_key
                && o.type_ == urid_map.atom_float
                && o.size as usize >= std::mem::size_of::<f32>()
                && !o.value.is_null()
            {
                let v = f64::from(*(o.value.cast::<f32>()));
                if v.is_finite() && v > 0.0 {
                    return Some(v);
                }
            }
            opt = opt.add(1);
        }
    }
    None
}

/// Resize the host-supplied parent HWND so its client area exactly
/// matches baseview's child HWND. Win32 analogue of `resize_ns_view`.
///
/// Reaper on Windows doesn't resize its LV2 UI container from
/// `ui:resize` at instantiate time, so Reaper's parent HWND stays at
/// its default extent and the child - which baseview already sized to
/// the editor's preferred dimensions - renders inside a too-large
/// frame that overflows the FX-slot chrome.
///
/// We intentionally *only* resize the parent, never the child: baseview
/// has already configured its wgpu/GL surface at the child's current
/// extent, and a `SetWindowPos` on the child after the fact makes the
/// rendered content stretch rather than re-layout.
// Win32 constants and FFI declarations live alongside the only
// caller; hoisting them out would split the early-`return` guard
// from the API names it talks about. Hence the function-level
// `items_after_statements` allow.
#[cfg(target_os = "windows")]
#[allow(clippy::items_after_statements)]
unsafe fn fit_win32_parent_to_child(parent: *mut c_void) {
    unsafe {
        if parent.is_null() {
            return;
        }

        const SWP_NOMOVE: u32 = 0x0002;
        const SWP_NOZORDER: u32 = 0x0004;
        const SWP_NOACTIVATE: u32 = 0x0010;
        const GW_CHILD: u32 = 5;

        // Windows API names are conventionally all-caps (RECT, HWND, etc.).
        // Renaming would lose that mapping for a Windows reader.
        #[allow(clippy::upper_case_acronyms)]
        #[repr(C)]
        struct RECT {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }

        unsafe extern "system" {
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
        if GetClientRect(child, &raw mut rect) == 0 {
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
}

/// Prepare the host's parent window for the embedded editor on X11.
///
/// Two fix-ups for hosts that drive the embedded window directly and
/// ignore `ui:resize` (REAPER):
///
/// 1. **Black background.** Give the parent an opaque-black background
///    pixel so any region the editor child doesn't cover - a resizable
///    editor smaller than the host's pane, or areas newly exposed when
///    the user resizes the FX window - reads as black instead of
///    uninitialised server memory (the "unpainted outer window" the
///    standalone fixes the same way). It's a persistent attribute, so
///    the server keeps filling exposed regions on every later resize.
/// 2. **Fit (non-resizable only).** When `fit`, pin the parent *and*
///    baseview's child to the editor's natural `(w, h)` so the host
///    can't stretch a fixed editor's surface (the blurry GUI). REAPER
///    drives the child directly (unlike Windows), so the pin is the
///    only lever. Resizable editors keep host-driven size.
///
/// The LV2 UI is handed only an `xcb_window_t` (no display handle), so
/// we open our own short-lived X connection - cross-client window ops
/// are valid X11. No-op if libX11 can't be dlopened or the window id
/// looks invalid.
#[cfg(all(unix, not(target_os = "macos")))]
fn prepare_x11_parent_window(parent: *mut c_void, w: u32, h: u32, fit: bool) {
    if parent.is_null() {
        return;
    }
    let Ok(lib) = xlib::Xlib::open() else {
        return;
    };
    // The host's `xcb_window_t` is numerically the Xlib `Window` id.
    let parent_id = parent as usize as xlib::Window;

    // SAFETY: we open and exclusively own this display connection for
    // the duration of the call and close it before returning. Operating
    // on window ids owned by another X client is well-defined; X
    // serialises requests across connections.
    unsafe {
        let display = (lib.XOpenDisplay)(std::ptr::null());
        if display.is_null() {
            return;
        }
        // Opaque black: alpha 0xFF over RGB 0. The top byte is ignored
        // on a 24-bit visual and makes a 32-bit ARGB visual opaque, so
        // it reads as black on either. `XClearWindow` repaints now; the
        // attribute makes the server fill future exposed regions too.
        (lib.XSetWindowBackground)(display, parent_id, 0xFF00_0000);
        (lib.XClearWindow)(display, parent_id);

        if fit && w > 0 && h > 0 {
            let child = x11_first_child(&lib, display, parent_id);
            (lib.XResizeWindow)(display, parent_id, w, h);
            // baseview's editor window is the parent's (only) child;
            // REAPER may have already stretched it, so pin it back too.
            // Resizing it re-fires baseview's `Resized`, which
            // reconfigures the wgpu surface to natural and re-renders.
            if let Some(c) = child {
                (lib.XResizeWindow)(display, c, w, h);
            }
        }
        (lib.XFlush)(display);
        (lib.XCloseDisplay)(display);
    }
}

/// First (only) child of `parent`, or `None`. Caller holds a live
/// display on its own thread.
#[cfg(all(unix, not(target_os = "macos")))]
unsafe fn x11_first_child(
    lib: &xlib::Xlib,
    display: *mut xlib::Display,
    parent: xlib::Window,
) -> Option<xlib::Window> {
    let mut root: xlib::Window = 0;
    let mut parent_ret: xlib::Window = 0;
    let mut children: *mut xlib::Window = std::ptr::null_mut();
    let mut n: std::os::raw::c_uint = 0;
    unsafe {
        if (lib.XQueryTree)(
            display,
            parent,
            &raw mut root,
            &raw mut parent_ret,
            &raw mut children,
            &raw mut n,
        ) == 0
        {
            return None;
        }
        let first = if n > 0 && !children.is_null() {
            Some(*children)
        } else {
            None
        };
        if !children.is_null() {
            (lib.XFree)(children.cast());
        }
        first
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct NSPoint {
    x: f64,
    y: f64,
}
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct NSSize {
    width: f64,
    height: f64,
}
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct NSRect {
    origin: NSPoint,
    size: NSSize,
}

/// Per-child-view fit constraints `(min, max, aspect)`, keyed by the
/// child `NSView` pointer. `frame_changed` reads these on every
/// `NSViewFrameDidChangeNotification` so the resize callback stays a
/// plain Cocoa selector (no trait object to thread through `objc`).
/// Entries are inserted by `install_fit_observer` and removed by
/// `remove_fit_observers` at cleanup.
#[cfg(target_os = "macos")]
type FitConstraints = ((u32, u32), (u32, u32), Option<(u32, u32)>);

#[cfg(target_os = "macos")]
fn fit_constraints() -> &'static std::sync::Mutex<std::collections::HashMap<usize, FitConstraints>>
{
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static MAP: OnceLock<Mutex<HashMap<usize, FitConstraints>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reposition every direct subview of `parent` so it tracks the host's
/// parent `NSView`. `NSView` is unflipped by default, so an attached
/// child at frame origin `(0, 0)` lands at the parent's bottom-left in
/// Cocoa coordinates; without this fixup the editor renders anchored to
/// the bottom of the host's plugin window.
///
/// - non-resizable: pin the child top-left via
///   `NSViewMinYMargin | NSViewMaxXMargin` so its frame stays fixed.
/// - resizable, no aspect/max constraint: fill the parent and tag
///   `NSViewWidthSizable | NSViewHeightSizable` so the child grows in
///   lock-step with the host pane. REAPER's LV2 runner on macOS doesn't
///   fire `ui_resize_dispatch`, so this autoresize cascade is what
///   carries user-driven resize.
/// - resizable with an aspect ratio, a min, or a finite max: the
///   autoresize mask can't express those, so pin the child
///   (`NSViewNotSizable`) and drive its frame from a frame-change
///   observer that re-fits to the largest on-ratio, within-bounds
///   rectangle on every host-pane resize. Returns the observed child
///   pointers so `cleanup_ui` can unregister them.
///
/// X11 and Win32 use top-left origins natively and have their own
/// resize paths (`fit_win32_parent_to_child` on Windows, the
/// `LV2UI_Resize` extension on X11).
#[cfg(target_os = "macos")]
unsafe fn anchor_child_for_resize(parent: *mut c_void, editor: &dyn Editor) -> Vec<usize> {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};

    // Cocoa autoresizing-mask bit flags (`NSAutoresizingMaskOptions`).
    const NSVIEW_NOT_SIZABLE: u64 = 0;
    const NSVIEW_WIDTH_SIZABLE: u64 = 2;
    const NSVIEW_MAX_X_MARGIN: u64 = 4;
    const NSVIEW_MIN_Y_MARGIN: u64 = 8;
    const NSVIEW_HEIGHT_SIZABLE: u64 = 16;

    let mut observed = Vec::new();
    if parent.is_null() {
        return observed;
    }

    let resizable = editor.can_resize();
    // The autoresize mask can only stretch a child to fill its parent;
    // it can't hold an aspect ratio or clamp to a min/max. Use the fit
    // observer whenever the editor declares any of those.
    let needs_fit = resizable
        && (editor.aspect_ratio().is_some()
            || editor.min_size() != (1, 1)
            || editor.max_size() != (u32::MAX, u32::MAX));

    let parent_obj = parent.cast::<Object>();
    let parent_frame: NSRect = msg_send![parent_obj, frame];
    let subviews: *mut Object = msg_send![parent_obj, subviews];
    if subviews.is_null() {
        return observed;
    }
    let count: usize = msg_send![subviews, count];
    for i in 0..count {
        let child: *mut Object = msg_send![subviews, objectAtIndex: i];
        if child.is_null() {
            continue;
        }
        if needs_fit {
            unsafe { install_fit_observer(parent_obj, child, editor) };
            let _: () = msg_send![child, setAutoresizingMask: NSVIEW_NOT_SIZABLE];
            unsafe { fit_view_into_superview(child) };
            observed.push(child as usize);
        } else if resizable {
            // Fill the parent. Origin `(0, 0)` is the bottom-left in
            // unflipped coords - it doesn't matter visually because
            // the child immediately covers the entire parent.
            let new_frame = NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: parent_frame.size,
            };
            let _: () = msg_send![child, setFrame: new_frame];
            let _: () =
                msg_send![child, setAutoresizingMask: NSVIEW_WIDTH_SIZABLE | NSVIEW_HEIGHT_SIZABLE];
        } else {
            let child_frame: NSRect = msg_send![child, frame];
            let new_origin = NSPoint {
                x: child_frame.origin.x,
                y: parent_frame.size.height - child_frame.size.height,
            };
            let _: () = msg_send![child, setFrameOrigin: new_origin];
            let _: () =
                msg_send![child, setAutoresizingMask: NSVIEW_MIN_Y_MARGIN | NSVIEW_MAX_X_MARGIN];
        }
    }
    observed
}

/// Register `child` to re-fit itself whenever `parent`'s frame changes.
/// Records the editor's constraints, adds a `frameChanged:` selector to
/// the child's (per-window unique) class, and subscribes the child to
/// the parent's `NSViewFrameDidChangeNotification`.
#[cfg(target_os = "macos")]
unsafe fn install_fit_observer(
    parent: *mut objc::runtime::Object,
    child: *mut objc::runtime::Object,
    editor: &dyn Editor,
) {
    use objc::runtime::{Class, Object, Sel, class_addMethod};
    use objc::{class, msg_send, sel, sel_impl};

    extern "C" fn frame_changed(this: &Object, _sel: Sel, _note: *mut Object) {
        unsafe { fit_view_into_superview(core::ptr::from_ref(this).cast_mut()) };
    }
    type ImpFn = unsafe extern "C" fn();

    fit_constraints()
        .lock()
        .expect("fit constraints poisoned")
        .insert(
            child as usize,
            (editor.min_size(), editor.max_size(), editor.aspect_ratio()),
        );

    let class_ptr: *mut Class = msg_send![child, class];
    // `v@:@` → void (id self, SEL _cmd, id notification).
    let type_encoding = c"v@:@".as_ptr();
    // SAFETY: `frame_changed` has the canonical Cocoa `IMP` ABI
    // (self, _cmd, sender). A fresh `BaseviewNSView_<uuid>` class per
    // window means this adds the method rather than colliding.
    let imp: ImpFn = unsafe {
        core::mem::transmute::<extern "C" fn(&Object, Sel, *mut Object), ImpFn>(frame_changed)
    };
    unsafe { class_addMethod(class_ptr, sel!(frameChanged:), imp, type_encoding) };

    let _: () = msg_send![parent, setPostsFrameChangedNotifications: true];
    let center: *mut Object = msg_send![class!(NSNotificationCenter), defaultCenter];
    let name = unsafe { ns_string("NSViewFrameDidChangeNotification") };
    let _: () = msg_send![center, addObserver: child selector: sel!(frameChanged:) name: name object: parent];
}

/// Resize `child` to the largest on-ratio, within-bounds rectangle that
/// fits its superview, anchored top-left in unflipped coords. No-op if
/// the child has no recorded constraints or no superview.
#[cfg(target_os = "macos")]
unsafe fn fit_view_into_superview(child: *mut objc::runtime::Object) {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};

    let Some((min, max, aspect)) = fit_constraints()
        .lock()
        .expect("fit constraints poisoned")
        .get(&(child as usize))
        .copied()
    else {
        return;
    };
    let superview: *mut Object = msg_send![child, superview];
    if superview.is_null() {
        return;
    }
    let sframe: NSRect = msg_send![superview, frame];
    let (fw, fh) = fit_size(
        pt_to_u32(sframe.size.width),
        pt_to_u32(sframe.size.height),
        min,
        max,
        aspect,
    );
    let new_frame = NSRect {
        origin: NSPoint {
            x: 0.0,
            y: sframe.size.height - f64::from(fh),
        },
        size: NSSize {
            width: f64::from(fw),
            height: f64::from(fh),
        },
    };
    let _: () = msg_send![child, setFrame: new_frame];
}

/// Unregister fit observers and drop their constraint entries. Called
/// from `cleanup_ui` before the child views are torn down so the
/// notification center holds no dangling reference.
#[cfg(target_os = "macos")]
unsafe fn remove_fit_observers(children: &[usize]) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    if children.is_empty() {
        return;
    }
    let center: *mut Object = msg_send![class!(NSNotificationCenter), defaultCenter];
    let mut map = fit_constraints().lock().expect("fit constraints poisoned");
    for &child in children {
        let obj = child as *mut Object;
        let _: () = msg_send![center, removeObserver: obj];
        map.remove(&child);
    }
}

/// Build an autoreleased `NSString` from a Rust `&str` so callers can
/// pass a notification name without linking the `AppKit` symbol.
#[cfg(target_os = "macos")]
unsafe fn ns_string(s: &str) -> *mut objc::runtime::Object {
    use objc::{class, msg_send, sel, sel_impl};
    let c = CString::new(s).expect("notification name has no interior nul");
    msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()]
}

/// Round a Cocoa point dimension to `u32`, clamping non-finite or
/// negative values to `0` (`fit_size` floors each axis at its min).
#[cfg(target_os = "macos")]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn pt_to_u32(v: f64) -> u32 {
    if !v.is_finite() || v <= 0.0 {
        0
    } else if v >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        v.round() as u32
    }
}

/// Set the frame of a Cocoa view to (width, height) preserving its
/// origin. Used as a fallback when the host doesn't honor `ui:resize`.
#[cfg(target_os = "macos")]
unsafe fn resize_ns_view(view: *mut c_void, width: u32, height: u32) {
    use objc::{class, msg_send, sel, sel_impl};
    // Objective-C `setFrameSize:` takes an NSSize (two doubles on
    // 64-bit platforms). We avoid linking AppKit directly by using
    // `msg_send!` on a known responder.
    #[repr(C)]
    struct NSSize {
        width: f64,
        height: f64,
    }
    if view.is_null() {
        return;
    }
    let size = NSSize {
        width: f64::from(width),
        height: f64::from(height),
    };
    let _: () = msg_send![view.cast::<objc::runtime::Object>(), setFrameSize: size];
    // Tell AppKit the view's intrinsic content size has changed so any
    // surrounding layout is invalidated.
    let _: () = msg_send![
        view.cast::<objc::runtime::Object>(),
        invalidateIntrinsicContentSize
    ];
    let _ = class!(NSView); // touch the class symbol so the linker keeps it
}

/// Patch baseview's child `NSView` so macOS resets the cursor to arrow
/// whenever the mouse enters our editor area.
///
/// baseview creates an `NSView` with a tracking area that has the
/// `NSTrackingCursorUpdate` option set, but it does not implement
/// `-[NSView cursorUpdate:]`. Without that handler the system falls
/// back to whatever cursor the containing window last set - in Reaper
/// that's often a crosshair from the track-list drag. We add a tiny
/// `cursorUpdate:` method to the child view's class at runtime that
/// pushes the arrow cursor.
///
/// Each baseview window gets a fresh `BaseviewNSView_<uuid>` class, so
/// re-registering the method on every UI instantiation is a no-op for
/// fresh classes and a silent fail on the unlikely duplicate.
#[cfg(target_os = "macos")]
unsafe fn install_child_cursor_update(parent: *mut c_void) {
    use objc::runtime::{Class, Object, Sel, class_addMethod};
    use objc::{class, msg_send, sel, sel_impl};

    extern "C" fn cursor_update(_this: &Object, _sel: Sel, _event: *mut Object) {
        unsafe {
            let cursor: *mut Object = msg_send![class!(NSCursor), arrowCursor];
            let _: () = msg_send![cursor, set];
        }
    }

    // objc's `class_addMethod` takes an untyped function pointer.
    // Transmute through an intermediate `extern "C" fn()` to keep
    // the ABI intact while satisfying the cast.
    type ImpFn = unsafe extern "C" fn();

    if parent.is_null() {
        return;
    }

    let subviews: *mut Object = msg_send![parent.cast::<Object>(), subviews];
    if subviews.is_null() {
        return;
    }
    let count: usize = msg_send![subviews, count];
    // The editor's child view is the most recent subview - iterate all
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
        // SAFETY: `cursor_update` has the canonical Cocoa `IMP` ABI
        // (self, _cmd, sender) and `class_addMethod` is documented
        // to accept any function with that calling convention.
        let imp: ImpFn = unsafe {
            core::mem::transmute::<extern "C" fn(&Object, Sel, *mut Object), ImpFn>(cursor_update)
        };
        unsafe { class_addMethod(class_ptr, selector, imp, type_encoding) };
    }
}

// `params` and `meter_slots` get cloned into per-callback closures
// below - owned-arg avoids forcing the caller to lend them across
// the closure-building scope.
#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
fn build_editor_context<P: PluginExport>(
    params: Arc<P::Params>,
    slots: &[ParamSlot],
    meter_slots: Arc<Vec<MeterSlot>>,
    write_function: Lv2UiWriteFn,
    controller: Lv2UiController,
    transport_slot: Arc<TransportSlot>,
    touch: Option<&'static Lv2UiTouch>,
    resize: Option<&'static Lv2UiResize>,
    tasks: Option<AnyTaskSpawner>,
) -> PluginContext {
    // Clone slot metadata into each closure - small vec, cheap.
    let slots_for_set: Vec<(u32, u32, truce_params::ParamRange)> = slots
        .iter()
        .map(|s| (s.id, s.port_index, s.range))
        .collect();
    // Begin/end_edit only need (id → port_index); a thinner clone keeps
    // the gesture closures from holding the full ParamRange.
    let slots_for_begin: Vec<(u32, u32)> = slots.iter().map(|s| (s.id, s.port_index)).collect();
    let slots_for_end = slots_for_begin.clone();
    // `Lv2UiController = *mut c_void`. `SendPtr` is the workspace's
    // canonical Send/Sync wrapper for raw pointers held across
    // closures (CLAP / VST3 / AU / AAX use it for `host_for_callback`,
    // `aax_ctx`, etc.). Going through `usize` worked but obscured the
    // intent that the value is a pointer; `SendPtr` makes the transit
    // explicit and matches the rest of the workspace.
    //
    // SAFETY: the LV2 host owns `controller` and guarantees it
    // outlives every UI callback that closes over it (LV2_UI__cleanup
    // is the only thing that may invalidate it, and the host doesn't
    // call any of these closures after cleanup).
    let controller_ptr: SendPtr<core::ffi::c_void> =
        unsafe { SendPtr::new(controller.cast_const()) };

    let params_get = params.clone();
    let params_get_plain = params.clone();
    let params_format = params.clone();
    let params_set = params.clone();
    let params_for_ctx = params.clone();
    let meter_slots_for_get = meter_slots.clone();

    // The write_function is a plain extern "C" fn - bitcast-safe to move
    // across closure boundaries. We keep controller as usize to sidestep
    // raw-pointer Send issues.
    let write_set = write_function;

    // Resolve the touch fn pointer + handle once, outside the closures,
    // so neither closure has to dereference the `&'static Lv2UiTouch`
    // through the captured `Option<&...>`. `touch_fn = None` collapses
    // both gesture closures into no-ops without runtime branching.
    //
    // `Lv2UiTouch::touch` is a host-supplied extern "C" fn - Send-safe by
    // ABI. `handle` is a host pointer we never deref ourselves; it's
    // forwarded back as the first arg to the host callback. Box-as-usize
    // would lose the function-pointer ABI, so we cast through usize for
    // `handle` only.
    let touch_fn = touch.and_then(|t| t.touch);
    let touch_handle = touch.map_or(0, |t| t.handle as usize);

    // `ui:resize` (UI → host push). Resolve fn ptr + handle once so the
    // closure stays cheap and avoids deref-through-option per call.
    // `None` collapses the closure into a no-op (host doesn't expose the
    // feature, e.g. jalv); editors can still call `request_resize` and
    // we'll just report failure.
    let host_resize_fn = resize.and_then(|r| r.ui_resize);
    let host_resize_handle = resize.map_or(0, |r| r.handle as usize);

    PluginContext::from_closures(
        ClosureBridge {
            begin_edit: Box::new(move |id: u32| {
                let Some(func) = touch_fn else { return };
                let Some((_, port_index)) = slots_for_begin.iter().find(|(pid, _)| *pid == id)
                else {
                    return;
                };
                unsafe { func(touch_handle as *mut c_void, *port_index, true) };
            }),
            end_edit: Box::new(move |id: u32| {
                let Some(func) = touch_fn else { return };
                let Some((_, port_index)) = slots_for_end.iter().find(|(pid, _)| *pid == id) else {
                    return;
                };
                unsafe { func(touch_handle as *mut c_void, *port_index, false) };
            }),
            request_resize: Box::new(move |w: u32, h: u32| {
                // Push the editor's requested size to the host via
                // the captured `ui:resize` feature. LV2 takes
                // `int32_t`; editor dimensions are bounded by
                // display size, well below `i32::MAX`. The host
                // returns 0 on success.
                let Some(func) = host_resize_fn else {
                    return false;
                };
                #[allow(clippy::cast_possible_wrap)]
                let (iw, ih) = (w as i32, h as i32);
                unsafe { func(host_resize_handle as *mut c_void, iw, ih) == 0 }
            }),
            set_param: Box::new(move |id: u32, normalized: f64| {
                let Some((_, port_index, range)) =
                    slots_for_set.iter().find(|(pid, _, _)| *pid == id)
                else {
                    return;
                };
                let plain = f32::from_f64(range.denormalize(normalized));
                params_set.set_normalized(id, normalized);
                unsafe {
                    let value = plain;
                    write_set(
                        controller_ptr.as_ptr().cast_mut(),
                        *port_index,
                        size_of_u32::<f32>(),
                        0, // LV2_UI_FLOAT_PROTOCOL = 0 (control ports)
                        (&raw const value).cast::<c_void>(),
                    );
                }
            }),
            get_param: Box::new(move |id: u32| params_get.get_normalized(id).unwrap_or(0.0)),
            get_param_plain: Box::new(move |id: u32| params_get_plain.get_plain(id).unwrap_or(0.0)),
            format_param: Box::new(move |id: u32| {
                let v = params_format.get_plain(id).unwrap_or(0.0);
                params_format.format_value(id, v).unwrap_or_default()
            }),
            get_meter: Box::new(move |id: u32| {
                meter_slots_for_get
                    .iter()
                    .find(|m| m.id == id)
                    .map_or(0.0, |m| f32::from_bits(m.value.load(Ordering::Relaxed)))
            }),
            get_state: Box::new(Vec::new),
            set_state: Box::new(|_bytes: Vec<u8>| {}),
            // The DSP broadcasts host transport as `time:Position` atoms on
            // the notify-out port. `port_event` decodes them and writes the
            // slot - this closure just reads the latest value.
            transport: Box::new(move || transport_slot.read()),
        },
        params_for_ctx,
    )
    .with_tasks(tasks)
}

/// Build a static UI descriptor for this plugin type. Monomorphized per P.
#[must_use]
pub fn ui_descriptor<P: PluginExport>(uri: &'static CStr) -> Lv2UiDescriptor {
    Lv2UiDescriptor {
        uri: uri.as_ptr(),
        instantiate: instantiate_ui_tramp::<P>,
        cleanup: cleanup_ui_tramp::<P>,
        port_event: port_event_tramp::<P>,
        extension_data: ui_extension_data_tramp::<P>,
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
    unsafe {
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
}

unsafe extern "C" fn cleanup_ui_tramp<P: PluginExport>(handle: Lv2UiHandle) {
    unsafe {
        cleanup_ui::<P>(handle);
    }
}

unsafe extern "C" fn port_event_tramp<P: PluginExport>(
    handle: Lv2UiHandle,
    port_index: u32,
    buffer_size: u32,
    format: u32,
    buffer: *const c_void,
) {
    unsafe {
        port_event::<P>(handle, port_index, buffer_size, format, buffer);
    }
}

unsafe extern "C" fn ui_extension_data_tramp<P: PluginExport>(uri: *const c_char) -> *const c_void {
    unsafe { ui_extension_data::<P>(uri) }
}
