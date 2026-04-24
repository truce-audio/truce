//! VST3 format wrapper for truce.
//!
//! Uses a C++ shim that implements the real VST3 COM interfaces
//! with correct vtable layout. All plugin logic is delegated to
//! Rust via C FFI callbacks.

pub mod ffi;

use std::ffi::CString;
use std::os::raw::c_char;
use std::slice;

use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_core::process::ProcessContext;
use truce_core::state;
use truce_params::Params;

use ffi::{Vst3Callbacks, Vst3MidiEvent, Vst3ParamDescriptor, Vst3PluginDescriptor};

// ---------------------------------------------------------------------------
// Instance wrapper
// ---------------------------------------------------------------------------

struct Vst3Instance<P: PluginExport> {
    plugin: P,
    event_list: EventList,
    output_events: EventList,
    plugin_id_hash: u64,
    sample_rate: f64,
    editor: Option<Box<dyn truce_core::editor::Editor>>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    transport_slot: std::sync::Arc<truce_core::TransportSlot>,
    /// Content scale reported by the host via
    /// `IPlugViewContentScaleSupport::setContentScaleFactor`. Defaults
    /// to 1.0 when the host never calls it (macOS Cocoa hosts, VST3
    /// runners that don't implement the interface). Used to convert
    /// the editor's logical size to physical pixels when reporting
    /// `getSize` on Windows/Linux.
    host_scale: f64,
}

// ---------------------------------------------------------------------------
// C callback implementations
//
// SAFETY for all unsafe extern "C" fn below:
// - `ctx` is a *mut c_void created by Box::into_raw in cb_create().
//   Valid until cb_destroy() (called exactly once by the C++ shim).
// - The C++ shim (TruceComponent) owns the Rust context and
//   guarantees exclusive access: process() on the audio thread,
//   all other callbacks on the main thread, never concurrent.
// - Audio buffer pointers come from the VST3 host via ProcessData
//   and are valid for the declared channel count × numSamples.
// - Parameter IDs and values come from IParamValueQueue and are
//   guaranteed valid by the VST3 host.
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_create<P: PluginExport>() -> *mut std::ffi::c_void {
    let mut plugin = P::create();
    plugin.init();
    let info = P::info();
    let instance = Box::new(Vst3Instance::<P> {
        plugin,
        event_list: EventList::new(),
        output_events: EventList::new(),
        plugin_id_hash: state::hash_plugin_id(info.vst3_id),
        sample_rate: 44100.0,
        editor: None,
        transport_slot: truce_core::TransportSlot::new(),
        host_scale: 1.0,
    });
    Box::into_raw(instance) as *mut std::ffi::c_void
}

unsafe extern "C" fn cb_destroy<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    if !ctx.is_null() {
        drop(Box::from_raw(ctx as *mut Vst3Instance<P>));
    }
}

unsafe extern "C" fn cb_reset<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_rate: f64,
    max_frames: u32,
) {
    let inst = &mut *(ctx as *mut Vst3Instance<P>);
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
    events: *const Vst3MidiEvent,
    num_events: u32,
    transport_ptr: *const ffi::Vst3Transport,
    param_changes: *const ffi::Vst3ParamChange,
    num_param_changes: u32,
) {
    let inst = &mut *(ctx as *mut Vst3Instance<P>);
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
                0xA0 => Some(EventBody::Aftertouch {
                    channel,
                    note: ev.data1,
                    pressure: ev.data2 as f32 / 127.0,
                }),
                0xF0 => {
                    // Note expression: data1=typeId, data2=value*127, _pad=noteId
                    let type_id = ev.data1;
                    let value = ev.data2 as u32 * (0xFFFFFFFF / 127);
                    let note = ev._pad;
                    match type_id {
                        0 => Some(EventBody::PerNoteCC {
                            channel: 0,
                            note,
                            cc: 7,
                            value,
                        }), // volume
                        1 => Some(EventBody::PerNoteCC {
                            channel: 0,
                            note,
                            cc: 10,
                            value,
                        }), // pan
                        2 => Some(EventBody::PerNotePitchBend {
                            channel: 0,
                            note,
                            value,
                        }), // tuning
                        3 => Some(EventBody::PerNoteCC {
                            channel: 0,
                            note,
                            cc: 1,
                            value,
                        }), // vibrato
                        4 => Some(EventBody::PerNoteCC {
                            channel: 0,
                            note,
                            cc: 11,
                            value,
                        }), // expression
                        5 => Some(EventBody::PerNoteCC {
                            channel: 0,
                            note,
                            cc: 74,
                            value,
                        }), // brightness
                        _ => None,
                    }
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

    // Build AudioBuffer from raw pointers
    let mut scratch = truce_core::buffer::RawBufferScratch::default();
    let mut audio_buffer = scratch.build(
        inputs,
        outputs,
        num_input_channels,
        num_output_channels,
        num_frames as u32,
    );

    // Apply sample-accurate parameter changes.
    // The C++ shim sends plain (denormalized) values.
    if !param_changes.is_null() && num_param_changes > 0 {
        let changes = slice::from_raw_parts(param_changes, num_param_changes as usize);
        for pc in changes {
            inst.plugin.params().set_plain(pc.id, pc.value);
            inst.event_list.push(Event {
                sample_offset: pc.sample_offset as u32,
                body: EventBody::ParamChange {
                    id: pc.id,
                    value: pc.value,
                },
            });
        }
        inst.event_list.sort();
    }

    let transport = if !transport_ptr.is_null() {
        let t = &*transport_ptr;
        TransportInfo {
            playing: t.playing != 0,
            recording: t.recording != 0,
            tempo: t.tempo,
            time_sig_num: t.time_sig_num as u8,
            time_sig_den: t.time_sig_den as u8,
            position_samples: t.position_samples as i64,
            position_seconds: 0.0,
            position_beats: t.position_beats,
            bar_start_beats: t.bar_start_beats,
            loop_active: t.cycle_active != 0,
            loop_start_beats: t.cycle_start_beats,
            loop_end_beats: t.cycle_end_beats,
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
    let inst = &*(ctx as *mut Vst3Instance<P>);
    inst.plugin.params().count() as u32
}

unsafe extern "C" fn cb_param_get_descriptor<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out: *mut Vst3ParamDescriptor,
) {
    let inst = &*(ctx as *mut Vst3Instance<P>);
    let infos = inst.plugin.params().param_infos();
    if let Some(info) = infos.get(index as usize) {
        let name = CString::new(info.name).unwrap_or_default();
        let short_name = CString::new(info.short_name).unwrap_or_default();
        let units = CString::new(info.unit.as_str()).unwrap_or_default();
        let group = CString::new(info.group).unwrap_or_default();

        let mut flags: i32 = 0;
        if info.flags.contains(truce_params::ParamFlags::AUTOMATABLE) {
            flags |= 1;
        } // kCanAutomate
        if info.flags.contains(truce_params::ParamFlags::READONLY) {
            flags |= 1 << 1;
        }
        if info.flags.contains(truce_params::ParamFlags::IS_BYPASS) {
            flags |= 1 << 16;
        } // kIsBypass
        if info.flags.contains(truce_params::ParamFlags::HIDDEN) {
            flags |= 1 << 4;
        }
        if info.range.step_count() > 0 {
            flags |= 1 << 8;
        } // kIsList

        let desc = &mut *out;
        desc.id = info.id;
        desc.name = name.into_raw();
        desc.short_name = short_name.into_raw();
        desc.units = units.into_raw();
        desc.min = info.range.min();
        desc.max = info.range.max();
        desc.default_normalized = info.range.normalize(info.default_plain);
        desc.step_count = info.range.step_count() as i32;
        desc.flags = flags;
        desc.group = group.into_raw();
    }
}

unsafe extern "C" fn cb_param_get_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
) -> f64 {
    let inst = &*(ctx as *mut Vst3Instance<P>);
    inst.plugin.params().get_plain(id).unwrap_or(0.0)
}

unsafe extern "C" fn cb_param_set_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
) {
    let inst = &*(ctx as *mut Vst3Instance<P>);
    inst.plugin.params().set_plain(id, value);
}

unsafe extern "C" fn cb_param_normalize<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    plain: f64,
) -> f64 {
    let inst = &*(ctx as *mut Vst3Instance<P>);
    let infos = inst.plugin.params().param_infos();
    infos
        .iter()
        .find(|i| i.id == id)
        .map(|i| i.range.normalize(plain))
        .unwrap_or(plain)
}

unsafe extern "C" fn cb_param_denormalize<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    normalized: f64,
) -> f64 {
    let inst = &*(ctx as *mut Vst3Instance<P>);
    let infos = inst.plugin.params().param_infos();
    infos
        .iter()
        .find(|i| i.id == id)
        .map(|i| i.range.denormalize(normalized))
        .unwrap_or(normalized)
}

unsafe extern "C" fn cb_param_format<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
    out: *mut c_char,
    out_len: u32,
) -> u32 {
    let inst = &*(ctx as *mut Vst3Instance<P>);
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
    let inst = &*(ctx as *mut Vst3Instance<P>);
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
    let inst = &mut *(ctx as *mut Vst3Instance<P>);
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

extern "C" {
    fn malloc(size: usize) -> *mut std::ffi::c_void;
    fn free(ptr: *mut std::ffi::c_void);
}
unsafe fn libc_malloc(size: usize) -> *mut std::ffi::c_void {
    malloc(size)
}
unsafe fn libc_free(ptr: *mut std::ffi::c_void) {
    free(ptr)
}

// ---------------------------------------------------------------------------
// Latency + tail callbacks
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_get_latency<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    let inst = &*(ctx as *mut Vst3Instance<P>);
    inst.plugin.latency()
}

unsafe extern "C" fn cb_get_tail<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    let inst = &*(ctx as *mut Vst3Instance<P>);
    inst.plugin.tail()
}

// ---------------------------------------------------------------------------
// Output event callbacks
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_get_output_event_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    let inst = &*(ctx as *mut Vst3Instance<P>);
    inst.output_events.len() as u32
}

unsafe extern "C" fn cb_get_output_event<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out: *mut Vst3MidiEvent,
) {
    let inst = &*(ctx as *mut Vst3Instance<P>);
    if let Some(event) = inst.output_events.get(index as usize) {
        let midi = &mut *out;
        midi.sample_offset = event.sample_offset;
        match &event.body {
            EventBody::NoteOn {
                channel,
                note,
                velocity,
            } => {
                midi.status = 0x90 | (channel & 0x0F);
                midi.data1 = *note;
                midi.data2 = (*velocity * 127.0) as u8;
            }
            EventBody::NoteOff {
                channel,
                note,
                velocity,
            } => {
                midi.status = 0x80 | (channel & 0x0F);
                midi.data1 = *note;
                midi.data2 = (*velocity * 127.0) as u8;
            }
            EventBody::ControlChange { channel, cc, value } => {
                midi.status = 0xB0 | (channel & 0x0F);
                midi.data1 = *cc;
                midi.data2 = (*value * 127.0) as u8;
            }
            _ => {
                midi.status = 0;
                midi.data1 = 0;
                midi.data2 = 0;
            }
        }
        midi._pad = 0;
    }
}

// ---------------------------------------------------------------------------
// GUI callbacks
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_gui_has_editor<P: PluginExport>(ctx: *mut std::ffi::c_void) -> i32 {
    if ctx.is_null() {
        return 0;
    }
    let inst = &mut *(ctx as *mut Vst3Instance<P>);
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
    let inst = &*(ctx as *mut Vst3Instance<P>);
    if let Some(ref editor) = inst.editor {
        let (ew, eh) = editor.size();
        // VST3 `ViewRect` is documented as "in pixels". That's literally
        // true on Windows/Linux, where hosts expect physical pixels and
        // may drive the scale via `IPlugViewContentScaleSupport`. On
        // macOS, AppKit handles the Retina backing automatically and
        // hosts expect logical points — scaling here would double the
        // window on Retina displays.
        #[cfg(target_os = "macos")]
        {
            *w = ew;
            *h = eh;
        }
        #[cfg(not(target_os = "macos"))]
        {
            *w = (ew as f64 * inst.host_scale) as u32;
            *h = (eh as f64 * inst.host_scale) as u32;
        }
    }
}

unsafe extern "C" fn cb_gui_set_content_scale<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    scale: f64,
) {
    if ctx.is_null() || !scale.is_finite() || scale <= 0.0 {
        return;
    }
    let inst = &mut *(ctx as *mut Vst3Instance<P>);
    inst.host_scale = scale;
    if let Some(ref mut editor) = inst.editor {
        editor.set_scale_factor(scale);
    }
}

unsafe extern "C" fn cb_gui_open<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    parent: *mut std::ffi::c_void,
) {
    let inst = &mut *(ctx as *mut Vst3Instance<P>);
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
            begin_edit: std::sync::Arc::new(move |id| {
                ffi::truce_vst3_begin_edit(ctx_raw.as_ptr() as *mut std::ffi::c_void, id);
            }),
            set_param: std::sync::Arc::new(move |id, value| {
                params_for_set.set_normalized(id, value);
                let norm = params_for_set.get_normalized(id).unwrap_or(0.0);
                ffi::truce_vst3_perform_edit(ctx_raw.as_ptr() as *mut std::ffi::c_void, id, norm);
            }),
            end_edit: std::sync::Arc::new(move |id| {
                ffi::truce_vst3_end_edit(ctx_raw.as_ptr() as *mut std::ffi::c_void, id);
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
        #[cfg(target_os = "macos")]
        let handle = truce_core::editor::RawWindowHandle::AppKit(parent);
        #[cfg(target_os = "windows")]
        let handle = truce_core::editor::RawWindowHandle::Win32(parent);
        #[cfg(target_os = "linux")]
        let handle = truce_core::editor::RawWindowHandle::X11(parent as u64);

        editor.open(handle, context);
    }
}

unsafe extern "C" fn cb_gui_close<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    let inst = &mut *(ctx as *mut Vst3Instance<P>);
    if let Some(ref mut editor) = inst.editor {
        editor.close();
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Compute a 16-byte CID from the VST3 ID string (FNV-1a hash).
/// Install-time override for the host-facing plugin name
/// (`PClassInfo::name`). Populated by `cargo truce install` via the
/// `vst3_name` field in `truce.toml`.
const VST3_NAME_OVERRIDE: Option<&'static str> = option_env!("TRUCE_VST3_NAME_OVERRIDE");

fn resolved_plugin_name(info: &truce_core::info::PluginInfo) -> &'static str {
    match VST3_NAME_OVERRIDE {
        Some(s) if !s.is_empty() => s,
        _ => info.name,
    }
}

fn vst3_cid(id: &str) -> [u8; 16] {
    let mut hash: u128 = 0xcbf29ce484222325_u128 | ((0x100000001b3_u128) << 64);
    for byte in id.bytes() {
        hash ^= byte as u128;
        hash = hash.wrapping_mul(0x01000000_01b3_0000_0000_0000_0001_00B3_u128);
    }
    hash.to_le_bytes()
}

pub fn register_vst3<P: PluginExport>() {
    let info = P::info();
    let instance = P::create();
    let param_infos = instance.params().param_infos();

    let mut param_descs: Vec<Vst3ParamDescriptor> = Vec::with_capacity(param_infos.len());
    for pi in &param_infos {
        let name = CString::new(pi.name).unwrap_or_default();
        let short_name = CString::new(pi.short_name).unwrap_or_default();
        let units = CString::new(pi.unit.as_str()).unwrap_or_default();
        let group = CString::new(pi.group).unwrap_or_default();

        let mut flags: i32 = 0;
        if pi.flags.contains(truce_params::ParamFlags::AUTOMATABLE) {
            flags |= 1;
        }
        if pi.flags.contains(truce_params::ParamFlags::IS_BYPASS) {
            flags |= 1 << 16;
        }
        if pi.range.step_count() > 0 {
            flags |= 1 << 8;
        }

        param_descs.push(Vst3ParamDescriptor {
            id: pi.id,
            name: name.into_raw(),
            short_name: short_name.into_raw(),
            units: units.into_raw(),
            min: pi.range.min(),
            max: pi.range.max(),
            default_normalized: pi.range.normalize(pi.default_plain),
            step_count: pi.range.step_count() as i32,
            flags,
            group: group.into_raw(),
        });
    }

    let name = CString::new(resolved_plugin_name(&info)).unwrap_or_default();
    let vendor = CString::new(info.vendor).unwrap_or_default();
    let url = CString::new(info.url).unwrap_or_default();
    let version = CString::new(info.version).unwrap_or_default();
    let category = CString::new("Audio Module Class").unwrap_or_default();
    let subcategories = CString::new(match info.category {
        PluginCategory::Instrument => "Instrument|Synth",
        PluginCategory::NoteEffect => "Fx|Event",
        PluginCategory::Effect => "Fx",
        PluginCategory::Analyzer => "Fx|Analyzer",
        PluginCategory::Tool => "Fx|Tools",
    })
    .unwrap_or_default();

    let descriptor = Box::leak(Box::new(Vst3PluginDescriptor {
        name: name.into_raw(),
        vendor: vendor.into_raw(),
        url: url.into_raw(),
        email: std::ptr::null(),
        version: version.into_raw(),
        cid: vst3_cid(info.vst3_id),
        category: category.into_raw(),
        subcategories: subcategories.into_raw(),
        num_inputs: P::bus_layouts()
            .first()
            .map(|l| l.total_input_channels())
            .unwrap_or(0),
        num_outputs: P::bus_layouts()
            .first()
            .map(|l| l.total_output_channels())
            .unwrap_or(2),
    }));

    let callbacks = Box::leak(Box::new(Vst3Callbacks {
        create: cb_create::<P>,
        destroy: cb_destroy::<P>,
        reset: cb_reset::<P>,
        process: cb_process::<P>,
        param_count: cb_param_count::<P>,
        param_get_descriptor: cb_param_get_descriptor::<P>,
        param_get_value: cb_param_get_value::<P>,
        param_set_value: cb_param_set_value::<P>,
        param_normalize: cb_param_normalize::<P>,
        param_denormalize: cb_param_denormalize::<P>,
        param_format: cb_param_format::<P>,
        state_save: cb_state_save::<P>,
        state_load: cb_state_load::<P>,
        state_free: cb_state_free,
        get_latency: cb_get_latency::<P>,
        get_tail: cb_get_tail::<P>,
        get_output_event_count: cb_get_output_event_count::<P>,
        get_output_event: cb_get_output_event::<P>,
        gui_has_editor: cb_gui_has_editor::<P>,
        gui_get_size: cb_gui_get_size::<P>,
        gui_open: cb_gui_open::<P>,
        gui_close: cb_gui_close::<P>,
        gui_set_content_scale: cb_gui_set_content_scale::<P>,
    }));

    let param_descs = param_descs.leak();

    unsafe {
        ffi::truce_vst3_register(
            descriptor as *const Vst3PluginDescriptor,
            callbacks as *const Vst3Callbacks,
            param_descs.as_ptr(),
            param_descs.len() as u32,
        );
    }
}

// ---------------------------------------------------------------------------
// export_vst3! macro
// ---------------------------------------------------------------------------

#[macro_export]
macro_rules! export_vst3 {
    ($plugin_type:ty) => {
        mod _vst3_entry {
            use super::*;

            #[no_mangle]
            pub extern "C" fn truce_vst3_init() {
                ::truce_vst3::register_vst3::<$plugin_type>();
            }

            #[no_mangle]
            #[allow(non_snake_case)]
            pub unsafe extern "C" fn GetPluginFactory() -> *mut ::std::ffi::c_void {
                // Lazy init: register on first call
                static INIT: ::std::sync::Once = ::std::sync::Once::new();
                INIT.call_once(|| {
                    truce_vst3_init();
                });
                ::truce_vst3::ffi::truce_vst3_get_factory()
            }

            #[cfg(target_os = "macos")]
            #[no_mangle]
            #[allow(non_snake_case)]
            pub extern "system" fn BundleEntry(_: *mut ::std::ffi::c_void) -> bool {
                true
            }

            #[cfg(target_os = "macos")]
            #[no_mangle]
            pub extern "system" fn bundleEntry(_: *mut ::std::ffi::c_void) -> bool {
                true
            }

            #[cfg(target_os = "macos")]
            #[no_mangle]
            #[allow(non_snake_case)]
            pub extern "system" fn BundleExit() -> bool {
                true
            }

            #[cfg(target_os = "macos")]
            #[no_mangle]
            pub extern "system" fn bundleExit() -> bool {
                true
            }

            #[cfg(target_os = "linux")]
            #[no_mangle]
            #[allow(non_snake_case)]
            pub extern "system" fn ModuleEntry(_: *mut ::std::ffi::c_void) -> bool {
                true
            }

            #[cfg(target_os = "linux")]
            #[no_mangle]
            #[allow(non_snake_case)]
            pub extern "system" fn ModuleExit() -> bool {
                true
            }

            #[cfg(target_os = "windows")]
            #[no_mangle]
            #[allow(non_snake_case)]
            pub extern "system" fn InitDll() -> bool {
                true
            }

            #[cfg(target_os = "windows")]
            #[no_mangle]
            #[allow(non_snake_case)]
            pub extern "system" fn ExitDll() -> bool {
                true
            }
        }
    };
}
