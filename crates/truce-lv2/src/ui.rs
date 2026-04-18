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
use truce_params::Params;

use crate::types::LV2Feature;

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

    // Build EditorContext closures driven by write_function / shadow params.
    let ctx = build_editor_context::<P>(
        params_arc.clone(),
        &param_slots,
        write_function,
        controller,
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

/// Port value update from host. For control ports the buffer is a single
/// `f32`; we update the shadow params so the UI reads the new value.
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
    if handle.is_null() || buffer.is_null() || format != 0 /* LV2_UI_FLOAT_PROTOCOL */ {
        return;
    }
    if buffer_size < core::mem::size_of::<f32>() as u32 {
        return;
    }
    let ui = &*(handle as *const Lv2UiInstance<P>);
    let Some(slot) = ui.param_slots.iter().find(|s| s.port_index == port_index) else {
        return;
    };
    let value = *(buffer as *const f32);
    if !value.is_finite() {
        return;
    }
    ui.params.set_plain(slot.id, value as f64);
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
