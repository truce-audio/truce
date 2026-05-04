//! VST3 format wrapper for truce.
//!
//! Uses a C++ shim that implements the real VST3 COM interfaces
//! with correct vtable layout. All plugin logic is delegated to
//! Rust via C FFI callbacks.

pub mod ffi;

use std::ffi::CString;
use std::os::raw::c_char;
use std::slice;

use truce_core::editor::{ClosureBridge, Editor, PluginContext, RawWindowHandle, SendPtr};
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_core::process::ProcessContext;
use truce_core::state;
use truce_params::Params;

use ffi::{Vst3Callbacks, Vst3MidiEvent, Vst3ParamDescriptor, Vst3PluginDescriptor};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Instance wrapper
// ---------------------------------------------------------------------------

struct Vst3Instance<P: PluginExport> {
    plugin: P,
    event_list: EventList,
    output_events: EventList,
    plugin_id_hash: u64,
    sample_rate: f64,
    /// Max block size declared by the host in `setupProcessing`.
    /// Used to debug-assert that `cb_process` never receives more
    /// frames than the plugin was sized for.
    max_block_size: usize,
    /// Reused per-block scratch for `RawBufferScratch::build`.
    /// Lives on the instance so the audio thread doesn't allocate.
    scratch: truce_core::buffer::RawBufferScratch,
    /// Cached `(id, range)` pairs sorted by id. Built once in
    /// `cb_create` from `params().param_infos()`. Hosts call
    /// `cb_param_normalize` / `cb_param_denormalize` extremely often
    /// while reading automation; rebuilding the full `Vec<ParamInfo>`
    /// per call (the previous behavior) is a heap allocation on what
    /// the host treats as a tight read path. Ranges are static for
    /// the life of the plugin instance, so caching them is safe.
    param_ranges: Vec<(u32, truce_params::ParamRange)>,
    editor: Option<Box<dyn Editor>>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    transport_slot: Arc<truce_core::TransportSlot>,
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
    let mut param_ranges: Vec<(u32, truce_params::ParamRange)> = plugin
        .params()
        .param_infos()
        .into_iter()
        .map(|i| (i.id, i.range))
        .collect();
    // Sort by id so `binary_search_by_key` works in the hot lookups.
    param_ranges.sort_by_key(|(id, _)| *id);
    let instance = Box::new(Vst3Instance::<P> {
        plugin,
        event_list: EventList::new(),
        output_events: EventList::new(),
        plugin_id_hash: state::hash_plugin_id(info.vst3_id),
        sample_rate: 44100.0,
        max_block_size: 0,
        scratch: truce_core::buffer::RawBufferScratch::default(),
        param_ranges,
        editor: None,
        transport_slot: truce_core::TransportSlot::new(),
        host_scale: 1.0,
    });
    Box::into_raw(instance) as *mut std::ffi::c_void
}

unsafe extern "C" fn cb_destroy<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    unsafe {
        if !ctx.is_null() {
            drop(Box::from_raw(ctx as *mut Vst3Instance<P>));
        }
    }
}

unsafe extern "C" fn cb_reset<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_rate: f64,
    max_frames: u32,
) {
    unsafe {
        let inst = &mut *(ctx as *mut Vst3Instance<P>);
        inst.sample_rate = sample_rate;
        inst.max_block_size = max_frames as usize;
        inst.plugin.reset(sample_rate, max_frames as usize);
        inst.plugin.params().set_sample_rate(sample_rate);
        inst.plugin.params().snap_smoothers();
    }
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
    unsafe {
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
                        // Note expression: data1=typeId, data2=value*127, _pad=noteId.
                        // Spec says data2 ∈ 0..=127, but the C++ shim isn't required
                        // to clamp — values 128..=255 are ABI-legal. Clamp first
                        // and scale through u64 so the multiplication can't wrap
                        // (the previous `data2 * (u32::MAX / 127)` overflowed u32
                        // for any data2 ≥ 128, and undershot full range — `u32::MAX
                        // / 127` truncates, so even data2 == 127 stopped 15 short
                        // of u32::MAX). Now data2 == 127 maps to exactly u32::MAX.
                        let type_id = ev.data1;
                        let data2_clamped = ev.data2.min(127) as u64;
                        let value = (data2_clamped * u32::MAX as u64 / 127) as u32;
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
        // Sort happens once below — after the param-change push
        // section also runs — instead of twice.

        // Build AudioBuffer from raw pointers. Uses the per-instance
        // `scratch` so the audio thread doesn't heap-allocate.
        debug_assert!(
            num_frames <= inst.max_block_size,
            "host violated VST3 contract: process() got {num_frames} frames \
             but setupProcessing declared max {}",
            inst.max_block_size
        );
        let mut audio_buffer = inst.scratch.build(
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
        }
        // Single stable sort across the merged MIDI + param-change
        // streams. Stable sort preserves the within-group order each
        // section already pushed in.
        inst.event_list.sort();

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
}

unsafe extern "C" fn cb_param_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        // Read the cached `param_ranges.len()` rather than walking the
        // `Params` impl. The cache is built once at instantiation
        // (`Vst3Instance::new`) and never grows; trait dispatch was
        // free per-call but consistent with the cache-first pattern
        // the rest of the file uses.
        let inst = &*(ctx as *mut Vst3Instance<P>);
        inst.param_ranges.len() as u32
    }
}

unsafe extern "C" fn cb_param_get_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
) -> f64 {
    unsafe {
        let inst = &*(ctx as *mut Vst3Instance<P>);
        inst.plugin.params().get_plain(id).unwrap_or(0.0)
    }
}

unsafe extern "C" fn cb_param_set_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
) {
    unsafe {
        let inst = &*(ctx as *mut Vst3Instance<P>);
        inst.plugin.params().set_plain(id, value);
    }
}

unsafe extern "C" fn cb_param_normalize<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    plain: f64,
) -> f64 {
    unsafe {
        let inst = &*(ctx as *mut Vst3Instance<P>);
        match inst.param_ranges.binary_search_by_key(&id, |(i, _)| *i) {
            Ok(idx) => inst.param_ranges[idx].1.normalize(plain),
            Err(_) => plain,
        }
    }
}

unsafe extern "C" fn cb_param_denormalize<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    normalized: f64,
) -> f64 {
    unsafe {
        let inst = &*(ctx as *mut Vst3Instance<P>);
        match inst.param_ranges.binary_search_by_key(&id, |(i, _)| *i) {
            Ok(idx) => inst.param_ranges[idx].1.denormalize(normalized),
            Err(_) => normalized,
        }
    }
}

unsafe extern "C" fn cb_param_format<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
    out: *mut c_char,
    out_len: u32,
) -> u32 {
    unsafe {
        // `out_len == 0` would underflow on `out_len as usize - 1`
        // and let `copy_nonoverlapping` write the full formatted
        // string into a buffer the host claimed had zero capacity.
        // Treat zero capacity as "host wants nothing" and return.
        if out_len == 0 || out.is_null() {
            return 0;
        }
        let inst = &*(ctx as *mut Vst3Instance<P>);
        match inst.plugin.params().format_value(id, value) {
            Some(text) => {
                let bytes = text.as_bytes();
                let len = bytes.len().min((out_len as usize) - 1);
                std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, out, len);
                *out.add(len) = 0;
                len as u32
            }
            None => 0,
        }
    }
}

unsafe extern "C" fn cb_state_save<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out_data: *mut *mut u8,
    out_len: *mut u32,
) {
    unsafe {
        let inst = &*(ctx as *mut Vst3Instance<P>);
        let (ids, values) = inst.plugin.params().collect_values();
        let extra = inst.plugin.save_state();
        let blob = state::serialize_state(inst.plugin_id_hash, &ids, &values, extra.as_deref());
        let len = blob.len();
        let ptr = libc_malloc(len) as *mut u8;
        if ptr.is_null() {
            // malloc failed — tell the host we wrote nothing rather
            // than leaving `*out_data` as a stale value while
            // `*out_len = len` claims `len` bytes are there. The C++
            // shim's `getState` returns kResultFalse / null on this.
            *out_data = std::ptr::null_mut();
            *out_len = 0;
            return;
        }
        std::ptr::copy_nonoverlapping(blob.as_ptr(), ptr, len);
        *out_data = ptr;
        *out_len = len as u32;
    }
}

unsafe extern "C" fn cb_state_load<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    data: *const u8,
    len: u32,
) {
    unsafe {
        let inst = &mut *(ctx as *mut Vst3Instance<P>);
        // `slice::from_raw_parts(null, n)` for `n > 0` is UB. Treat
        // `(null, *)` and `(_, 0)` the same as "host gave us nothing".
        if data.is_null() || len == 0 {
            return;
        }
        let blob = slice::from_raw_parts(data, len as usize);
        if let Some(deserialized) = state::deserialize_state(blob, inst.plugin_id_hash) {
            inst.plugin.params().restore_values(&deserialized.params);
            inst.plugin.params().snap_smoothers();
            if let Some(extra) = &deserialized.extra {
                inst.plugin.load_state(extra);
            }
            if let Some(ref mut editor) = inst.editor {
                editor.state_changed();
            }
        }
    }
}

unsafe extern "C" fn cb_state_free(_data: *mut u8, _len: u32) {
    unsafe {
        if !_data.is_null() {
            libc_free(_data as *mut std::ffi::c_void);
        }
    }
}

unsafe extern "C" {
    fn malloc(size: usize) -> *mut std::ffi::c_void;
    fn free(ptr: *mut std::ffi::c_void);
}
unsafe fn libc_malloc(size: usize) -> *mut std::ffi::c_void {
    unsafe { malloc(size) }
}
unsafe fn libc_free(ptr: *mut std::ffi::c_void) {
    unsafe { free(ptr) }
}

// ---------------------------------------------------------------------------
// Latency + tail callbacks
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_get_latency<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*(ctx as *mut Vst3Instance<P>);
        inst.plugin.latency()
    }
}

unsafe extern "C" fn cb_get_tail<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*(ctx as *mut Vst3Instance<P>);
        inst.plugin.tail()
    }
}

// ---------------------------------------------------------------------------
// Output event callbacks
// ---------------------------------------------------------------------------

/// Map a truce `Event` body to a 3-byte VST3 MIDI packet. Returns
/// `None` for event types that don't fit (MIDI 2.0, ParamChange,
/// Transport, etc.). The output count and the index→event lookup
/// share this filter so unsupported events are skipped cleanly
/// rather than emitted as a zeroed packet (which earlier hosts
/// interpreted as a `note 0` Note-Off).
fn try_encode_vst3_midi(event: &Event) -> Option<Vst3MidiEvent> {
    let (status, data1, data2) = match &event.body {
        EventBody::NoteOn {
            channel,
            note,
            velocity,
        } => (0x90 | (channel & 0x0F), *note, (*velocity * 127.0) as u8),
        EventBody::NoteOff {
            channel,
            note,
            velocity,
        } => (0x80 | (channel & 0x0F), *note, (*velocity * 127.0) as u8),
        EventBody::ControlChange { channel, cc, value } => (
            0xB0 | (channel & 0x0F),
            *cc,
            (value.clamp(0.0, 1.0) * 127.0) as u8,
        ),
        EventBody::Aftertouch {
            channel,
            note,
            pressure,
        } => (
            0xA0 | (channel & 0x0F),
            *note,
            (pressure.clamp(0.0, 1.0) * 127.0) as u8,
        ),
        EventBody::ChannelPressure { channel, pressure } => (
            0xD0 | (channel & 0x0F),
            (pressure.clamp(0.0, 1.0) * 127.0) as u8,
            0,
        ),
        EventBody::PitchBend { channel, value } => {
            // 14-bit signed [-1, 1] → unsigned 0..16383, 8192 = center.
            let n = ((value.clamp(-1.0, 1.0) + 1.0) * 8191.5).round() as u16;
            (0xE0 | (channel & 0x0F), (n & 0x7F) as u8, ((n >> 7) & 0x7F) as u8)
        }
        EventBody::ProgramChange { channel, program } => (0xC0 | (channel & 0x0F), *program, 0),
        _ => return None,
    };
    Some(Vst3MidiEvent {
        sample_offset: event.sample_offset,
        status,
        data1,
        data2,
        _pad: 0,
    })
}

unsafe extern "C" fn cb_get_output_event_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*(ctx as *mut Vst3Instance<P>);
        inst.output_events
            .iter()
            .filter(|e| try_encode_vst3_midi(e).is_some())
            .count() as u32
    }
}

unsafe extern "C" fn cb_get_output_event<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out: *mut Vst3MidiEvent,
) {
    unsafe {
        let inst = &*(ctx as *mut Vst3Instance<P>);
        // Walk the filtered iterator until we hit the index-th
        // encodable event. Out-of-range index → leave `*out` untouched
        // is the documented contract; `*out` was zero-initialized by
        // the C++ shim before calling. Returning explicitly here makes
        // a future shim regression (forgetting to bounds-check
        // against `cb_get_output_event_count`) fail loudly rather
        // than emit stale stack data.
        match inst
            .output_events
            .iter()
            .filter_map(try_encode_vst3_midi)
            .nth(index as usize)
        {
            Some(packet) => *out = packet,
            None => return,
        }
    }
}

// ---------------------------------------------------------------------------
// GUI callbacks
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_gui_has_editor<P: PluginExport>(ctx: *mut std::ffi::c_void) -> i32 {
    unsafe {
        if ctx.is_null() {
            return 0;
        }
        let inst = &mut *(ctx as *mut Vst3Instance<P>);
        if inst.editor.is_none() {
            inst.editor = inst.plugin.editor();
        }
        if inst.editor.is_some() { 1 } else { 0 }
    }
}

unsafe extern "C" fn cb_gui_get_size<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    w: *mut u32,
    h: *mut u32,
) {
    unsafe {
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
}

unsafe extern "C" fn cb_gui_set_content_scale<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    scale: f64,
) {
    unsafe {
        if ctx.is_null() || !scale.is_finite() || scale <= 0.0 {
            return;
        }
        // Clamp to the same range the GUI cluster's `EditorScale`
        // cell uses. A buggy host passing `f64::MAX` would otherwise
        // propagate to the editor and overflow when the editor
        // multiplies its logical size to physical pixels.
        let scale = scale.clamp(0.25, 8.0);
        let inst = &mut *(ctx as *mut Vst3Instance<P>);
        inst.host_scale = scale;
        if let Some(ref mut editor) = inst.editor {
            editor.set_scale_factor(scale);
        }
    }
}

unsafe extern "C" fn cb_gui_open<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    parent: *mut std::ffi::c_void,
) {
    unsafe {
        let inst = &mut *(ctx as *mut Vst3Instance<P>);
        if let Some(ref mut editor) = inst.editor {
            let params = inst.plugin.params_arc();
            let plugin_ptr = SendPtr::new(&inst.plugin as *const P);
            let ctx_raw = SendPtr::new(ctx);
            let params_for_set = params.clone();
            let params_for_get = params.clone();
            let params_for_plain = params.clone();
            let params_for_fmt = params.clone();
            let params_for_ctx = params.clone();
            let transport_slot = inst.transport_slot.clone();
            let context = PluginContext::from_closures(
                ClosureBridge {
                    begin_edit: Box::new(move |id| {
                        ffi::truce_vst3_begin_edit(ctx_raw.as_ptr() as *mut std::ffi::c_void, id);
                    }),
                    set_param: Box::new(move |id, value| {
                        // Single trait dispatch: same value-then-readback
                        // pattern collapsed via the trait helper. The
                        // post-clamp normalized value is what the host
                        // expects for `IComponentHandler::performEdit`.
                        let norm = params_for_set.set_normalized_returning_normalized(id, value);
                        ffi::truce_vst3_perform_edit(
                            ctx_raw.as_ptr() as *mut std::ffi::c_void,
                            id,
                            norm,
                        );
                    }),
                    end_edit: Box::new(move |id| {
                        ffi::truce_vst3_end_edit(ctx_raw.as_ptr() as *mut std::ffi::c_void, id);
                    }),
                    request_resize: Box::new(|_w, _h| false),
                    get_param: Box::new(move |id| params_for_get.get_normalized(id).unwrap_or(0.0)),
                    get_param_plain: Box::new(move |id| {
                        params_for_plain.get_plain(id).unwrap_or(0.0)
                    }),
                    format_param: Box::new(move |id| {
                        let plain = params_for_fmt.get_plain(id).unwrap_or(0.0);
                        params_for_fmt
                            .format_value(id, plain)
                            .unwrap_or_else(|| format!("{:.1}", plain))
                    }),
                    get_meter: Box::new(move |id| {
                        let plugin = plugin_ptr.get();
                        plugin.get_meter(id)
                    }),
                    get_state: Box::new(move || {
                        let plugin = plugin_ptr.get();
                        plugin.save_state().unwrap_or_default()
                    }),
                    set_state: Box::new(move |data| {
                        let plugin = &mut *(plugin_ptr.as_ptr() as *mut P);
                        plugin.load_state(&data);
                    }),
                    transport: Box::new(move || transport_slot.read()),
                },
                params_for_ctx,
            );
            #[cfg(target_os = "macos")]
            let handle = RawWindowHandle::AppKit(parent);
            #[cfg(target_os = "windows")]
            let handle = RawWindowHandle::Win32(parent);
            #[cfg(target_os = "linux")]
            let handle = RawWindowHandle::X11(parent as u64);

            editor.open(handle, context);
        }
    }
}

unsafe extern "C" fn cb_gui_close<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    unsafe {
        let inst = &mut *(ctx as *mut Vst3Instance<P>);
        if let Some(ref mut editor) = inst.editor {
            editor.close();
        }
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
    truce_core::info::resolve_name_override(VST3_NAME_OVERRIDE, info.name)
}

fn vst3_cid(id: &str) -> [u8; 16] {
    // FNV-1a-128, per http://www.isthe.com/chongo/tech/comp/fnv/.
    // Standard constants — DAWs persist this CID as the plugin's identity in
    // saved sessions, so the algorithm and constants must stay stable across
    // releases. (The pre-2026-05-03 implementation used mangled offset/prime
    // bytes that produced a deterministic but non-FNV hash with long zero
    // runs in the multiplier; sessions saved against a truce-built plugin
    // before that fix will see a different CID and need to re-bind.)
    const FNV_OFFSET_BASIS: u128 = 0x6C62272E07BB014262B821756295C58D;
    const FNV_PRIME: u128 = 0x0000000001000000000000000000013B;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in id.bytes() {
        hash ^= byte as u128;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash.to_le_bytes()
}

pub fn register_vst3<P: PluginExport>() {
    let info = P::info();
    let instance = P::create();
    let param_infos = instance.params().param_infos();

    let mut param_descs: Vec<Vst3ParamDescriptor> = Vec::with_capacity(param_infos.len());
    for pi in &param_infos {
        let cs = truce_core::wrapper::ParamCStrings::from_info(pi);

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
            name: cs.name.into_raw(),
            short_name: cs.short_name.into_raw(),
            units: cs.unit.into_raw(),
            min: pi.range.min(),
            max: pi.range.max(),
            default_normalized: pi.range.normalize(pi.default_plain),
            step_count: pi.range.step_count() as i32,
            flags,
            group: cs.group.into_raw(),
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
        num_inputs: truce_core::wrapper::default_io_channels::<P>().0,
        num_outputs: truce_core::wrapper::default_io_channels::<P>().1,
    }));

    let callbacks = Box::leak(Box::new(Vst3Callbacks {
        create: cb_create::<P>,
        destroy: cb_destroy::<P>,
        reset: cb_reset::<P>,
        process: cb_process::<P>,
        param_count: cb_param_count::<P>,
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

    // Unify with the `Box::leak(Box::new(...))` shape above so every
    // descriptor handed to `truce_vst3_register` lives behind the
    // same kind of leaked allocation. `Vec::leak` produces a
    // `&'static mut [T]` from a heap reallocation that may differ in
    // capacity from len; converting through `into_boxed_slice()`
    // first trims to exact len and lets us route through `Box::leak`
    // alongside `descriptor` and `callbacks`.
    let param_descs: &'static [Vst3ParamDescriptor] = Box::leak(param_descs.into_boxed_slice());

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

            #[unsafe(no_mangle)]
            pub extern "C" fn truce_vst3_init() {
                ::truce_vst3::register_vst3::<$plugin_type>();
            }

            #[unsafe(no_mangle)]
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
            #[unsafe(no_mangle)]
            #[allow(non_snake_case)]
            pub extern "system" fn BundleEntry(_: *mut ::std::ffi::c_void) -> bool {
                true
            }

            #[cfg(target_os = "macos")]
            #[unsafe(no_mangle)]
            pub extern "system" fn bundleEntry(_: *mut ::std::ffi::c_void) -> bool {
                true
            }

            #[cfg(target_os = "macos")]
            #[unsafe(no_mangle)]
            #[allow(non_snake_case)]
            pub extern "system" fn BundleExit() -> bool {
                true
            }

            #[cfg(target_os = "macos")]
            #[unsafe(no_mangle)]
            pub extern "system" fn bundleExit() -> bool {
                true
            }

            #[cfg(target_os = "linux")]
            #[unsafe(no_mangle)]
            #[allow(non_snake_case)]
            pub extern "system" fn ModuleEntry(_: *mut ::std::ffi::c_void) -> bool {
                true
            }

            #[cfg(target_os = "linux")]
            #[unsafe(no_mangle)]
            #[allow(non_snake_case)]
            pub extern "system" fn ModuleExit() -> bool {
                true
            }

            #[cfg(target_os = "windows")]
            #[unsafe(no_mangle)]
            #[allow(non_snake_case)]
            pub extern "system" fn InitDll() -> bool {
                true
            }

            #[cfg(target_os = "windows")]
            #[unsafe(no_mangle)]
            #[allow(non_snake_case)]
            pub extern "system" fn ExitDll() -> bool {
                true
            }
        }
    };
}
