//! VST2 format wrapper for truce.
//!
//! Uses a C shim that implements the AEffect interface. The shim calls
//! back into Rust for all plugin logic via C FFI. Clean-room
//! implementation — no Steinberg SDK headers.

pub mod ffi;

use std::ffi::CString;
use std::os::raw::c_char;
use std::slice;

use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::process::ProcessContext;
use truce_core::state;
use truce_params::Params;

use ffi::{Vst2Callbacks, Vst2MidiEvent, Vst2ParamDescriptor, Vst2PluginDescriptor};

// ---------------------------------------------------------------------------
// Instance wrapper
// ---------------------------------------------------------------------------

struct Vst2Instance<P: PluginExport> {
    plugin: P,
    event_list: EventList,
    output_events: EventList,
    plugin_id_hash: u64,
    sample_rate: f64,
    editor: Option<Box<dyn truce_core::editor::Editor>>,
    /// AEffect pointer, set by the C shim after creation. Used for host callbacks.
    aeffect_ptr: *mut std::ffi::c_void,
    /// Whether state has been loaded at least once (via effSetChunk).
    state_loaded: bool,
    /// Buffered parent window handle when editor open arrives before state load.
    pending_editor_parent: Option<*mut std::ffi::c_void>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    transport_slot: std::sync::Arc<truce_core::TransportSlot>,
}

extern "C" {
    fn truce_vst2_host_begin_edit(effect: *mut std::ffi::c_void, param_id: u32);
    fn truce_vst2_host_automate(effect: *mut std::ffi::c_void, param_id: u32, normalized: f32);
    fn truce_vst2_host_end_edit(effect: *mut std::ffi::c_void, param_id: u32);
    fn truce_vst2_host_get_time(effect: *mut std::ffi::c_void, out: *mut Vst2TransportSnapshot);
}

/// FFI-compatible snapshot filled by `truce_vst2_host_get_time`. Layout
/// must match `Vst2TransportSnapshot` in `vst2_types.h`.
#[repr(C)]
#[derive(Default)]
struct Vst2TransportSnapshot {
    valid: i32,
    playing: i32,
    recording: i32,
    loop_active: i32,
    time_sig_num: i32,
    time_sig_den: i32,
    tempo: f64,
    position_samples: f64,
    position_beats: f64,
    bar_start_beats: f64,
    loop_start_beats: f64,
    loop_end_beats: f64,
}

impl Vst2TransportSnapshot {
    fn to_transport_info(&self) -> TransportInfo {
        TransportInfo {
            playing: self.playing != 0,
            recording: self.recording != 0,
            tempo: self.tempo,
            time_sig_num: self.time_sig_num.clamp(0, u8::MAX as i32) as u8,
            time_sig_den: self.time_sig_den.clamp(0, u8::MAX as i32) as u8,
            position_samples: self.position_samples as i64,
            position_seconds: 0.0,
            position_beats: self.position_beats,
            bar_start_beats: self.bar_start_beats,
            loop_active: self.loop_active != 0,
            loop_start_beats: self.loop_start_beats,
            loop_end_beats: self.loop_end_beats,
        }
    }
}

// ---------------------------------------------------------------------------
// C callback implementations
//
// SAFETY for all unsafe extern "C" fn below:
// - `ctx` is a *mut c_void created by Box::into_raw in cb_create().
//   Valid until cb_destroy() (called exactly once by the C shim).
// - The C shim (vst2_shim.c) owns the Rust context. The host
//   guarantees sequential callback access per plugin instance.
// - Audio buffer pointers come from the host via processReplacing
//   and are valid for numSamples × channel count.
// - effGetChunk/effSetChunk data pointers are managed by the host.
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_create<P: PluginExport>() -> *mut std::ffi::c_void {
    let mut plugin = P::create();
    plugin.init();
    let info = P::info();
    let instance = Box::new(Vst2Instance::<P> {
        plugin,
        event_list: EventList::new(),
        output_events: EventList::new(),
        plugin_id_hash: state::hash_plugin_id(info.clap_id),
        sample_rate: 44100.0,
        editor: None,
        aeffect_ptr: std::ptr::null_mut(),
        state_loaded: false,
        pending_editor_parent: None,
        transport_slot: truce_core::TransportSlot::new(),
    });
    Box::into_raw(instance) as *mut std::ffi::c_void
}

unsafe extern "C" fn cb_destroy<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    if !ctx.is_null() {
        drop(Box::from_raw(ctx as *mut Vst2Instance<P>));
    }
}

unsafe extern "C" fn cb_reset<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_rate: f64,
    max_frames: u32,
) {
    let inst = &mut *(ctx as *mut Vst2Instance<P>);
    inst.sample_rate = sample_rate;
    inst.plugin.reset(sample_rate, max_frames as usize);
    inst.plugin.params().set_sample_rate(sample_rate);
    inst.plugin.params().snap_smoothers();

    // Mark the instance as "fully initialized" so any subsequent
    // `cb_gui_open` calls open the editor immediately rather than
    // deferring. This covers the fresh-instance case where the host
    // calls `effMainsChanged(true)` (→ this reset) but never calls
    // `effSetChunk` because there's no saved state.
    inst.state_loaded = true;

    // If the host opened the editor before state_load but never called
    // state_load (new instance, no saved state), flush the pending open now.
    if let Some(parent) = inst.pending_editor_parent.take() {
        open_editor_inner(inst, parent);
    }
}

unsafe extern "C" fn cb_process<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    inputs: *const *const f32,
    outputs: *mut *mut f32,
    num_input_channels: u32,
    num_output_channels: u32,
    num_frames: u32,
    events: *const Vst2MidiEvent,
    num_events: u32,
) {
    let inst = &mut *(ctx as *mut Vst2Instance<P>);
    let num_frames = num_frames as usize;

    // Convert MIDI events
    inst.event_list.clear();
    if !events.is_null() && num_events > 0 {
        let event_slice = slice::from_raw_parts(events, num_events as usize);
        for ev in event_slice {
            let status = ev.status & 0xF0;
            let channel = ev.status & 0x0F;
            let body = match status {
                0x90 if ev.data2 > 0 => Some(EventBody::NoteOn {
                    channel,
                    note: ev.data1,
                    velocity: ev.data2 as f32 / 127.0,
                }),
                0x90 => Some(EventBody::NoteOff {
                    channel,
                    note: ev.data1,
                    velocity: 0.0,
                }),
                0x80 => Some(EventBody::NoteOff {
                    channel,
                    note: ev.data1,
                    velocity: ev.data2 as f32 / 127.0,
                }),
                0xB0 => Some(EventBody::ControlChange {
                    channel,
                    cc: ev.data1,
                    value: ev.data2 as f32 / 127.0,
                }),
                0xE0 => {
                    let raw = ((ev.data2 as u16) << 7) | (ev.data1 as u16);
                    Some(EventBody::PitchBend {
                        channel,
                        value: (raw as f32 - 8192.0) / 8192.0,
                    })
                }
                _ => None,
            };
            if let Some(body) = body {
                inst.event_list.push(Event {
                    sample_offset: ev.delta_frames,
                    body,
                });
            }
        }
    }
    inst.event_list.sort();

    // Build AudioBuffer from raw pointers (copies input→output for effects)
    let mut scratch = truce_core::buffer::RawBufferScratch::default();
    let mut audio_buffer = scratch.build(
        inputs, outputs, num_input_channels, num_output_channels, num_frames as u32,
    );

    let transport = if !inst.aeffect_ptr.is_null() {
        let mut snap = Vst2TransportSnapshot::default();
        truce_vst2_host_get_time(inst.aeffect_ptr, &mut snap);
        if snap.valid != 0 {
            snap.to_transport_info()
        } else {
            TransportInfo::default()
        }
    } else {
        TransportInfo::default()
    };
    inst.output_events.clear();
    inst.transport_slot.write(&transport);
    let mut context = ProcessContext::new(&transport, inst.sample_rate, num_frames, &mut inst.output_events);

    inst.plugin
        .process(&mut audio_buffer, &inst.event_list, &mut context);
}

unsafe extern "C" fn cb_param_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    let inst = &*(ctx as *mut Vst2Instance<P>);
    inst.plugin.params().count() as u32
}

unsafe extern "C" fn cb_param_get_descriptor<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out: *mut Vst2ParamDescriptor,
) {
    let inst = &*(ctx as *mut Vst2Instance<P>);
    let infos = inst.plugin.params().param_infos();
    if let Some(info) = infos.get(index as usize) {
        let name = CString::new(info.name).unwrap_or_default();
        let unit = CString::new(info.unit.as_str()).unwrap_or_default();
        let group = CString::new(info.group).unwrap_or_default();

        let desc = &mut *out;
        desc.id = info.id;
        desc.name = name.into_raw();
        desc.min = info.range.min();
        desc.max = info.range.max();
        desc.default_value = info.default_plain;
        desc.step_count = info.range.step_count();
        desc.unit = unit.into_raw();
        desc.group = group.into_raw();
    }
}

unsafe extern "C" fn cb_param_get_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
) -> f64 {
    let inst = &*(ctx as *mut Vst2Instance<P>);
    inst.plugin.params().get_plain(id).unwrap_or(0.0)
}

unsafe extern "C" fn cb_param_set_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
) {
    let inst = &*(ctx as *mut Vst2Instance<P>);
    inst.plugin.params().set_plain(id, value);
}

unsafe extern "C" fn cb_param_format_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
    out: *mut c_char,
    out_len: u32,
) -> u32 {
    let inst = &*(ctx as *mut Vst2Instance<P>);
    match inst.plugin.params().format_value(id, value) {
        Some(text) => {
            let bytes = text.as_bytes();
            let len = bytes.len().min(out_len as usize - 1);
            std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, out, len);
            *out.add(len) = 0;
            len as u32
        }
        None => 0,
    }
}

unsafe extern "C" fn cb_state_save<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out_data: *mut *mut u8,
    out_len: *mut u32,
) {
    let inst = &*(ctx as *mut Vst2Instance<P>);
    let (ids, values) = inst.plugin.params().collect_values();
    let extra = inst.plugin.save_state();
    let blob = state::serialize_state(inst.plugin_id_hash, &ids, &values, extra.as_deref());

    let len = blob.len();
    // Use Vec::into_raw_parts pattern for the C shim to free later
    let mut boxed = blob.into_boxed_slice();
    let ptr = boxed.as_mut_ptr();
    std::mem::forget(boxed);
    *out_data = ptr;
    *out_len = len as u32;
}

unsafe extern "C" fn cb_state_load<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    data: *const u8,
    len: u32,
) {
    let inst = &mut *(ctx as *mut Vst2Instance<P>);
    let blob = slice::from_raw_parts(data, len as usize);
    if let Some(deserialized) = state::deserialize_state(blob, inst.plugin_id_hash) {
        inst.plugin.params().restore_values(&deserialized.params);
        if let Some(extra) = &deserialized.extra {
            inst.plugin.load_state(extra);
        }
        // Notify an already-open editor that state changed (undo, preset recall).
        if inst.pending_editor_parent.is_none() {
            if let Some(ref mut editor) = inst.editor {
                editor.state_changed();
            }
        }
    }
    inst.state_loaded = true;

    // If the host opened the editor before loading state, open it now.
    if let Some(parent) = inst.pending_editor_parent.take() {
        open_editor_inner(inst, parent);
    }
}

unsafe extern "C" fn cb_state_free(data: *mut u8, len: u32) {
    if !data.is_null() && len > 0 {
        drop(Vec::from_raw_parts(data, len as usize, len as usize));
    }
}

// ---------------------------------------------------------------------------
// Latency + tail
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_get_latency<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    let inst = &*(ctx as *mut Vst2Instance<P>);
    inst.plugin.latency()
}

unsafe extern "C" fn cb_get_tail<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    let inst = &*(ctx as *mut Vst2Instance<P>);
    inst.plugin.tail()
}

// ---------------------------------------------------------------------------
// GUI callbacks
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_gui_has_editor<P: PluginExport>(ctx: *mut std::ffi::c_void) -> i32 {
    if ctx.is_null() {
        return 0;
    }
    let inst = &mut *(ctx as *mut Vst2Instance<P>);
    if inst.editor.is_none() {
        inst.editor = inst.plugin.editor();
    }
    if inst.editor.is_some() {
        1
    } else {
        0
    }
}

unsafe extern "C" fn cb_gui_get_size<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    w: *mut u32,
    h: *mut u32,
) {
    let inst = &*(ctx as *mut Vst2Instance<P>);
    if let Some(ref editor) = inst.editor {
        let (ew, eh) = editor.size();
        let scale = editor.scale_factor();
        *w = (ew as f64 * scale) as u32;
        *h = (eh as f64 * scale) as u32;
    }
}

unsafe extern "C" fn cb_set_effect_ptr<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    effect: *mut std::ffi::c_void,
) {
    let inst = &mut *(ctx as *mut Vst2Instance<P>);
    inst.aeffect_ptr = effect;
}

/// Actually open the editor with the given parent window handle.
unsafe fn open_editor_inner<P: PluginExport>(
    inst: &mut Vst2Instance<P>,
    parent: *mut std::ffi::c_void,
) {
    if let Some(ref mut editor) = inst.editor {
        let params = inst.plugin.params_arc();
        let plugin_ptr = truce_core::editor::SendPtr::new(&inst.plugin as *const P);
        let effect_ptr = truce_core::editor::SendPtr::new(inst.aeffect_ptr);
        let params_for_set = params.clone();
        let params_for_get = params.clone();
        let params_for_plain = params.clone();
        let params_for_fmt = params.clone();
        let transport_slot = inst.transport_slot.clone();
        let context = truce_core::editor::EditorContext {
            begin_edit: std::sync::Arc::new(move |id| {
                if !effect_ptr.as_ptr().is_null() {
                    truce_vst2_host_begin_edit(effect_ptr.as_ptr() as *mut std::ffi::c_void, id);
                }
            }),
            set_param: std::sync::Arc::new(move |id, value| {
                params_for_set.set_normalized(id, value);
                if !effect_ptr.as_ptr().is_null() {
                    let norm = params_for_set.get_normalized(id).unwrap_or(0.0) as f32;
                    truce_vst2_host_automate(effect_ptr.as_ptr() as *mut std::ffi::c_void, id, norm);
                }
            }),
            end_edit: std::sync::Arc::new(move |id| {
                if !effect_ptr.as_ptr().is_null() {
                    truce_vst2_host_end_edit(effect_ptr.as_ptr() as *mut std::ffi::c_void, id);
                }
            }),
            request_resize: std::sync::Arc::new(|_w, _h| false),
            get_param: std::sync::Arc::new(move |id| {
                params_for_get.get_normalized(id).unwrap_or(0.0)
            }),
            get_param_plain: std::sync::Arc::new(move |id| {
                params_for_plain.get_plain(id).unwrap_or(0.0)
            }),
            format_param: std::sync::Arc::new(move |id| {
                let plain = params_for_fmt.get_plain(id).unwrap_or(0.0);
                params_for_fmt.format_value(id, plain).unwrap_or_else(|| format!("{:.1}", plain))
            }),
            get_meter: std::sync::Arc::new(move |id| {
                let plugin = plugin_ptr.get();
                plugin.get_meter(id)
            }),
            get_state: std::sync::Arc::new(move || {
                let plugin = plugin_ptr.get();
                plugin.save_state().unwrap_or_default()
            }),
            set_state: std::sync::Arc::new(move |data| {
                let plugin = &mut *(plugin_ptr.as_ptr() as *mut P);
                plugin.load_state(&data);
            }),
            transport: std::sync::Arc::new(move || transport_slot.read()),
        };
        #[cfg(target_os = "macos")]
        let handle = truce_core::editor::RawWindowHandle::AppKit(parent);
        #[cfg(target_os = "windows")]
        let handle = truce_core::editor::RawWindowHandle::Win32(parent);
        #[cfg(target_os = "linux")]
        let handle = truce_core::editor::RawWindowHandle::X11(parent as u64);

        editor.open(handle, context);
    }
}

unsafe extern "C" fn cb_gui_open<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    parent: *mut std::ffi::c_void,
) {
    let inst = &mut *(ctx as *mut Vst2Instance<P>);
    if inst.state_loaded {
        // State already restored — open immediately.
        open_editor_inner(inst, parent);
    } else {
        // Host opened editor before loading state (Reaper VST2 ordering).
        // Buffer the parent handle; we'll open after state_load.
        inst.pending_editor_parent = Some(parent);
    }
}

unsafe extern "C" fn cb_gui_close<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    let inst = &mut *(ctx as *mut Vst2Instance<P>);
    if let Some(ref mut editor) = inst.editor {
        editor.close();
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_vst2<P: PluginExport>() {
    let info = P::info();
    let layouts = P::bus_layouts();
    let layout = layouts
        .first()
        .expect("Plugin must have at least one bus layout");

    let name = CString::new(info.name).unwrap();
    let vendor = CString::new(info.vendor).unwrap();

    let descriptor = Box::leak(Box::new(Vst2PluginDescriptor {
        component_type: info.au_type,
        component_subtype: info.fourcc,
        name: name.into_raw(),
        vendor: vendor.into_raw(),
        version: 1,
        num_inputs: layout.total_input_channels(),
        num_outputs: layout.total_output_channels(),
    }));

    let callbacks = Box::leak(Box::new(Vst2Callbacks {
        create: cb_create::<P>,
        destroy: cb_destroy::<P>,
        reset: cb_reset::<P>,
        process: cb_process::<P>,
        param_count: cb_param_count::<P>,
        param_get_descriptor: cb_param_get_descriptor::<P>,
        param_get_value: cb_param_get_value::<P>,
        param_set_value: cb_param_set_value::<P>,
        param_format_value: cb_param_format_value::<P>,
        state_save: cb_state_save::<P>,
        state_load: cb_state_load::<P>,
        state_free: cb_state_free,
        get_latency: cb_get_latency::<P>,
        get_tail: cb_get_tail::<P>,
        set_effect_ptr: cb_set_effect_ptr::<P>,
        gui_has_editor: cb_gui_has_editor::<P>,
        gui_get_size: cb_gui_get_size::<P>,
        gui_open: cb_gui_open::<P>,
        gui_close: cb_gui_close::<P>,
    }));

    // Build param descriptors
    let temp_plugin = P::create();
    let infos = temp_plugin.params().param_infos();
    let mut param_descs: Vec<Vst2ParamDescriptor> = Vec::with_capacity(infos.len());
    for pi in &infos {
        let name = CString::new(pi.name).unwrap();
        let unit = CString::new(pi.unit.as_str()).unwrap();
        let group = CString::new(pi.group).unwrap();
        param_descs.push(Vst2ParamDescriptor {
            id: pi.id,
            name: name.into_raw(),
            min: pi.range.min(),
            max: pi.range.max(),
            default_value: pi.default_plain,
            step_count: pi.range.step_count(),
            unit: unit.into_raw(),
            group: group.into_raw(),
        });
    }
    let num_params = param_descs.len() as u32;
    let params_ptr = Box::leak(param_descs.into_boxed_slice()).as_ptr();

    unsafe {
        ffi::truce_vst2_register(descriptor, callbacks, params_ptr, num_params);
    }
}

// ---------------------------------------------------------------------------
// Export macro
// ---------------------------------------------------------------------------

#[macro_export]
macro_rules! export_vst2 {
    ($plugin_type:ty) => {
        #[allow(non_snake_case)]
        mod _vst2_entry {
            use super::*;

            // Register the plugin when the library is loaded.
            // VSTPluginMain (in vst2_shim.c) checks g_vst2_callbacks
            // so registration must happen before the host calls it.
            #[used]
            #[cfg_attr(target_os = "linux", link_section = ".init_array")]
            #[cfg_attr(target_os = "macos", link_section = "__DATA,__mod_init_func")]
            #[cfg_attr(target_os = "windows", link_section = ".CRT$XCU")]
            static INIT: extern "C" fn() = {
                extern "C" fn init() {
                    ::truce_vst2::register_vst2::<$plugin_type>();
                }
                init
            };
        }
    };
}
