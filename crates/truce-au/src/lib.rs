//! Audio Unit v3 format wrapper for truce.
//!
//! Uses an Objective-C shim compiled via `cc` that implements the
//! `AUAudioUnit` subclass. The shim calls back into Rust for all
//! plugin logic via C FFI.

pub mod ffi;

use std::ffi::CString;
use std::os::raw::c_char;
use std::slice;

use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::process::ProcessContext;
use truce_core::state;
use truce_params::Params;

use ffi::{AuCallbacks, AuMidiEvent, AuParamDescriptor, AuPluginDescriptor, AuTransportSnapshot};

// ---------------------------------------------------------------------------
// Instance wrapper — one per plugin instance, stored as the opaque ctx
// ---------------------------------------------------------------------------

struct AuInstance<P: PluginExport> {
    plugin: P,
    event_list: EventList,
    output_events: EventList,
    plugin_id_hash: u64,
    sample_rate: f64,
    editor: Option<Box<dyn truce_core::editor::Editor>>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    transport_slot: std::sync::Arc<truce_core::TransportSlot>,
}

// ---------------------------------------------------------------------------
// C callback implementations (generic over P)
//
// SAFETY for all unsafe extern "C" fn below:
// - `ctx` is a *mut c_void created by Box::into_raw in cb_create().
//   Valid until cb_destroy() (called exactly once by the AU shim).
// - The AU v2 shim (au_v2_shim.c) and v3 shim (au_shim.m) own the
//   Rust context. The AU host guarantees: render callback on the
//   audio thread with exclusive access; all other callbacks on the
//   main thread, serialized.
// - Audio buffer pointers come from the host's AudioBufferList and
//   are valid for the declared channel count × frame count.
// - MIDI events come from MusicDeviceMIDIEvent (v2) or
//   AURenderEvent linked list (v3).
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_create<P: PluginExport>() -> *mut std::ffi::c_void {
    let mut plugin = P::create();
    plugin.init();
    let info = P::info();
    let instance = Box::new(AuInstance::<P> {
        plugin,
        event_list: EventList::new(),
        output_events: EventList::new(),
        plugin_id_hash: state::hash_plugin_id(info.clap_id), // reuse CLAP ID for hashing
        sample_rate: 44100.0,
        editor: None,
        transport_slot: truce_core::TransportSlot::new(),
    });
    Box::into_raw(instance) as *mut std::ffi::c_void
}

unsafe extern "C" fn cb_destroy<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    if !ctx.is_null() {
        drop(Box::from_raw(ctx as *mut AuInstance<P>));
    }
}

unsafe extern "C" fn cb_reset<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_rate: f64,
    max_frames: u32,
) {
    let inst = &mut *(ctx as *mut AuInstance<P>);
    inst.sample_rate = sample_rate;
    inst.plugin.reset(sample_rate, max_frames as usize);
    inst.plugin.params().set_sample_rate(sample_rate);
    inst.plugin.params().snap_smoothers();
}

unsafe extern "C" fn cb_process<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    inputs: *const *const f32,
    outputs: *mut *mut f32,
    num_input_channels: u32,
    num_output_channels: u32,
    num_frames: u32,
    events: *const AuMidiEvent,
    num_events: u32,
    transport_ptr: *const AuTransportSnapshot,
) {
    let inst = &mut *(ctx as *mut AuInstance<P>);
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
                    sample_offset: ev.sample_offset,
                    body,
                });
            }
        }
    }
    inst.event_list.sort();

    // Build AudioBuffer from raw pointers (copies input→output for effects)
    let mut scratch = truce_core::buffer::RawBufferScratch::default();
    let mut audio_buffer = scratch.build(
        inputs,
        outputs,
        num_input_channels,
        num_output_channels,
        num_frames as u32,
    );

    let transport = if !transport_ptr.is_null() && (*transport_ptr).valid != 0 {
        let t = &*transport_ptr;
        TransportInfo {
            playing: t.playing != 0,
            recording: t.recording != 0,
            tempo: t.tempo,
            time_sig_num: t.time_sig_num.clamp(0, u8::MAX as i32) as u8,
            time_sig_den: t.time_sig_den.clamp(0, u8::MAX as i32) as u8,
            position_samples: t.position_samples as i64,
            position_seconds: 0.0,
            position_beats: t.position_beats,
            bar_start_beats: t.bar_start_beats,
            loop_active: t.loop_active != 0,
            loop_start_beats: t.loop_start_beats,
            loop_end_beats: t.loop_end_beats,
        }
    } else {
        TransportInfo::default()
    };
    inst.output_events.clear();
    inst.transport_slot.write(&transport);
    let mut context = ProcessContext::new(
        &transport,
        inst.sample_rate,
        num_frames,
        &mut inst.output_events,
    );

    inst.plugin
        .process(&mut audio_buffer, &inst.event_list, &mut context);
}

unsafe extern "C" fn cb_param_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    let inst = &*(ctx as *mut AuInstance<P>);
    inst.plugin.params().count() as u32
}

unsafe extern "C" fn cb_param_get_descriptor<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out: *mut AuParamDescriptor,
) {
    let inst = &*(ctx as *mut AuInstance<P>);
    let infos = inst.plugin.params().param_infos();
    if let Some(info) = infos.get(index as usize) {
        // Store strings in leaked CStrings (they live for the process lifetime)
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
    let inst = &*(ctx as *mut AuInstance<P>);
    inst.plugin.params().get_plain(id).unwrap_or(0.0)
}

unsafe extern "C" fn cb_param_set_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
) {
    let inst = &*(ctx as *mut AuInstance<P>);
    inst.plugin.params().set_plain(id, value);
}

unsafe extern "C" fn cb_param_format_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
    out: *mut c_char,
    out_len: u32,
) -> u32 {
    let inst = &*(ctx as *mut AuInstance<P>);
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
    let inst = &*(ctx as *mut AuInstance<P>);
    let (ids, values) = inst.plugin.params().collect_values();
    let extra = inst.plugin.save_state();
    let blob = state::serialize_state(inst.plugin_id_hash, &ids, &values, extra.as_deref());

    let len = blob.len();
    let ptr = libc_malloc(len) as *mut u8;
    if !ptr.is_null() {
        std::ptr::copy_nonoverlapping(blob.as_ptr(), ptr, len);
    }
    *out_data = ptr;
    *out_len = len as u32;
}

unsafe extern "C" fn cb_state_load<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    data: *const u8,
    len: u32,
) {
    let inst = &mut *(ctx as *mut AuInstance<P>);
    let blob = slice::from_raw_parts(data, len as usize);
    if let Some(deserialized) = state::deserialize_state(blob, inst.plugin_id_hash) {
        inst.plugin.params().restore_values(&deserialized.params);
        if let Some(extra) = &deserialized.extra {
            inst.plugin.load_state(extra);
        }
        if let Some(ref mut editor) = inst.editor {
            editor.state_changed();
        }
    }
}

unsafe extern "C" fn cb_state_free(_data: *mut u8, _len: u32) {
    if !_data.is_null() {
        libc_free(_data as *mut std::ffi::c_void);
    }
}

// ---------------------------------------------------------------------------
// GUI callbacks
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_gui_has_editor<P: PluginExport>(ctx: *mut std::ffi::c_void) -> i32 {
    if ctx.is_null() {
        return 0;
    }
    let inst = &mut *(ctx as *mut AuInstance<P>);
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
    let inst = &*(ctx as *mut AuInstance<P>);
    if let Some(ref editor) = inst.editor {
        // AU is macOS-only; hosts embed our NSView inside a Cocoa
        // container at logical-point coordinates and AppKit handles
        // the Retina backing transparently. Report the editor size
        // as-is — no scaling.
        let (ew, eh) = editor.size();
        *w = ew;
        *h = eh;
    }
}

unsafe extern "C" fn cb_gui_open<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    parent: *mut std::ffi::c_void,
) {
    let inst = &mut *(ctx as *mut AuInstance<P>);
    if let Some(ref mut editor) = inst.editor {
        let params = inst.plugin.params_arc();
        let plugin_ptr = truce_core::editor::SendPtr::new(&inst.plugin as *const P);
        let ctx_raw = truce_core::editor::SendPtr::new(ctx);
        let params_for_set = params.clone();
        let params_for_get = params.clone();
        let params_for_plain = params.clone();
        let params_for_fmt = params.clone();
        let transport_slot = inst.transport_slot.clone();
        let context = truce_core::editor::EditorContext {
            begin_edit: std::sync::Arc::new(|_id| {}),
            set_param: std::sync::Arc::new(move |id, value| {
                params_for_set.set_normalized(id, value);
                // Notify AU host of the parameter change
                let plain = params_for_set.get_plain(id).unwrap_or(0.0) as f32;
                truce_au_v2_host_set_param(ctx_raw.as_ptr() as *mut std::ffi::c_void, id, plain);
            }),
            end_edit: std::sync::Arc::new(|_id| {}),
            request_resize: std::sync::Arc::new(|_w, _h| false),
            get_param: std::sync::Arc::new(move |id| {
                params_for_get.get_normalized(id).unwrap_or(0.0)
            }),
            get_param_plain: std::sync::Arc::new(move |id| {
                params_for_plain.get_plain(id).unwrap_or(0.0)
            }),
            format_param: std::sync::Arc::new(move |id| {
                let plain = params_for_fmt.get_plain(id).unwrap_or(0.0);
                params_for_fmt
                    .format_value(id, plain)
                    .unwrap_or_else(|| format!("{:.1}", plain))
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
        let handle = truce_core::editor::RawWindowHandle::AppKit(parent);
        editor.open(handle, context);
    }
}

unsafe extern "C" fn cb_gui_close<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    let inst = &mut *(ctx as *mut AuInstance<P>);
    if let Some(ref mut editor) = inst.editor {
        editor.close();
    }
    // Keep the editor alive — just closed, not dropped.
    // Dropping and recreating on each open/close cycle can cause
    // instability in AU v3 appex (the audio thread accesses the same
    // AuInstance via raw pointer). The editor will be reopened in-place
    // by the next gui_open call.
}

extern "C" {
    fn malloc(size: usize) -> *mut std::ffi::c_void;
    fn free(ptr: *mut std::ffi::c_void);
    fn truce_au_v2_host_set_param(ctx: *mut std::ffi::c_void, param_id: u32, value: f32);
}

unsafe fn libc_malloc(size: usize) -> *mut std::ffi::c_void {
    malloc(size)
}

unsafe fn libc_free(ptr: *mut std::ffi::c_void) {
    free(ptr)
}

// ---------------------------------------------------------------------------
// Registration: called once from the export_au! macro
// ---------------------------------------------------------------------------

/// Register the plugin with the AU system. Must be called once at load time
/// (typically from a constructor function generated by `export_au!`).
/// Install-time override for the host-facing AU display name.
/// Populated by `cargo truce install` from the `au_name` (AU v2) or
/// `au3_name` (AU v3) field in `truce.toml`; each build is targeted
/// to one AU version so one env var covers both.
const AU_NAME_OVERRIDE: Option<&'static str> = option_env!("TRUCE_AU_NAME_OVERRIDE");

fn resolved_plugin_name(info: &truce_core::info::PluginInfo) -> &'static str {
    truce_core::info::resolve_name_override(AU_NAME_OVERRIDE, info.name)
}

pub fn register_au<P: PluginExport>() {
    let info = P::info();

    // Build param descriptors
    let instance = P::create();
    let param_infos = instance.params().param_infos();
    let mut param_descs: Vec<AuParamDescriptor> = Vec::with_capacity(param_infos.len());

    for pi in &param_infos {
        let cs = truce_core::wrapper::ParamCStrings::from_info(pi);
        param_descs.push(AuParamDescriptor {
            id: pi.id,
            name: cs.name.into_raw(),
            min: pi.range.min(),
            max: pi.range.max(),
            default_value: pi.default_plain,
            step_count: pi.range.step_count(),
            unit: cs.unit.into_raw(),
            group: cs.group.into_raw(),
        });
    }

    let name = CString::new(resolved_plugin_name(&info)).unwrap_or_default();
    let vendor = CString::new(info.vendor).unwrap_or_default();

    let descriptor = Box::leak(Box::new(AuPluginDescriptor {
        component_type: info.au_type,
        component_subtype: info.fourcc,
        component_manufacturer: info.au_manufacturer,
        name: name.into_raw(),
        vendor: vendor.into_raw(),
        version: 0x00010000, // 1.0.0
        num_inputs: truce_core::wrapper::default_io_channels::<P>().0,
        num_outputs: truce_core::wrapper::default_io_channels::<P>().1,
    }));

    let callbacks = Box::leak(Box::new(AuCallbacks {
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
        gui_has_editor: cb_gui_has_editor::<P>,
        gui_get_size: cb_gui_get_size::<P>,
        gui_open: cb_gui_open::<P>,
        gui_close: cb_gui_close::<P>,
    }));

    let param_descs = param_descs.leak();

    unsafe {
        ffi::truce_au_register(
            descriptor as *const AuPluginDescriptor,
            callbacks as *const AuCallbacks,
            param_descs.as_ptr(),
            param_descs.len() as u32,
        );

        // Reference the ObjC shim symbols to force the linker to include them
        std::hint::black_box(ffi::truce_au_register as *const ());
    }
}

// ---------------------------------------------------------------------------
// export_au! macro
// ---------------------------------------------------------------------------

/// Export an Audio Unit v3 plugin entry point.
///
/// Usage:
/// ```ignore
/// export_au!(MyPlugin);
/// ```
///
/// Where `MyPlugin` implements `PluginExport`.
#[macro_export]
macro_rules! export_au {
    ($plugin_type:ty) => {
        #[cfg(target_os = "macos")]
        mod _au_entry {
            use super::*;

            /// Called by the constructor to init the plugin.
            #[no_mangle]
            pub extern "C" fn truce_au_init() {
                ::truce_au::register_au::<$plugin_type>();
            }

            // AU v2 factory — delegates to au_v2_shim.c if compiled,
            // otherwise the weak stub in au_shim_common.c returns NULL.
            extern "C" {
                fn truce_au_v2_factory_bridge(
                    desc: *const ::std::ffi::c_void,
                ) -> *mut ::std::ffi::c_void;
            }

            #[no_mangle]
            pub unsafe extern "C" fn TruceAUFactory(
                desc: *const ::std::ffi::c_void,
            ) -> *mut ::std::ffi::c_void {
                truce_au_v2_factory_bridge(desc)
            }
        }
    };
}
