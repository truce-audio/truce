//! LV2 UI (X11UI) — Phase 6.
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
//! Widget out-parameter currently returns the host-provided PARENT window
//! (pragmatic for Ardour/Jalv which accept it; stricter X11UI hosts may
//! want the actual child window ID — follow-up).

use std::ffi::{c_char, c_void, CStr, CString};
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use truce_core::editor::{Editor, EditorContext, RawWindowHandle};
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

pub const LV2_UI__X11UI: &str = "http://lv2plug.in/ns/extensions/ui#X11UI";
pub const LV2_UI__PARENT: &str = "http://lv2plug.in/ns/extensions/ui#parent";

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
    /// Host callback to write values back to the plugin.
    write_function: Lv2UiWriteFn,
    controller: Lv2UiController,
    /// The plugin's editor — drives rendering.
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
    // Locate PARENT feature — the X11 Window the host wants us to embed in.
    let parent_window = parse_parent_feature(features);
    let Some(parent_window) = parent_window else {
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

    let Some(mut editor) = plugin.editor() else {
        return std::ptr::null_mut();
    };

    // Resolve host URIDs for atom-event decoding on the UI side.
    let urid_map = UridMap::from_features(features);
    let atom_event_transfer_urid =
        urid_map.intern("http://lv2plug.in/ns/ext/atom#eventTransfer");
    let transport_slot = TransportSlot::new();

    // Build EditorContext closures driven by write_function / shadow params.
    let ctx = build_editor_context::<P>(
        params_arc.clone(),
        &param_slots,
        write_function,
        controller,
        transport_slot.clone(),
    );

    editor.open(RawWindowHandle::X11(parent_window), ctx);

    // Set widget out-param. Strict X11UI hosts want the plugin's child
    // window ID; pragmatic ones (Ardour, Jalv) accept the parent. See the
    // module comment for follow-up.
    if !widget.is_null() {
        *widget = parent_window as *mut c_void;
    }

    let ui = Box::new(Lv2UiInstance::<P> {
        _plugin: plugin,
        params: params_arc,
        param_slots,
        write_function,
        controller,
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
        let Some(slot) = ui.param_slots.iter().find(|s| s.port_index == port_index) else {
            return;
        };
        let value = *(buffer as *const f32);
        if !value.is_finite() {
            return;
        }
        ui.params.set_plain(slot.id, value as f64);
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
    if atom_hdr.type_ != ui.urid_map.atom_object
        && atom_hdr.type_ != ui.urid_map.atom_blank
    {
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
    (*one).seq_header.atom.size =
        (core::mem::size_of::<AtomSequenceBody>()
            + core::mem::size_of::<i64>()
            + core::mem::size_of::<Atom>()
            + body_size) as u32;
    (*one).seq_header.body.unit = 0;
    (*one).seq_header.body.pad = 0;
    (*one).event_time = 0;
    (*one).event_body = atom_hdr;
    let ev_body_dest = scratch
        .as_mut_ptr()
        .add(core::mem::size_of::<OneEvent>());
    core::ptr::copy_nonoverlapping(body_ptr, ev_body_dest, body_size);

    let mut info = truce_core::events::TransportInfo::default();
    let reader = AtomSequenceReader::new(
        scratch.as_ptr() as *const AtomSequence,
        &ui.urid_map,
    );
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

unsafe fn parse_parent_feature(features: *const *const LV2Feature) -> Option<u64> {
    if features.is_null() {
        return None;
    }
    let parent_uri = CString::new(LV2_UI__PARENT).ok()?;
    let mut i = 0usize;
    loop {
        let feat_ptr = *features.add(i);
        if feat_ptr.is_null() {
            return None;
        }
        let feat = &*feat_ptr;
        if !feat.uri.is_null() && CStr::from_ptr(feat.uri) == parent_uri.as_c_str() {
            // data is `void*` but semantically a Window ID on X11UI.
            return Some(feat.data as u64);
        }
        i += 1;
    }
}

fn build_editor_context<P: PluginExport>(
    params: Arc<P::Params>,
    slots: &[ParamSlot],
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

    // The write_function is a plain extern "C" fn — bitcast-safe to move
    // across closure boundaries. We keep controller as usize to sidestep
    // raw-pointer Send issues.
    let write_set = write_function;

    EditorContext {
        begin_edit: Arc::new(|_id: u32| {}),
        end_edit: Arc::new(|_id: u32| {}),
        request_resize: Arc::new(|_w: u32, _h: u32| false),
        set_param: Arc::new(move |id: u32, normalized: f64| {
            let Some((_, port_index, range)) =
                slots_for_set.iter().find(|(pid, _, _)| *pid == id)
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
        get_meter: Arc::new(|_id: u32| 0.0),
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
