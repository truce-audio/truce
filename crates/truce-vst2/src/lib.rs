//! VST2 format wrapper for truce.
//!
//! Uses a C shim that implements the `AEffect` interface. The shim calls
//! back into Rust for all plugin logic via C FFI. Clean-room
//! implementation - no Steinberg SDK headers.

pub mod ffi;

use std::ffi::CString;
use std::os::raw::c_char;
use std::slice;

use truce_core::Float;
use truce_core::bus::BusLayout;
use truce_core::cast::{len_u32, sample_pos_i64};
use truce_core::chunked_process::{ChunkedProcess, process_chunked};
use truce_core::editor::{ClosureBridge, Editor, PluginContext, RawWindowHandle, SendPtr};
use truce_core::events::{EVENT_LIST_PREALLOC, Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::midi::decode_short_message;
use truce_core::state;
use truce_core::wrapper::{
    default_io_channels, first_bus_layout, log_midi_ports_clamped, log_missing_bus_layout,
    run_audio_block, run_extern_callback_with, run_register,
};
use truce_params::{ParamFlags, ParamInfo, Params};

use ffi::{Vst2Callbacks, Vst2MidiEvent, Vst2ParamDescriptor, Vst2PluginDescriptor};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

// ---------------------------------------------------------------------------
// Instance wrapper
// ---------------------------------------------------------------------------

/// Bounded handoff slot for state loads. Capacity 1: presets don't
/// arrive faster than the audio thread completes a block, and on
/// overflow we want most-recent-wins (`force_push`) so a rapid
/// double-recall doesn't get the audio thread to apply a stale state
/// after the host already moved on.
type StateLoadQueue = crossbeam_queue::ArrayQueue<state::DeserializedState>;

struct Vst2Instance<P: PluginExport> {
    plugin: P,
    /// Stable handle to the params Arc, set once at instance creation.
    /// Host-thread callbacks (`cb_param_*`, `cb_state_save`) read params
    /// through this handle so they never form a `&inst.plugin`
    /// reference. Params are atomic-backed and `Sync`.
    params_arc: Arc<P::Params>,
    /// Atomic snapshots of the plugin's most recent `latency()` /
    /// `tail()`. Updated by the audio thread (or `cb_reset`).
    latency_cache: AtomicU32,
    tail_cache: AtomicU32,
    event_list: EventList,
    /// Set when `cb_push_sysex_input` has queued `SysEx` for the
    /// upcoming `process` block. `SysEx` input arrives through that
    /// separate callback (during `effProcessEvents`) before `process`
    /// runs, so `process` must not blindly clear `event_list` or it
    /// wipes the queued `SysEx`. The first push of a block clears +
    /// sets this; `process` consumes it instead of re-clearing.
    sysex_inputs_pending: bool,
    output_events: EventList,
    /// Per-sub-block scratch for `chunked_process::process_chunked`.
    sub_event_scratch: EventList,
    /// Cached param-info table for the chunker's split predicate.
    param_infos: Vec<ParamInfo>,
    /// `min_subblock_samples` from `truce.toml`'s `[automation]`.
    min_subblock_samples: u32,
    plugin_id_hash: u64,
    sample_rate: f64,
    /// Max block size declared by the host via `effSetBlockSize` /
    /// `effOpen` (delivered through `cb_reset`'s `max_frames`). A
    /// generous default keeps the contract assert in `cb_process`
    /// from tripping for hosts that send process before declaring a
    /// max.
    max_block_size: usize,
    /// `true` once `cb_reset` has run. `cb_process` early-returns and
    /// zeros outputs while false so DSP doesn't run with un-snapped
    /// smoothers / unset sample rate.
    prepared: bool,
    /// Reused per-block scratch for `RawBufferScratch::build`. Lives
    /// on the instance so the audio thread doesn't heap-allocate.
    ///
    /// Parameterised by `P::Sample`; widens/narrows host-`f32`
    /// buffers around `plugin.process()` for plugins on `prelude64`.
    scratch: truce_core::buffer::RawBufferScratch<<P as truce_core::plugin::PluginRuntime>::Sample>,
    editor: Option<Box<dyn Editor>>,
    /// `AEffect` pointer, set by the C shim after creation. Used for host callbacks.
    aeffect_ptr: *mut std::ffi::c_void,
    /// Whether state has been loaded at least once (via effSetChunk).
    state_loaded: bool,
    /// Buffered parent window handle when editor open arrives before state load.
    pending_editor_parent: Option<*mut std::ffi::c_void>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    transport_slot: Arc<truce_core::TransportSlot>,
    /// Bounded SPSC handoff for state loads. Host (`cb_state_load`)
    /// and editor (`set_state` callback) deserialize on their thread
    /// and push the result; the audio thread pops at the top of
    /// `cb_process` and calls [`state::apply_state`]
    /// under its exclusive `&mut plugin`.
    pending_state: Arc<StateLoadQueue>,
}

// SAFETY: `Vst2Instance` holds two raw `*mut c_void` host handles
// (`aeffect_ptr` and the optional `pending_editor_parent`). Neither
// auto-derives `Send` and `*mut` makes the whole struct `!Send` by
// default. VST2 hosts call every dispatcher / callback on a single
// host thread per instance - never concurrently and never from the
// audio thread for editor-state pointers. The two pointers are read
// only inside `unsafe extern "C"` callbacks the host invokes
// sequentially. This impl asserts the single-thread invariant
// explicitly so a future `Mutex<Box<Vst2Instance<P>>>` (or any other
// generic store that requires `Send`) compiles instead of failing
// silently at the bound.
unsafe impl<P: PluginExport> Send for Vst2Instance<P> {}

unsafe extern "C" {
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
        // Default-init hosts hand us `tempo == 0.0`, which downstream
        // consumers (LFOs synced to BPM, beat-grid math) divide
        // through. Fall back to 120 BPM, matching CLAP's
        // `build_transport_info` default and the snapshot helpers
        // in `truce-core::TransportInfo::for_screenshot`.
        let tempo = if self.tempo > 0.0 { self.tempo } else { 120.0 };
        // The two `as u8` casts are post-clamped to `0..=255`; the
        // truncation lint is impossible to trip but the lint can't
        // see the `clamp`.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (time_sig_num, time_sig_den) = (
            self.time_sig_num.clamp(0, i32::from(u8::MAX)) as u8,
            self.time_sig_den.clamp(0, i32::from(u8::MAX)) as u8,
        );
        TransportInfo {
            playing: self.playing != 0,
            recording: self.recording != 0,
            tempo,
            time_sig_num,
            time_sig_den,
            position_samples: sample_pos_i64(self.position_samples),
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
// Intentional leaks
//
// Every `CString::into_raw()` and the `std::mem::forget(boxed)` for
// per-param descriptor strings (name / unit / group, plugin name +
// vendor) hands a `*const c_char` to a `Vst2{Plugin,Param}Descriptor`
// that the VST2 host caches for the process lifetime. Hosts re-read
// these pointers on demand (display, parameter dialogs, automation)
// with no callback to signal "you may free this now". Freeing is
// therefore unsound.
//
// The leak is bounded: O(plugin_count × (param_count + a few strings))
// per process, allocated once at registration time. No leak per audio
// callback, per render, per editor open. VST2 dylibs get unloaded with
// the host process, which reclaims the allocation.
//
// `Box::into_raw(boxed_instance)` in `cb_create` follows the same
// pattern but is *paired* with `cb_destroy` reconstituting the Box -
// so it isn't a leak, just a C-lifetime handoff.
//
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
    let param_infos = plugin.params().param_infos();
    let params_arc = plugin.params_arc();
    let latency_cache = AtomicU32::new(plugin.latency());
    let tail_cache = AtomicU32::new(plugin.tail());
    let instance = Box::new(Vst2Instance::<P> {
        plugin,
        params_arc,
        latency_cache,
        tail_cache,
        event_list: EventList::with_capacity(EVENT_LIST_PREALLOC),
        sysex_inputs_pending: false,
        output_events: EventList::with_capacity(EVENT_LIST_PREALLOC),
        sub_event_scratch: EventList::with_capacity(EVENT_LIST_PREALLOC),
        param_infos,
        min_subblock_samples: info.automation.min_subblock_samples,
        plugin_id_hash: state::shared_plugin_state_hash(&info),
        sample_rate: 44100.0,
        // 8192 covers the largest block sizes mainstream DAWs use; a
        // non-zero default keeps the process-before-prepared path
        // from tripping the contract assert.
        max_block_size: 8192,
        prepared: false,
        scratch: truce_core::buffer::RawBufferScratch::default(),
        editor: None,
        aeffect_ptr: std::ptr::null_mut(),
        state_loaded: false,
        pending_editor_parent: None,
        transport_slot: truce_core::TransportSlot::new(),
        pending_state: Arc::new(StateLoadQueue::new(1)),
    });
    Box::into_raw(instance).cast::<std::ffi::c_void>()
}

unsafe extern "C" fn cb_destroy<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    unsafe {
        if !ctx.is_null() {
            drop(Box::from_raw(ctx.cast::<Vst2Instance<P>>()));
        }
    }
}

unsafe extern "C" fn cb_reset<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_rate: f64,
    max_frames: u32,
) {
    unsafe {
        let inst = &mut *ctx.cast::<Vst2Instance<P>>();
        // Clamp host-supplied max_frames to a sane minimum: hosts
        // that ignore their own setBlockSize contract can pass 0
        // here, which would size plugin-internal delay lines to zero
        // and blow up on the first non-zero process() call.
        let max_frames = (max_frames as usize).max(1024);
        inst.sample_rate = sample_rate;
        inst.max_block_size = max_frames;
        let (num_in, num_out) = default_io_channels::<P>().unwrap_or((2, 2));
        inst.scratch
            .ensure_capacity(num_in as usize, num_out as usize, max_frames);
        inst.plugin.reset(sample_rate, max_frames);
        inst.plugin.params().set_sample_rate(sample_rate);
        inst.plugin.params().snap_smoothers();
        inst.latency_cache
            .store(inst.plugin.latency(), Ordering::Relaxed);
        inst.tail_cache.store(inst.plugin.tail(), Ordering::Relaxed);
        inst.prepared = true;

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
    let nf = num_frames as usize;
    let ok = run_audio_block::<P>("VST2", || unsafe {
        let inst = &mut *ctx.cast::<Vst2Instance<P>>();
        let num_frames = num_frames as usize;

        // Host called process() before effMainsChanged(true) - sample
        // rate and smoothers haven't been primed yet. Zero outputs
        // and bail rather than running DSP through uninitialized state.
        if !inst.prepared {
            for ch in 0..num_output_channels as usize {
                let ptr = *outputs.add(ch);
                if !ptr.is_null() {
                    std::ptr::write_bytes(ptr, 0, num_frames);
                }
            }
            inst.event_list.clear();
            inst.sysex_inputs_pending = false;
            return;
        }

        // Apply any pending state-load before per-block work so the
        // plugin sees consistent params and extra state for the
        // entire block. See `pending_state` field comment for the
        // queue-overflow policy.
        if let Some(state) = inst.pending_state.pop() {
            state::apply_state(&mut inst.plugin, &state);
        }

        // Convert MIDI events. SysEx input arrives through
        // `cb_push_sysex_input` during the host's `effProcessEvents`
        // dispatch, which runs before this callback, so preserve any
        // queued SysEx instead of clearing it; otherwise clear stale
        // events from the previous block before appending short MIDI.
        if inst.sysex_inputs_pending {
            inst.sysex_inputs_pending = false;
        } else {
            inst.event_list.clear();
        }
        if !events.is_null() && num_events > 0 {
            let event_slice = slice::from_raw_parts(events, num_events as usize);
            for ev in event_slice {
                if let Some(body) = decode_short_message(ev.status, ev.data1, ev.data2) {
                    inst.event_list.push(Event {
                        sample_offset: ev.delta_frames,
                        port: 0,
                        body,
                    });
                }
            }
        }
        inst.event_list.sort();

        // Build AudioBuffer from raw pointers, reusing the per-instance scratch.
        debug_assert!(
            num_frames <= inst.max_block_size,
            "host violated VST2 contract: process() got {num_frames} frames \
             but effSetBlockSize/effOpen declared max {}",
            inst.max_block_size
        );
        let mut audio_buffer = inst.scratch.build(
            inputs,
            outputs,
            num_input_channels,
            num_output_channels,
            len_u32(num_frames),
            P::supports_in_place(),
        );

        let transport = if inst.aeffect_ptr.is_null() {
            TransportInfo::default()
        } else {
            let mut snap = Vst2TransportSnapshot::default();
            truce_vst2_host_get_time(inst.aeffect_ptr, &raw mut snap);
            if snap.valid != 0 {
                snap.to_transport_info()
            } else {
                TransportInfo::default()
            }
        };
        inst.output_events.clear();
        inst.transport_slot.write(&transport);

        let mut transport_snap = transport;
        let chunk_args = ChunkedProcess {
            events: &inst.event_list,
            sub_event_scratch: &mut inst.sub_event_scratch,
            transport: &mut transport_snap,
            sample_rate: inst.sample_rate,
            output_events: &mut inst.output_events,
            params_fn: None,
            meters_fn: None,
            param_infos: &inst.param_infos,
            min_subblock_samples: inst.min_subblock_samples,
        };
        process_chunked(
            &mut inst.plugin,
            inst.params_arc.as_ref() as &dyn Params,
            &mut audio_buffer,
            chunk_args,
        );
        let _ = audio_buffer;
        // Narrow rendered f64 output back to host f32 when needed.
        // No-op for `f32` plugins.
        inst.scratch
            .finish_widening_f32(outputs, num_output_channels, len_u32(num_frames));
        notify_process_param_changes(inst);

        // Refresh latency / tail caches so the host's main-thread
        // queries don't have to call into `inst.plugin`.
        inst.latency_cache
            .store(inst.plugin.latency(), Ordering::Relaxed);
        inst.tail_cache.store(inst.plugin.tail(), Ordering::Relaxed);
    });
    if !ok {
        unsafe {
            for ch in 0..num_output_channels as usize {
                let ptr = *outputs.add(ch);
                if !ptr.is_null() {
                    std::ptr::write_bytes(ptr, 0, nf);
                }
            }
        }
    }
}

fn notify_process_param_changes<P: PluginExport>(inst: &Vst2Instance<P>) {
    if inst.aeffect_ptr.is_null() {
        return;
    }

    for event in inst.output_events.iter() {
        let EventBody::ParamChange { id, value } = event.body else {
            continue;
        };
        let Some(info) = inst.param_infos.iter().find(|info| info.id == id) else {
            continue;
        };

        let normalized = f32::from_f64(info.range.normalize(value));
        unsafe {
            truce_vst2_host_automate(inst.aeffect_ptr, id, normalized);
        }
    }
}

/// Map a truce `Event` body to a 3-byte VST2 MIDI packet. Returns
/// `None` for event types that don't fit (MIDI 2.0, `ParamChange`,
/// Transport, etc.).
fn try_encode_vst2_midi(event: &Event) -> Option<Vst2MidiEvent> {
    use truce_core::midi::pitch_bend_to_bytes;
    let (status, data1, data2) = match &event.body {
        EventBody::NoteOn {
            channel,
            note,
            velocity,
            ..
        } => (0x90 | (channel & 0x0F), *note, *velocity),
        EventBody::NoteOff {
            channel,
            note,
            velocity,
            ..
        } => (0x80 | (channel & 0x0F), *note, *velocity),
        EventBody::ControlChange {
            channel, cc, value, ..
        } => (0xB0 | (channel & 0x0F), *cc, *value),
        EventBody::Aftertouch {
            channel,
            note,
            pressure,
            ..
        } => (0xA0 | (channel & 0x0F), *note, *pressure),
        EventBody::ChannelPressure {
            channel, pressure, ..
        } => (0xD0 | (channel & 0x0F), *pressure, 0),
        EventBody::PitchBend { channel, value, .. } => {
            let (lsb, msb) = pitch_bend_to_bytes(*value);
            (0xE0 | (channel & 0x0F), lsb, msb)
        }
        EventBody::ProgramChange {
            channel, program, ..
        } => (0xC0 | (channel & 0x0F), *program, 0),
        _ => return None,
    };
    Some(Vst2MidiEvent {
        delta_frames: event.sample_offset,
        status,
        data1,
        data2,
        _pad: 0,
    })
}

unsafe extern "C" fn cb_output_event_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<Vst2Instance<P>>();
        len_u32(
            inst.output_events
                .iter()
                .filter(|e| try_encode_vst2_midi(e).is_some())
                .count(),
        )
    }
}

unsafe extern "C" fn cb_output_event_at<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out: *mut Vst2MidiEvent,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst2Instance<P>>();
        if let Some(packet) = inst
            .output_events
            .iter()
            .filter_map(try_encode_vst2_midi)
            .nth(index as usize)
        {
            *out = packet;
        }
    }
}

// `SysEx` input - shim hands us the byte pointer + length; we copy
// into the plug-in's `EventList` pool. Pool-full failures drop the
// message (atomic-by-spec; truncating corrupts).
unsafe extern "C" fn cb_push_sysex_input<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    delta_frames: u32,
    bytes: *const u8,
    len: u32,
) {
    unsafe {
        let inst = &mut *ctx.cast::<Vst2Instance<P>>();
        if bytes.is_null() || len == 0 {
            return;
        }
        // First SysEx of the block clears the previous block's events
        // and flags `process` to keep what we queue here rather than
        // clearing again.
        if !inst.sysex_inputs_pending {
            inst.event_list.clear();
            inst.sysex_inputs_pending = true;
        }
        let slice = std::slice::from_raw_parts(bytes, len as usize);
        let _ = inst.event_list.push_sysex(delta_frames, slice);
    }
}

unsafe extern "C" fn cb_output_sysex_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<Vst2Instance<P>>();
        len_u32(
            inst.output_events
                .iter()
                .filter(|e| matches!(e.body, EventBody::SysEx { .. }))
                .count(),
        )
    }
}

unsafe extern "C" fn cb_output_sysex_at<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out_delta_frames: *mut u32,
    out_bytes: *mut *const u8,
    out_len: *mut u32,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst2Instance<P>>();
        if let Some(event) = inst
            .output_events
            .iter()
            .filter(|e| matches!(e.body, EventBody::SysEx { .. }))
            .nth(index as usize)
        {
            let bytes = inst.output_events.sysex_bytes(&event.body);
            *out_delta_frames = event.sample_offset;
            *out_bytes = bytes.as_ptr();
            *out_len = len_u32(bytes.len());
        }
    }
}

unsafe extern "C" fn cb_param_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<Vst2Instance<P>>();
        len_u32(inst.params_arc.count())
    }
}

unsafe extern "C" fn cb_param_get_normalized<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
) -> f64 {
    unsafe {
        let inst = &*ctx.cast::<Vst2Instance<P>>();
        inst.params_arc.get_normalized(id).unwrap_or(0.0)
    }
}

unsafe extern "C" fn cb_param_set_normalized<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst2Instance<P>>();
        inst.params_arc.set_normalized(id, value);
    }
}

unsafe extern "C" fn cb_param_format_current<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    out: *mut c_char,
    out_len: u32,
) -> u32 {
    unsafe {
        // `out_len == 0` underflows on `out_len as usize - 1`;
        // `copy_nonoverlapping` would then write the full formatted
        // string into a buffer the host claimed had zero capacity.
        if out_len == 0 || out.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<Vst2Instance<P>>();
        let Some(plain) = inst.params_arc.get_plain(id) else {
            return 0;
        };
        match inst.params_arc.format_value(id, plain) {
            Some(text) => {
                let bytes = text.as_bytes();
                let len = bytes.len().min((out_len as usize) - 1);
                std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out, len);
                *out.add(len) = 0;
                len_u32(len)
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
    // Pre-zero the out pointers so a panic anywhere in the body below
    // leaves the host seeing an empty blob rather than a stale buffer
    // pointer paired with whatever length was last written.
    unsafe {
        *out_data = std::ptr::null_mut();
        *out_len = 0;
    }
    run_extern_callback_with::<P, ()>("vst2", "save_state", (), || unsafe {
        let inst = &*ctx.cast::<Vst2Instance<P>>();
        let (ids, values) = inst.params_arc.collect_values();
        // `plugin.save_state()` reads through the plugin reference: a
        // user impl that mutates non-atomic state from `process` while
        // also reading it from `save_state` races here. The contract
        // is "save_state must be safe to call concurrently with
        // process"; impls that copy from atomic params are fine.
        let extra = inst.plugin.save_state();
        let blob = state::serialize_state(inst.plugin_id_hash, &ids, &values, &extra);

        let len = blob.len();
        // Hand the C shim a heap-allocated buffer it'll later return
        // via `cb_state_free`, which reconstitutes a `Vec` with
        // `Vec::from_raw_parts(data, len, len)`. That symmetry has
        // two preconditions:
        //   1. Both sides must use the **same allocator** (the Rust
        //      global allocator here on save → the same global on
        //      `cb_state_free`). VST3 / AU split state alloc and free
        //      across `libc_malloc` / Rust; this code path must NOT.
        //   2. `cap == len`, which `Box<[T]>` guarantees by definition
        //      (a boxed slice has no capacity bookkeeping). Avoid
        //      replacing this with `blob.into_raw_parts()` etc.
        let mut boxed = blob.into_boxed_slice();
        let ptr = boxed.as_mut_ptr();
        std::mem::forget(boxed);
        *out_data = ptr;
        *out_len = len_u32(len);
    });
}

unsafe extern "C" fn cb_state_load<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    data: *const u8,
    len: u32,
) {
    run_extern_callback_with::<P, ()>("vst2", "load_state", (), || unsafe {
        let inst = &mut *ctx.cast::<Vst2Instance<P>>();

        // `slice::from_raw_parts(null, 0)` is sound but `from_raw_parts(null, n)`
        // for `n > 0` is UB. Hosts under stress (or buggy hosts) have
        // been seen to call effSetChunk with `(null, non_zero)`; treat
        // it the same as "host gave us nothing" rather than UB.
        // Deserialize on the host's main thread, hand the result to
        // the audio thread for application via `pending_state`. The
        // `restored` flag below tracks deserialize success (host-side
        // data integrity) and drives the editor-open state machine
        // exactly as before - the audio thread will catch up on its
        // next process block.
        let restored = if !data.is_null() && len > 0 {
            let blob = slice::from_raw_parts(data, len as usize);
            if let Some(deserialized) = state::deserialize_state(blob, inst.plugin_id_hash) {
                // Apply params synchronously so host-thread reads of
                // `effGetParameter` after `effSetChunk` see the
                // restored values without waiting for `cb_process`
                // to pop the queue.
                state::apply_params(&*inst.params_arc, &deserialized);
                let _ = inst.pending_state.force_push(deserialized);
                true
            } else {
                false
            }
        } else {
            false
        };

        // Single ordered block - read once on each side instead of
        // checking `inst.editor` and `inst.pending_editor_parent` in
        // separate `if let` arms. Two arms could let a pending-parent
        // open path land out of order with the state_changed
        // notification; the match collapses both outcomes into one
        // decision tree.
        match (
            restored,
            inst.editor.is_some(),
            inst.pending_editor_parent.take(),
        ) {
            // Editor already open + valid state: notify in place.
            (true, true, None) => {
                if let Some(ref mut editor) = inst.editor {
                    editor.state_changed();
                }
            }
            // Pending editor + (any restore outcome): open the editor;
            // construction reads the just-restored params, so a
            // separate `state_changed` would double-fire.
            (_, _, Some(parent)) => {
                inst.state_loaded = true;
                open_editor_inner(inst, parent);
                return;
            }
            // No editor + restore failed / null buffer: nothing to notify.
            _ => {}
        }

        inst.state_loaded = true;
    });
}

/// Free a state blob handed out by [`cb_state_save`].
///
/// **Contract:** `data` must point to memory allocated via the Rust
/// global allocator with `cap == len`. `cb_state_save` upholds this
/// via `Vec::into_boxed_slice` (which trims `cap` to `len`) then
/// `mem::forget`. `Vec::from_raw_parts` requires the allocator and
/// `cap` to match exactly, so any change to the allocation strategy
/// on the save side must update this free side in lock-step.
unsafe extern "C" fn cb_state_free(data: *mut u8, len: u32) {
    unsafe {
        if !data.is_null() && len > 0 {
            // Reconstruct as a Vec (not a Box) because the original
            // allocation came from `Vec::into_boxed_slice` and Box's
            // free path expects different layout metadata.
            #[allow(clippy::same_length_and_capacity)]
            let v = Vec::from_raw_parts(data, len as usize, len as usize);
            drop(v);
        }
    }
}

// ---------------------------------------------------------------------------
// Latency + tail
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_get_latency<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<Vst2Instance<P>>();
        inst.latency_cache.load(Ordering::Relaxed)
    }
}

unsafe extern "C" fn cb_get_tail<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<Vst2Instance<P>>();
        inst.tail_cache.load(Ordering::Relaxed)
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
        let inst = &mut *ctx.cast::<Vst2Instance<P>>();
        if inst.editor.is_none() {
            inst.editor = inst.plugin.editor();
        }
        i32::from(inst.editor.is_some())
    }
}

unsafe extern "C" fn cb_gui_get_size<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    w: *mut u32,
    h: *mut u32,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst2Instance<P>>();
        if let Some(ref editor) = inst.editor {
            // VST2 has no standardised DPI channel - hosts read back
            // whatever `effEditGetRect` returns and embed the NSView /
            // HWND at that pixel size. Report the editor's logical size
            // unchanged; hosts on Retina macOS will scale through AppKit
            // and Windows VST2 plugins have never been HiDPI-aware.
            let (ew, eh) = editor.size();
            *w = ew;
            *h = eh;
        }
    }
}

unsafe extern "C" fn cb_set_effect_ptr<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    effect: *mut std::ffi::c_void,
) {
    unsafe {
        let inst = &mut *ctx.cast::<Vst2Instance<P>>();
        inst.aeffect_ptr = effect;
    }
}

/// Actually open the editor with the given parent window handle.
unsafe fn open_editor_inner<P: PluginExport>(
    inst: &mut Vst2Instance<P>,
    parent: *mut std::ffi::c_void,
) {
    unsafe {
        if let Some(ref mut editor) = inst.editor {
            let params = inst.plugin.params_arc();
            let plugin_ptr = SendPtr::new(&raw const inst.plugin);
            let effect_ptr = SendPtr::new(inst.aeffect_ptr);
            let params_for_set = params.clone();
            let params_for_get = params.clone();
            let params_for_plain = params.clone();
            let params_for_fmt = params.clone();
            let params_for_ctx = params.clone();
            let pending_state_for_set = inst.pending_state.clone();
            let transport_slot = inst.transport_slot.clone();
            let context = PluginContext::from_closures(
                ClosureBridge {
                    begin_edit: Box::new(move |id| {
                        if !effect_ptr.as_ptr().is_null() {
                            truce_vst2_host_begin_edit(effect_ptr.as_ptr().cast_mut(), id);
                        }
                    }),
                    set_param: Box::new(move |id, value| {
                        let norm = f32::from_f64(
                            params_for_set.set_normalized_returning_normalized(id, value),
                        );
                        if !effect_ptr.as_ptr().is_null() {
                            truce_vst2_host_automate(effect_ptr.as_ptr().cast_mut(), id, norm);
                        }
                    }),
                    end_edit: Box::new(move |id| {
                        if !effect_ptr.as_ptr().is_null() {
                            truce_vst2_host_end_edit(effect_ptr.as_ptr().cast_mut(), id);
                        }
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
                            .unwrap_or_else(|| format!("{plain:.1}"))
                    }),
                    get_meter: Box::new(move |id| {
                        let plugin = plugin_ptr.get();
                        plugin.get_meter(id)
                    }),
                    get_state: Box::new(move || {
                        let plugin = plugin_ptr.get();
                        plugin.save_state()
                    }),
                    set_state: Box::new(move |bytes| {
                        // The editor sends RAW custom-state bytes - exactly
                        // what `save_state()` emits and `get_state` above
                        // returns - NOT a full `serialize_state` envelope.
                        // Route them to the plugin's `load_state` on the
                        // audio thread via the same handoff queue the host
                        // load path uses (the queue is what avoids aliasing
                        // `process()`'s `&mut plugin`). No params ride along:
                        // the editor mutates params through `set_param`.
                        let _ = pending_state_for_set.force_push(state::DeserializedState {
                            params: Vec::new(),
                            extra: Some(bytes),
                        });
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

unsafe extern "C" fn cb_gui_open<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    parent: *mut std::ffi::c_void,
) {
    // `vst2_shim.c` is the primary owner of the gui_open ordering: on
    // `effEditOpen` it stashes `parent` in its own `deferred_parent`
    // when `state_loaded == 0` and only forwards to this callback
    // *after* `effSetChunk` (or the fresh-instance `effMainsChanged`)
    // bumps `state_loaded`. The Rust-side `pending_editor_parent`
    // path below is a defensive backstop - it covers a hypothetical
    // future caller (e.g. an integration-test driver) that bypasses
    // the C shim and invokes `cb_gui_open` directly. The two paths
    // never race in practice because the C shim's `state_loaded`
    // and the Rust-side `inst.state_loaded` are both flipped along
    // the same `effSetChunk → state_load → gui_open` chain. If
    // either side is ever extracted, this comment names the contract
    // to keep.
    unsafe {
        let inst = &mut *ctx.cast::<Vst2Instance<P>>();
        if inst.state_loaded {
            open_editor_inner(inst, parent);
        } else {
            inst.pending_editor_parent = Some(parent);
        }
    }
}

unsafe extern "C" fn cb_gui_close<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    unsafe {
        let inst = &mut *ctx.cast::<Vst2Instance<P>>();
        if let Some(ref mut editor) = inst.editor {
            editor.close();
        }
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Plugin display-name returned from `effGetEffectName`. Reads
/// `truce.toml`'s `vst2_name` (baked into `PluginInfo` by
/// `truce::plugin_info!`), falling back to `PluginInfo::name`.
fn resolved_plugin_name(info: &truce_core::info::PluginInfo) -> &'static str {
    truce_core::info::resolve_name_override(info.vst2_name, info.name)
}

pub fn register_vst2<P: PluginExport>() {
    // Called from the export macro's `extern "C" fn init()` static
    // initializer. Catch any panic so it doesn't cross the FFI
    // boundary and abort the host process.
    run_register::<P>("VST2", || {
        let Some(layout) = first_bus_layout::<P>() else {
            log_missing_bus_layout::<P>("VST2");
            return;
        };
        register_vst2_inner::<P>(&layout);
    });
}

fn register_vst2_inner<P: PluginExport>(layout: &BusLayout) {
    let info = P::info();

    let name = CString::new(resolved_plugin_name(&info)).unwrap_or_default();
    let vendor = CString::new(info.vendor).unwrap_or_default();

    // Static metadata path: derive emits a `LazyLock`-cached
    // `Vec<ParamInfo>` so registration doesn't need to construct a
    // plugin instance just to read parameter shape. Hand-written
    // `PluginExport` impls without a `Params::param_infos_static`
    // override fall back to the historical
    // `Self::create().params().param_infos()` walk inside the trait
    // default - see `PluginExport::param_infos_static`.
    let infos = P::param_infos_static();
    let bypass_param_id = infos
        .iter()
        .find(|pi| pi.flags.contains(ParamFlags::IS_BYPASS))
        .map_or(u32::MAX, |pi| pi.id);

    // VST2 has a single MIDI stream per direction; clamp a multi-port
    // declaration to one and warn.
    log_midi_ports_clamped("VST2", "input", info.midi_input_ports);
    log_midi_ports_clamped("VST2", "output", info.midi_output_ports);

    let descriptor = Box::leak(Box::new(Vst2PluginDescriptor {
        component_type: info.au_type,
        component_subtype: info.fourcc,
        name: name.into_raw(),
        vendor: vendor.into_raw(),
        version: 1,
        num_inputs: layout.total_input_channels(),
        num_outputs: layout.total_output_channels(),
        bypass_param_id,
        accepts_midi_in: i32::from(info.accepts_midi_in),
        emits_midi: i32::from(info.emits_midi),
    }));

    let callbacks = Box::leak(Box::new(Vst2Callbacks {
        create: cb_create::<P>,
        destroy: cb_destroy::<P>,
        reset: cb_reset::<P>,
        process: cb_process::<P>,
        param_count: cb_param_count::<P>,
        param_get_normalized: cb_param_get_normalized::<P>,
        param_set_normalized: cb_param_set_normalized::<P>,
        param_format_current: cb_param_format_current::<P>,
        output_event_count: cb_output_event_count::<P>,
        output_event_at: cb_output_event_at::<P>,
        push_sysex_input: cb_push_sysex_input::<P>,
        output_sysex_count: cb_output_sysex_count::<P>,
        output_sysex_at: cb_output_sysex_at::<P>,
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

    // Build param descriptors (param_infos was already collected for
    // the bypass-id scan above).
    let mut param_descs: Vec<Vst2ParamDescriptor> = Vec::with_capacity(infos.len());
    for pi in &infos {
        let cs = truce_core::wrapper::ParamCStrings::from_info(pi);
        param_descs.push(Vst2ParamDescriptor {
            id: pi.id,
            name: cs.name.into_raw(),
            min: pi.range.min(),
            max: pi.range.max(),
            default_value: pi.default_plain,
            step_count: pi.range.step_count().map_or(0, std::num::NonZero::get),
            unit: cs.unit.into_raw(),
            group: cs.group.into_raw(),
        });
    }
    let num_params = len_u32(param_descs.len());
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
            #[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
            #[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
            #[cfg_attr(target_os = "windows", unsafe(link_section = ".CRT$XCU"))]
            static INIT: extern "C" fn() = {
                extern "C" fn init() {
                    ::truce_vst2::register_vst2::<$plugin_type>();
                }
                init
            };
        }
    };
}
