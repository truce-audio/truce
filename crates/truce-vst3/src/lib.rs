//! VST3 format wrapper for truce.
//!
//! Uses a C++ shim that implements the real VST3 COM interfaces
//! with correct vtable layout. All plugin logic is delegated to
//! Rust via C FFI callbacks.

pub mod ffi;

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::slice;

use truce_core::TransportSlot;
use truce_core::buffer::RawBufferScratch;
use truce_core::bus::{BusConfig, BusKind, BusLayout};
use truce_core::cast::{len_u32, sample_pos_i64};
use truce_core::chunked_process::{ChunkedProcess, process_chunked};
use truce_core::config::{AudioConfig, ProcessMode};
use truce_core::editor::EditorBuilder;
use truce_core::editor::{
    ClosureBridge, Editor, PluginContext, RawWindowHandle, SendPtr, clamp_logical_size,
    fit_logical_size,
};
use truce_core::events::{EVENT_LIST_PREALLOC, Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::{PluginCategory, PluginInfo, resolve_name_override};
use truce_core::meters::MeterStore;
use truce_core::midi::{
    decode_short_message, denorm_7bit, denorm_pitch_bend, downconvert_to_midi1,
    per_note_bend_from_semitones, per_note_bend_semitones, pitch_bend_to_bytes,
};
use truce_core::plugin::PluginRuntime;
use truce_core::rt::{RtSection, audit};
use truce_core::snapshot::SnapshotSlot;
use truce_core::state;
use truce_core::tasks::AnyTaskSpawner;
use truce_core::wrapper::{
    ParamCStrings, SharedPlugin, default_io_channels, enter_plugin, find_bus_layout,
    log_missing_bus_layout, run_audio_block, run_extern_callback_with, run_register, save_extra,
    shared_plugin,
};
use truce_params::MidiSource;
use truce_params::sample::{Float, Sample};
use truce_params::{ParamFlags, ParamInfo, ParamRange, Params};

use ffi::{Vst3Callbacks, Vst3MidiEvent, Vst3ParamDescriptor, Vst3PluginDescriptor};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Instance wrapper
// ---------------------------------------------------------------------------

/// Bounded handoff slot for state loads. Capacity 1: presets don't
/// arrive faster than the audio thread completes a block, and on
/// overflow we want most-recent-wins (`force_push`) so a rapid
/// double-recall doesn't get the audio thread to apply a stale state
/// after the host already moved on.
type StateLoadQueue = crossbeam_queue::ArrayQueue<state::DeserializedState>;

/// VST3 `RestartFlags::kLatencyChanged`. The audio thread flags this
/// through `truce_vst3_mark_restart` (an atomic bit-set, no host call);
/// the shim drains it via `IComponentHandler::restartComponent` on the
/// next host main-thread callback, so the actual notification lands on
/// the UI thread - never the render thread or a truce-owned thread.
const K_LATENCY_CHANGED: i32 = 8;

struct Vst3Instance<P: PluginExport> {
    /// The plugin in the wrapper-standard ownership cell: the audio
    /// thread owns it per block, host lifecycle callbacks own it while
    /// processing is stopped, and the two never overlap. `cb_state_save`
    /// and the editor's `get_state` read the lock-free snapshot instead,
    /// so they never touch it. See `truce_core::wrapper::SharedPlugin`.
    plugin: SharedPlugin<P>,
    /// Stable handle to the params Arc, set once at instance creation.
    /// Host-thread callbacks (`cb_param_*`) read params through this
    /// handle so a param query never touches the plugin.
    /// Params are atomic-backed and `Sync`.
    params_arc: Arc<P::Params>,
    /// Shared meter storage, set once at instance creation. The
    /// editor's `get_meter` closure reads these atomic slots instead
    /// of the plugin instance.
    meter_store: Arc<MeterStore>,
    /// Lock-free custom-state slot the audio thread publishes
    /// into, read by `save_state` so a snapshot-capable plugin's
    /// save never touches the plugin. Cached on the instance.
    snapshot: Arc<SnapshotSlot>,
    /// Background-task spawner (`None` unless the plugin wired `tasks:`),
    /// cached at creation so the editor schedules without touching the plugin.
    task_spawner: Option<AnyTaskSpawner>,
    /// Lock-free editor factory, cached at creation - building
    /// the editor never touches the plugin (`--shell` rebuilds
    /// from the reloaded dylib, so GUI edits hot-reload).
    editor_builder: EditorBuilder<P::Params>,
    event_list: EventList,
    sysex_inputs_pending: bool,
    output_events: EventList,
    /// Per-sub-block scratch for `chunked_process::process_chunked`.
    /// Pre-allocated to the same capacity as `event_list`.
    sub_event_scratch: EventList,
    /// Full param-info cache for the chunker's `is_chunked(id)`
    /// lookup. Built once at `cb_create`; static for the life of
    /// the instance.
    param_infos: Vec<ParamInfo>,
    /// `min_subblock_samples` from `truce.toml`'s `[automation]`
    /// table. Read at instance construction and passed to
    /// `chunked_process::process_chunked` every block.
    min_subblock_samples: u32,
    plugin_id_hash: u64,
    sample_rate: f64,
    /// Max block size declared by the host in `setupProcessing`.
    /// Used to debug-assert that `cb_process` never receives more
    /// frames than the plugin was sized for. Defaults to a generous
    /// fallback so the contract check stays meaningful even for hosts
    /// that skip `setupProcessing` (e.g. some validator robustness
    /// tests).
    max_block_size: usize,
    /// `true` once `cb_reset` has run (i.e. the host called
    /// `setActive(true)`). Until then, `cb_process` early-returns and
    /// zeros outputs - running DSP before the plugin's smoothers and
    /// per-rate state are primed produces NaN / garbage that the host
    /// then has to clean up. Pluginval's "process before activate"
    /// robustness paths exercise exactly this case.
    prepared: bool,
    /// `true` between `setActive(true)` and `setActive(false)`.
    /// `cb_state_load` reads it to decide whether the audio thread will
    /// drain `pending_state`: if inactive, no `cb_process` runs, so it
    /// applies the custom-state blob synchronously rather than leaving
    /// it stranded (which would let a following `getState` re-serialize
    /// stale extra state). Written only from `cb_set_active` (main
    /// thread); unlike `prepared`, it tracks deactivation too.
    active: AtomicBool,
    /// Reused per-block scratch for `RawBufferScratch::build`.
    /// Lives on the instance so the audio thread doesn't allocate.
    ///
    /// Parameterised by `P::Sample` so plugins on `prelude64` get
    /// the widening-scratch path (host wire is `f32`, plugin DSP is
    /// `f64`) transparently. Same-precision plugins (`prelude32`)
    /// stay zero-copy through the host pointers.
    scratch: RawBufferScratch<<P as PluginRuntime>::Sample>,
    /// Cached `(id, range)` pairs sorted by id. Built once in
    /// `cb_create` from `params().param_infos()`. Hosts call
    /// `cb_param_normalize` / `cb_param_denormalize` extremely often
    /// while reading automation; rebuilding the full `Vec<ParamInfo>`
    /// per call would heap-allocate on a tight host read path. Ranges
    /// are static for the life of the plugin instance, so caching is
    /// safe.
    param_ranges: Vec<(u32, ParamRange)>,
    /// Precomputed MIDI-controller bindings, sorted by param id, for the
    /// audio-thread bridge in `process_block`. Only params with a
    /// `midi_map` appear, so it's empty for the common no-mapping plugin
    /// and the per-change lookup short-circuits (`binary_search` on an
    /// empty slice is `O(1)`).
    midi_maps: Vec<(u32, MidiMap)>,
    editor: Option<Box<dyn Editor>>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    transport_slot: Arc<TransportSlot>,
    /// Content scale reported by the host via
    /// `IPlugViewContentScaleSupport::setContentScaleFactor`. Defaults
    /// to 1.0 when the host never calls it (macOS Cocoa hosts, VST3
    /// runners that don't implement the interface). Used to convert
    /// the editor's logical size to physical pixels when reporting
    /// `getSize` on Windows/Linux.
    host_scale: f64,
    /// Bounded SPSC handoff for state loads. Host (`cb_state_load`)
    /// and editor (`set_state` callback) deserialize on their thread
    /// and push the result; the audio thread pops at the top of
    /// `cb_process` and calls [`state::apply_state`]
    /// under its exclusive `&mut plugin`.
    pending_state: Arc<StateLoadQueue>,
    /// Atomic snapshots of the plugin's most recent `latency()` /
    /// `tail()` reports. Updated by the audio thread (or `cb_reset`)
    /// so host-thread callbacks (`cb_get_latency`, `cb_get_tail`) read
    /// the value without forming a `&Inst.plugin` reference. Initial
    /// value is whatever the plugin reports immediately after `init()`.
    latency_cache: AtomicU32,
    tail_cache: AtomicU32,
    /// Last-seen values of the hidden MIDI proxy params (f64 bits),
    /// indexed by `id - MIDI_PROXY_ID_BASE`. Empty when the plugin
    /// doesn't accept MIDI input. Written by `cb_param_set_value` and
    /// read by `cb_param_get_value` - both host-thread; atomic for
    /// interior mutability through the shared `&Inst` those callbacks
    /// hold.
    midi_proxy_values: Vec<AtomicU64>,
    /// Correlates the host's per-voice `noteId` counters (arbitrary,
    /// assigned at note-on, scoped per event bus) to the
    /// `(channel, note)` pair truce's per-note events address. Written
    /// and read on the audio thread inside `cb_process`; cleared by
    /// `cb_reset` while audio is stopped.
    note_id_map: NoteIdMap,
}

/// Fixed-capacity `(port, noteId) -> (channel, note)` correlation for
/// incoming VST3 note expression. A `noteId` is an arbitrary per-voice
/// counter the host assigns on note-on - and scopes per event bus, so
/// two buses can carry the same id for different voices - meaning
/// expression events can't be decoded without remembering which note
/// each id addresses on which bus. Inline array + linear scan keeps
/// the audio thread alloc-free; 128 slots covers every
/// simultaneously-sounding voice a host realistically drives.
struct NoteIdMap {
    slots: [NoteIdSlot; Self::CAPACITY],
    /// Round-robin overwrite position for when every slot is live.
    cursor: usize,
}

/// One tracked voice. `note_id < 0` marks a free slot (hosts only
/// assign non-negative ids, `-1` means unassigned).
#[derive(Clone, Copy)]
struct NoteIdSlot {
    note_id: i32,
    port: u8,
    channel: u8,
    note: u8,
}

impl NoteIdSlot {
    const FREE: Self = Self {
        note_id: -1,
        port: 0,
        channel: 0,
        note: 0,
    };
}

impl NoteIdMap {
    const CAPACITY: usize = 128;

    fn new() -> Self {
        Self {
            slots: [NoteIdSlot::FREE; Self::CAPACITY],
            cursor: 0,
        }
    }

    /// Track a sounding note. Re-registering a live `(port, id)` pair
    /// updates it in place; when the map is full the oldest slot is
    /// overwritten so a leaked entry can never wedge the map. Entries
    /// deliberately outlive their note-off - hosts keep sending
    /// expression through the release phase - so slots are reclaimed
    /// by overwrite or [`Self::clear`], never by removal.
    fn insert(&mut self, port: u8, note_id: i32, channel: u8, note: u8) {
        if note_id < 0 {
            return;
        }
        let slot = self
            .slots
            .iter()
            .position(|s| s.note_id == note_id && s.port == port)
            .or_else(|| self.slots.iter().position(|s| s.note_id < 0))
            .unwrap_or_else(|| {
                let c = self.cursor;
                self.cursor = (c + 1) % Self::CAPACITY;
                c
            });
        self.slots[slot] = NoteIdSlot {
            note_id,
            port,
            channel,
            note,
        };
    }

    fn lookup(&self, port: u8, note_id: i32) -> Option<(u8, u8)> {
        if note_id < 0 {
            return None;
        }
        self.slots
            .iter()
            .find(|s| s.note_id == note_id && s.port == port)
            .map(|s| (s.channel, s.note))
    }

    fn clear(&mut self) {
        self.slots = [NoteIdSlot::FREE; Self::CAPACITY];
        self.cursor = 0;
    }
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
    let param_infos: Vec<ParamInfo> = plugin.params().param_infos();
    let mut param_ranges: Vec<(u32, ParamRange)> =
        param_infos.iter().map(|i| (i.id, i.range)).collect();
    // Sort by id so `binary_search_by_key` works in the hot lookups.
    param_ranges.sort_by_key(|(id, _)| *id);
    // Precompute the MIDI-controller bindings, sorted by id, so the
    // audio thread bridges mapped controllers without a linear scan.
    let mut midi_maps: Vec<(u32, MidiMap)> = param_infos
        .iter()
        .filter_map(|i| MidiMap::from_param(i).map(|m| (i.id, m)))
        .collect();
    midi_maps.sort_by_key(|(id, _)| *id);
    let params_arc = plugin.params_arc();
    let meter_store = plugin.meter_store();
    let snapshot = plugin.snapshot_slot();
    let task_spawner = plugin.task_spawner();
    let editor_builder = plugin.editor_builder();
    let latency_cache = AtomicU32::new(plugin.latency());
    let tail_cache = AtomicU32::new(plugin.tail());
    let instance = Box::new(Vst3Instance::<P> {
        plugin: shared_plugin(plugin),
        params_arc,
        meter_store,
        snapshot,
        task_spawner,
        editor_builder,
        event_list: EventList::with_capacity(EVENT_LIST_PREALLOC),
        sysex_inputs_pending: false,
        output_events: EventList::with_capacity(EVENT_LIST_PREALLOC),
        sub_event_scratch: EventList::with_capacity(EVENT_LIST_PREALLOC),
        param_infos,
        min_subblock_samples: info.automation.min_subblock_samples,
        plugin_id_hash: state::shared_plugin_state_hash(&info),
        sample_rate: 44100.0,
        // 8192 covers the largest block sizes mainstream DAWs / validators
        // use (Reaper / pluginval ≤ 4096); a non-zero default keeps the
        // process-before-activate path from tripping the contract assert.
        max_block_size: 8192,
        prepared: false,
        active: AtomicBool::new(false),
        scratch: RawBufferScratch::default(),
        param_ranges,
        midi_maps,
        editor: None,
        transport_slot: TransportSlot::new(),
        host_scale: 1.0,
        pending_state: Arc::new(StateLoadQueue::new(1)),
        latency_cache,
        tail_cache,
        midi_proxy_values: (0..midi_proxy_len::<P>())
            .map(|i| {
                // Bounded by MIDI_PROXY_COUNT.
                #[allow(clippy::cast_possible_truncation)]
                let controller = (i as u32) % MIDI_PROXY_PER_CHANNEL;
                AtomicU64::new(midi_proxy_default(controller).to_bits())
            })
            .collect(),
        note_id_map: NoteIdMap::new(),
    });
    let raw = Box::into_raw(instance);
    raw.cast::<std::ffi::c_void>()
}

unsafe extern "C" fn cb_destroy<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    unsafe {
        if !ctx.is_null() {
            drop(Box::from_raw(ctx.cast::<Vst3Instance<P>>()));
        }
    }
}

/// Map a VST3 `ProcessModes` value (`kRealtime` 0, `kPrefetch` 1,
/// `kOffline` 2) to a truce [`ProcessMode`]. Unknown values fall back
/// to `Realtime`.
fn vst3_process_mode(mode: i32) -> ProcessMode {
    match mode {
        1 => ProcessMode::Buffered,
        2 => ProcessMode::Offline,
        _ => ProcessMode::Realtime,
    }
}

unsafe extern "C" fn cb_reset<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_rate: f64,
    max_frames: u32,
    process_mode: i32,
) {
    // Author `reset` can panic (allocation, DSP prep); firewall it so the
    // panic can't unwind across the C ABI and abort the host.
    run_extern_callback_with::<P, ()>("vst3", "reset", (), || unsafe {
        let inst = &mut *ctx.cast::<Vst3Instance<P>>();
        // Clamp host-supplied max_frames up to a sane minimum: hosts that
        // don't honor their own setupProcessing contract can pass 0 here,
        // which would size plugin-internal delay lines to zero and blow up
        // on the first non-zero process() call.
        let max_frames = (max_frames as usize).max(1024);
        inst.sample_rate = sample_rate;
        inst.max_block_size = max_frames;
        // Grow per-block scratch to cover this layout's channel count
        // and block size before the first process() call so the audio
        // thread stays alloc-free.
        let (num_in, num_out) = default_io_channels::<P>().unwrap_or((2, 2));
        inst.scratch
            .ensure_capacity(num_in as usize, num_out as usize, max_frames);
        {
            let mut plugin = enter_plugin(&inst.plugin);
            let config = AudioConfig::new(sample_rate, max_frames)
                .with_process_mode(vst3_process_mode(process_mode));
            plugin.reset(&config);
            inst.latency_cache
                .store(plugin.latency(), Ordering::Relaxed);
            inst.tail_cache.store(plugin.tail(), Ordering::Relaxed);
        }
        // Voices don't survive a reset; a stale correlation could
        // route new expression to a dead (channel, note).
        inst.note_id_map.clear();
        inst.prepared = true;
    });
}

/// `IComponent::setActive`. Tracks activation so `cb_state_load` knows
/// whether the audio thread will drain the pending-state queue.
unsafe extern "C" fn cb_set_active<P: PluginExport>(ctx: *mut std::ffi::c_void, active: i32) {
    unsafe {
        (*ctx.cast::<Vst3Instance<P>>())
            .active
            .store(active != 0, Ordering::Relaxed);
    }
}

/// The `bus_layouts()` index matching `(in_ch, out_ch)`, or `-1`. The
/// shim's `setBusArrangements` calls this to accept any declared layout.
/// Static per plugin type - no instance context.
unsafe extern "C" fn cb_match_bus_layout<P: PluginExport>(in_ch: u32, out_ch: u32) -> i32 {
    find_bus_layout::<P>(in_ch, out_ch).map_or(-1, |i| i32::try_from(i).unwrap_or(-1))
}

/// Per-bus channel width of a declared layout, for the shim's per-bus
/// `getBusInfo` / `getBusArrangement` and its process-time channel gather.
/// `0` for any out-of-range index.
unsafe extern "C" fn cb_layout_bus_channels<P: PluginExport>(
    layout_index: u32,
    is_output: i32,
    bus_index: u32,
) -> u32 {
    let (Ok(li), Ok(bi)) = (usize::try_from(layout_index), usize::try_from(bus_index)) else {
        return 0;
    };
    let layouts = P::bus_layouts();
    let Some(layout) = layouts.get(li) else {
        return 0;
    };
    let buses = if is_output != 0 {
        &layout.outputs
    } else {
        &layout.inputs
    };
    buses.get(bi).map_or(0, |b| b.channels.channel_count())
}

/// Match a host-proposed per-bus arrangement (arrays of per-bus channel
/// counts) to a declared `bus_layouts()` index, or `-1`. A layout matches
/// when its per-bus widths equal the host's, direction by direction - so
/// a sidechain bus is matched on its own width, not summed into the main.
unsafe extern "C" fn cb_match_bus_layout_perbus<P: PluginExport>(
    in_channels: *const u32,
    num_in: u32,
    out_channels: *const u32,
    num_out: u32,
) -> i32 {
    // SAFETY: the shim passes arrays of the lengths it declares, or null
    // with length 0 for a bus-less direction.
    let ins = unsafe { slice_or_empty(in_channels, num_in) };
    let outs = unsafe { slice_or_empty(out_channels, num_out) };
    let widths_match = |buses: &[BusConfig], want: &[u32]| {
        buses.len() == want.len()
            && buses
                .iter()
                .zip(want)
                .all(|(b, &w)| b.channels.channel_count() == w)
    };
    P::bus_layouts()
        .iter()
        .position(|l| widths_match(&l.inputs, ins) && widths_match(&l.outputs, outs))
        .map_or(-1, |i| i32::try_from(i).unwrap_or(-1))
}

/// `(num_input_buses, num_output_buses, input_kinds_ptr, output_kinds_ptr)`
/// for the descriptor, from the plugin's first declared layout. The
/// kind-byte arrays are leaked to `'static`.
fn descriptor_buses<P: PluginExport>() -> (u32, u32, *const u8, *const u8) {
    let first = P::bus_layouts().into_iter().next().unwrap_or_default();
    let ins = leak_bus_kinds(&first.inputs);
    let outs = leak_bus_kinds(&first.outputs);
    (
        u32::try_from(ins.len()).unwrap_or(0),
        u32::try_from(outs.len()).unwrap_or(0),
        ins.as_ptr(),
        outs.as_ptr(),
    )
}

/// Leak the per-bus kind bytes of a bus list (`0` = Main, `1` = Sidechain)
/// to `'static` for the descriptor's raw pointer.
fn leak_bus_kinds(buses: &[BusConfig]) -> &'static [u8] {
    Box::leak(
        buses
            .iter()
            .map(|b| u8::from(b.kind == BusKind::Sidechain))
            .collect::<Vec<u8>>()
            .into_boxed_slice(),
    )
}

/// Build a slice from a `(ptr, len)` the C++ shim handed us, or an empty
/// slice when the pointer is null (a direction with no buses).
unsafe fn slice_or_empty<'a>(ptr: *const u32, len: u32) -> &'a [u32] {
    match usize::try_from(len) {
        Ok(n) if !ptr.is_null() && n > 0 => unsafe { std::slice::from_raw_parts(ptr, n) },
        _ => &[],
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
    process_mode: i32,
) {
    // SAFETY: forwarded - the shim's contract is the same.
    unsafe {
        process_block::<P, f32>(
            ctx,
            inputs,
            outputs,
            num_input_channels,
            num_output_channels,
            num_frames,
            events,
            num_events,
            transport_ptr,
            param_changes,
            num_param_changes,
            process_mode,
        );
    }
}

/// 64-bit wire twin of [`cb_process`]. The shim routes here when the
/// host negotiated `kSample64` in `setupProcessing` (only offered for
/// `f64` plugins), so an `f64` plugin reads and writes host memory
/// directly with no widen/narrow pass.
unsafe extern "C" fn cb_process_f64<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    inputs: *const *const f64,
    outputs: *mut *mut f64,
    num_input_channels: u32,
    num_output_channels: u32,
    num_frames: u32,
    events: *const Vst3MidiEvent,
    num_events: u32,
    transport_ptr: *const ffi::Vst3Transport,
    param_changes: *const ffi::Vst3ParamChange,
    num_param_changes: u32,
    process_mode: i32,
) {
    // SAFETY: forwarded - the shim's contract is the same.
    unsafe {
        process_block::<P, f64>(
            ctx,
            inputs,
            outputs,
            num_input_channels,
            num_output_channels,
            num_frames,
            events,
            num_events,
            transport_ptr,
            param_changes,
            num_param_changes,
            process_mode,
        );
    }
}

/// Shared body of [`cb_process`] / [`cb_process_f64`], generic over
/// the host wire precision `H`. `RawBufferScratch` zero-copies when
/// `H` matches the plugin's `Sample` and converts through scratch
/// otherwise, so both wires work for both plugin precisions.
// The parameter list mirrors the C ABI callback signature 1:1.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
unsafe fn process_block<P: PluginExport, H: Sample>(
    ctx: *mut std::ffi::c_void,
    inputs: *const *const H,
    outputs: *mut *mut H,
    num_input_channels: u32,
    num_output_channels: u32,
    num_frames: u32,
    events: *const Vst3MidiEvent,
    num_events: u32,
    transport_ptr: *const ffi::Vst3Transport,
    param_changes: *const ffi::Vst3ParamChange,
    num_param_changes: u32,
    process_mode: i32,
) {
    let nf = num_frames as usize;
    let ok = run_audio_block::<P>("VST3", || unsafe {
        let inst = &mut *ctx.cast::<Vst3Instance<P>>();
        let num_frames = nf;

        // Host called process() before setActive(true) - the plugin
        // hasn't been told its sample rate / max block size yet, so
        // running DSP would feed garbage out of un-snapped smoothers.
        // Zero outputs and bail.
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

        // Take ownership of the plugin for the whole block: an
        // uncontended `Acquire`, never a wait, since the host contract
        // keeps `process` from overlapping a lifecycle callback and host
        // saves read the snapshot instead of the plugin.
        let mut plugin = enter_plugin(&inst.plugin);

        // Apply any pending state-load before per-block work so the
        // plugin sees consistent params and extra state for the
        // entire block. See `pending_state` field comment for the
        // queue-overflow policy.
        if let Some(state) = inst.pending_state.pop() {
            state::apply_state(&mut *plugin, &state);
        }

        // Paranoid allocation check (the `rt-paranoid` feature): guard the
        // wrapper's per-block glue - event conversion, transport, process,
        // output encode, snapshot publish - as well as the plugin. Placed
        // after the state-load apply above, since `load_state` legitimately
        // allocates. No-op and zero-sized when the feature is off.
        let _rt = RtSection::enter();

        // Convert MIDI events. SysEx input arrives through a separate
        // callback before this process callback, so preserve the
        // queued SysEx entries when present and append short MIDI.
        if inst.sysex_inputs_pending {
            inst.sysex_inputs_pending = false;
        } else {
            inst.event_list.clear();
        }
        if !events.is_null() && num_events > 0 {
            let event_slice = slice::from_raw_parts(events, num_events as usize);
            for ev in event_slice {
                let body = if ev.status & 0xF0 == 0xF0 {
                    // VST3-specific: note expression carried in the
                    // same event struct. `data1=typeId`, `ne_value` is
                    // the host's full-precision `0..=1` value, and
                    // `note_id` is the host's per-voice counter -
                    // resolve it through the map built from note-ons
                    // below. An id the map doesn't know (never
                    // note-on'd, overwritten, or a host bug) is
                    // unattributable; drop the event rather than
                    // guess a pitch.
                    let type_id = ev.data1;
                    // Tuning is semitone-denominated: VST3's ±120 st
                    // domain re-scales onto the wire's ±48 st
                    // full-scale. The other types are plain `0..=1`.
                    let value = if type_id == 2 {
                        vst3_tuning_to_wire(ev.ne_value)
                    } else {
                        unit_to_u32(ev.ne_value)
                    };
                    inst.note_id_map
                        .lookup(ev.port, ev.note_id)
                        .and_then(|(channel, note)| {
                            let make_pn_cc = |cc| EventBody::PerNoteCC {
                                group: 0,
                                channel,
                                note,
                                cc,
                                value,
                                registered: true,
                            };
                            match type_id {
                                0 => Some(make_pn_cc(7)),  // volume
                                1 => Some(make_pn_cc(10)), // pan
                                2 => Some(EventBody::PerNotePitchBend {
                                    group: 0,
                                    channel,
                                    note,
                                    value,
                                }), // tuning
                                3 => Some(make_pn_cc(1)),  // vibrato
                                4 => Some(make_pn_cc(11)), // expression
                                5 => Some(make_pn_cc(74)), // brightness
                                _ => None,
                            }
                        })
                } else {
                    // Correlate the host's per-bus noteId with the
                    // note it addresses so later note-expression
                    // events can be resolved. The entry survives the
                    // note-off: hosts keep sending expression through
                    // the release phase, and stale slots are reclaimed
                    // by round-robin overwrite.
                    if ev.status & 0xF0 == 0x90 && ev.data2 > 0 {
                        inst.note_id_map
                            .insert(ev.port, ev.note_id, ev.status & 0x0F, ev.data1);
                    }
                    decode_short_message(ev.status, ev.data1, ev.data2)
                };
                if let Some(body) = body {
                    inst.event_list.push(Event {
                        sample_offset: ev.sample_offset,
                        port: ev.port,
                        body,
                    });
                }
            }
        }
        // Sort happens once below - after the param-change push
        // section also runs - instead of twice.

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
            len_u32(num_frames),
            P::supports_in_place(),
        );

        // Queue sample-accurate parameter changes. `set_plain` is
        // deferred to the chunker's per-sub-block apply pass so
        // smoothers see `set_target` at the event's sample rather
        // than at the head of the audio block.
        // The C++ shim sends plain (denormalized) values.
        if !param_changes.is_null() && num_param_changes > 0 {
            let changes = slice::from_raw_parts(param_changes, num_param_changes as usize);
            for pc in changes {
                // VST3 delivers sampleOffset as int32; per-block
                // offsets are non-negative and bounded by block size.
                #[allow(clippy::cast_sign_loss)]
                let sample_offset = pc.sample_offset as u32;
                // Unbound MIDI controllers arrive on the hidden proxy
                // ids: decode straight to the event. No `ParamChange`
                // and no `Params` write - a proxy is not a plugin
                // parameter. The shim's denormalize is identity for
                // proxy ids, so `pc.value` is the host's raw `0..=1`.
                // The id carries the event bus it was mapped for, so
                // multi-port plugins keep controllers per port.
                if let Some((port, channel, controller)) = midi_proxy_decode(pc.id) {
                    #[allow(clippy::cast_possible_truncation)]
                    let normalized = pc.value.clamp(0.0, 1.0) as f32;
                    inst.event_list.push(Event::on_port(
                        sample_offset,
                        port,
                        midi_proxy_event(channel, controller, normalized),
                    ));
                    continue;
                }
                // MIDI-mapped controllers (pitch bend, CC, pressure,
                // program) arrive here as parameter changes because
                // VST3 has no native input event for them. Bridge them
                // back into the MIDI event the plugin expects, in
                // addition to the plain `ParamChange` so the bound
                // parameter still tracks the controller.
                //
                // The bridged event is port 0: an explicit `midi_map`
                // binds one plugin parameter across every bus, so the
                // host delivers a bus-less parameter change with no
                // originating port to recover - unlike the per-bus
                // proxy ids decoded above.
                if let Ok(idx) = inst.midi_maps.binary_search_by_key(&pc.id, |(id, _)| *id) {
                    inst.event_list.push(Event {
                        sample_offset,
                        port: 0,
                        body: midi_event_from_map(&inst.midi_maps[idx].1, pc.value),
                    });
                }
                inst.event_list.push(Event {
                    sample_offset,
                    port: 0,
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
        inst.event_list.ensure_sorted_by_offset();

        let transport = if transport_ptr.is_null() {
            TransportInfo::default()
        } else {
            let t = &*transport_ptr;
            TransportInfo {
                playing: t.playing != 0,
                recording: t.recording != 0,
                tempo: t.tempo,
                // VST3 hosts deliver `i32` time-signature fields; the
                // u8 narrowing is bounded by the MIDI domain (≤ 255).
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                time_sig_num: t.time_sig_num as u8,
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                time_sig_den: t.time_sig_den as u8,
                position_samples: sample_pos_i64(t.position_samples),
                // Derived from samples so a plugin reading
                // `position_seconds` gets the same value on every format
                // (CLAP populates it directly). Guard the pre-reset zero SR.
                position_seconds: if inst.sample_rate > 0.0 {
                    t.position_samples / inst.sample_rate
                } else {
                    0.0
                },
                position_beats: t.position_beats,
                bar_start_beats: t.bar_start_beats,
                loop_active: t.cycle_active != 0,
                loop_start_beats: t.cycle_start_beats,
                loop_end_beats: t.cycle_end_beats,
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
            process_mode: vst3_process_mode(process_mode),
            output_events: &mut inst.output_events,
            params_fn: None,
            meters_fn: None,
            param_infos: &inst.param_infos,
            min_subblock_samples: inst.min_subblock_samples,
        };
        process_chunked(
            &mut *plugin,
            inst.params_arc.as_ref() as &dyn Params,
            &mut audio_buffer,
            chunk_args,
        );
        // End the `audio_buffer` borrow before reaching back into scratch.
        let _ = audio_buffer;
        // For `f64` plugins the scratch holds the rendered output -
        // copy + narrow it back to the host's `f32` pointers here.
        // No-op for `f32` plugins (output already pointed at the
        // host buffer).
        inst.scratch
            .finish_widening(outputs, num_output_channels, len_u32(num_frames));

        // Refresh latency / tail caches so the host's main-thread
        // queries don't have to touch the plugin. On an actual
        // latency change, flag a restart: `mark_restart` only sets an
        // atomic bit (RT-safe), and the shim calls `restartComponent` on
        // the next host main-thread callback.
        let new_latency = plugin.latency();
        if inst.latency_cache.swap(new_latency, Ordering::Relaxed) != new_latency {
            // `ctx` is the shim's live component key for this instance;
            // `mark_restart` only sets a bit on its atomic (RT-safe).
            ffi::truce_vst3_mark_restart(ctx, K_LATENCY_CHANGED);
        }
        inst.tail_cache.store(plugin.tail(), Ordering::Relaxed);
    });
    if !ok {
        // Panic in plugin.process() - zero outputs so the host
        // doesn't keep playing whatever stale samples were in the
        // buffer when DSP died.
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

/// Test-only smoke helper for the `rt-paranoid` CI gate: drives a few
/// real process blocks through this wrapper's per-block glue via the
/// shared `process_block` body (with null events / transport / param
/// changes and small stereo buffers), returning the steady-state
/// audio-thread allocation count (0 = clean). Vacuously 0 unless the
/// `rt-paranoid` feature installs the checking allocator. Not public API.
#[doc(hidden)]
#[must_use]
pub fn rt_paranoid_smoke<P: PluginExport>() -> u32 {
    const FRAMES: u32 = 512;
    const CH: u32 = 2;
    let frames = FRAMES as usize;
    // SAFETY: constructs, drives, and destroys its own instance; all
    // pointers below outlive each `process_block` call, buffers sized to
    // `FRAMES`, and the event / transport / param pointers are null
    // (which `process_block` tolerates).
    unsafe {
        let ctx = cb_create::<P>();
        cb_reset::<P>(ctx, 48_000.0, FRAMES, 0);

        // Non-zero input so the sanity check below can confirm the block
        // actually processed (a no-op harness would leave zeros).
        let in_left = vec![0.5f32; frames];
        let in_right = vec![0.5f32; frames];
        let mut out_left = vec![0f32; frames];
        let mut out_right = vec![0f32; frames];
        let in_ptrs: [*const f32; 2] = [in_left.as_ptr(), in_right.as_ptr()];
        let mut out_ptrs: [*mut f32; 2] = [out_left.as_mut_ptr(), out_right.as_mut_ptr()];

        let mut count = 0;
        for _ in 0..3 {
            let ((), n) = audit(|| {
                process_block::<P, f32>(
                    ctx,
                    in_ptrs.as_ptr(),
                    out_ptrs.as_mut_ptr(),
                    CH,
                    CH,
                    FRAMES,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    0,
                );
            });
            count = n;
        }

        assert!(
            out_left.iter().any(|s| s.abs() > 0.0),
            "vst3 smoke: process did not run (output stayed zero)"
        );
        cb_destroy::<P>(ctx);
        count
    }
}

unsafe extern "C" fn cb_param_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        // Read the cached `param_ranges.len()` rather than walking the
        // `Params` impl. The cache is built once at instantiation
        // (`Vst3Instance::new`) and never grows; trait dispatch was
        // free per-call but consistent with the cache-first pattern
        // the rest of the file uses.
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        len_u32(inst.param_ranges.len() + inst.midi_proxy_values.len())
    }
}

unsafe extern "C" fn cb_param_get_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
) -> f64 {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(rel) = id.checked_sub(MIDI_PROXY_ID_BASE)
            && let Some(slot) = inst.midi_proxy_values.get(rel as usize)
        {
            return f64::from_bits(slot.load(Ordering::Relaxed));
        }
        inst.params_arc.get_plain(id).unwrap_or(0.0)
    }
}

unsafe extern "C" fn cb_param_set_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(rel) = id.checked_sub(MIDI_PROXY_ID_BASE)
            && let Some(slot) = inst.midi_proxy_values.get(rel as usize)
        {
            slot.store(value.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
            return;
        }
        inst.params_arc.set_plain(id, value);
    }
}

unsafe extern "C" fn cb_param_normalize<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    plain: f64,
) -> f64 {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
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
        let inst = &*ctx.cast::<Vst3Instance<P>>();
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
    // The author's `format_value` can panic (an `unwrap` on a host value
    // outside the declared domain); firewall it so that can't abort the
    // host. On panic the host sees an empty display string.
    run_extern_callback_with::<P, u32>("vst3", "format_value", 0, || unsafe {
        // `out_len == 0` would underflow on `out_len as usize - 1`
        // and let `copy_nonoverlapping` write the full formatted
        // string into a buffer the host claimed had zero capacity.
        // Treat zero capacity as "host wants nothing" and return.
        if out_len == 0 || out.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        match inst.params_arc.format_value(id, value) {
            Some(text) => {
                let bytes = text.as_bytes();
                let len = bytes.len().min((out_len as usize) - 1);
                std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out, len);
                *out.add(len) = 0;
                len_u32(len)
            }
            None => 0,
        }
    })
}

unsafe extern "C" fn cb_param_parse<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    text: *const c_char,
    out_plain: *mut f64,
) -> i32 {
    // Author `parse_value` can panic; firewall it (0 = "not parsed").
    run_extern_callback_with::<P, i32>("vst3", "parse_value", 0, || unsafe {
        if text.is_null() || out_plain.is_null() {
            return 0;
        }
        let Ok(text) = CStr::from_ptr(text).to_str() else {
            return 0;
        };
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        match inst.params_arc.parse_value(id, text) {
            Some(v) => {
                *out_plain = v;
                1
            }
            None => 0,
        }
    })
}

unsafe extern "C" fn cb_state_save<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out_data: *mut *mut u8,
    out_len: *mut u32,
) {
    // Pre-zero the out pointers so a panic anywhere in the body below
    // leaves the host seeing an empty blob rather than a stale buffer
    // pointer paired with whatever length was last written. The body
    // overwrites these on the happy path.
    unsafe {
        *out_data = std::ptr::null_mut();
        *out_len = 0;
    }
    run_extern_callback_with::<P, ()>("vst3", "save_state", (), || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let (ids, values) = inst.params_arc.collect_values();
        // Read the custom state from the lock-free snapshot the audio
        // thread publishes each block. Never touches the plugin, so it
        // can't stall a block in flight.
        //
        // Allocator pin: this wrapper allocates with `libc_malloc` and
        // the C++ shim frees with `libc::free`. The Rust global
        // allocator must not appear on either side. (VST2 uses the
        // Rust global allocator for both save + free; do not cross
        // wires when refactoring `_save_state` paths together.)
        let extra = save_extra(&inst.snapshot);
        let persist = inst.params_arc.serialize_persist();
        let blob = state::serialize_state(inst.plugin_id_hash, &ids, &values, &extra, &persist);
        let len = blob.len();
        let ptr = libc_malloc(len).cast::<u8>();
        if ptr.is_null() {
            // malloc failed - `*out_data` is already null and
            // `*out_len` already 0 from the pre-zero above; nothing
            // to do on this branch except return.
            return;
        }
        std::ptr::copy_nonoverlapping(blob.as_ptr(), ptr, len);
        *out_data = ptr;
        *out_len = len_u32(len);
    });
}

unsafe extern "C" fn cb_state_load<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    data: *const u8,
    len: u32,
) -> i32 {
    run_extern_callback_with::<P, i32>("vst3", "load_state", 0, || unsafe {
        let inst = &mut *ctx.cast::<Vst3Instance<P>>();
        // `slice::from_raw_parts(null, n)` for `n > 0` is UB. Treat
        // `(null, *)` and `(_, 0)` the same as "host gave us nothing".
        if data.is_null() || len == 0 {
            return 0;
        }
        let blob = slice::from_raw_parts(data, len as usize);
        // Not this plugin's envelope? Offer the bytes to the plugin's
        // `migrate_state` hook (legacy sessions from a pre-truce
        // build); `None` fails the load honestly.
        let Some(deserialized) = state::parse_or_migrate::<P>(
            blob,
            inst.plugin_id_hash,
            state::PluginFormat::Vst3,
            None,
        ) else {
            return 0;
        };
        // Apply params synchronously on the host thread (atomic-safe)
        // so host-side queries that read parameter values right
        // after `setState` see the restored values without first
        // running a process block. pluginval / DAW preset reload
        // both observe this.
        state::apply_params(&*inst.params_arc, &deserialized);
        if inst.active.load(Ordering::Relaxed) {
            // Active: the audio thread drains `pending_state` at the top
            // of the next block and applies the custom-state blob under
            // its exclusive `&mut plugin`. `force_push` overwrites any
            // older pending blob - see the `pending_state` field comment
            // for why newest-wins is right.
            let _ = inst.pending_state.force_push(deserialized);
        } else {
            // Inactive: no `cb_process` will run, so apply the full
            // state (params + extra) synchronously under the plugin
            // lock - uncontended here since no audio thread is
            // processing. Otherwise a `getState` before the next
            // activate would re-serialize stale custom state.
            let mut plugin = enter_plugin(&inst.plugin);
            state::apply_state(&mut *plugin, &deserialized);
            // No `cb_process` will publish, so refresh the snapshot slot
            // now - a `getState` while still inactive reads live state.
            plugin.republish_snapshot();
        }
        if let Some(ref mut editor) = inst.editor {
            editor.state_changed();
        }
        1
    })
}

unsafe extern "C" fn cb_state_free(data: *mut u8, _len: u32) {
    unsafe {
        if !data.is_null() {
            libc_free(data.cast::<std::ffi::c_void>());
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
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        inst.latency_cache.load(Ordering::Relaxed)
    }
}

unsafe extern "C" fn cb_get_tail<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        inst.tail_cache.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Output event callbacks
// ---------------------------------------------------------------------------

/// Map a truce `Event` body to a 3-byte VST3 MIDI packet. Returns
/// `None` for event types that don't fit (MIDI 2.0, `ParamChange`,
/// Transport, etc.). The output count and the index→event lookup
/// share this filter so unsupported events are skipped cleanly
/// rather than emitted as a zeroed packet (which earlier hosts
/// interpreted as a `note 0` Note-Off).
fn try_encode_vst3_midi(event: &Event) -> Option<Vst3MidiEvent> {
    // MIDI 2.0 channel-voice output has no UMP transport on VST3, so
    // down-convert to 1.0. Bodies that map to a predefined expression
    // type ride note expression via `note_expression_of` - converting
    // them here too would double-emit. Everything else (including
    // per-note CCs with no predefined type) falls through to the 1.0
    // down-convert, so an unmapped per-note CC degrades to a channel
    // CC exactly as it does on CLAP.
    let body = match event.body {
        body if note_expression_of(&body).is_some() => return None,
        other => downconvert_to_midi1(&other).unwrap_or(other),
    };
    let (status, data1, data2) = match &body {
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
    Some(Vst3MidiEvent {
        sample_offset: event.sample_offset,
        status,
        data1,
        data2,
        port: event.port,
        note_id: -1,
        ne_value: 0.0,
    })
}

/// VST3 has no native CC / pitch-bend / channel-pressure / program
/// input event. Hosts route those MIDI messages to a parameter the
/// plugin advertises through `IMidiMapping` (see
/// `cb_midi_mapping_get_param_id`) and deliver them as parameter
/// changes. When `info` carries such a binding, turn the parameter
/// change back into the MIDI event the plugin expects, so event-based
/// plugins behave the same here as on AU / CLAP / LV2 (which hand the
/// plugin raw MIDI). Returns `None` for unmapped parameters - the
/// caller still emits the plain `ParamChange`.
///
/// `plain` is the denormalized value the shim already produced; we
/// re-normalize through the parameter's range so the MIDI-domain
/// mapping is independent of how the binding parameter declares its
/// range.
//
// `norm as f32` is a lossless-enough narrowing of a clamped `0..=1`
// value; the MIDI encoders take `f32`.
#[allow(clippy::cast_possible_truncation)]
/// A parameter's precomputed MIDI-controller binding. Built once per
/// instance for every param that declares a `midi_map`, so the audio
/// thread can bridge a mapped controller change to its `EventBody`
/// through a binary search instead of a linear `ParamInfo` scan.
#[derive(Clone, Copy)]
struct MidiMap {
    source: MidiSource,
    channel: u8,
    range: ParamRange,
}

impl MidiMap {
    /// The binding `info` declares, or `None` when it has no `midi_map`.
    fn from_param(info: &ParamInfo) -> Option<Self> {
        Some(Self {
            source: info.midi_map?,
            channel: info.midi_channel.unwrap_or(0),
            range: info.range,
        })
    }
}

/// Bridge a MIDI-mapped parameter change back into the `EventBody` the
/// plugin expects. VST3 has no native input event for channel MIDI, so
/// the host delivers it as a parameter change on the mapped id.
// `normalize` yields a `0.0..=1.0` value; the MIDI encoders take `f32`.
#[allow(clippy::cast_possible_truncation)]
fn midi_event_from_map(map: &MidiMap, plain: f64) -> EventBody {
    let channel = map.channel;
    let norm = map.range.normalize(plain) as f32; // 0.0..=1.0
    match map.source {
        // Host-normalized `0..1` is the pitch-wheel position (0 = full
        // down, 0.5 = center, 1 = full up); shift to `[-1, 1]` for the
        // 14-bit encoder.
        MidiSource::PitchBend => EventBody::PitchBend {
            group: 0,
            channel,
            value: denorm_pitch_bend(norm * 2.0 - 1.0),
        },
        MidiSource::Cc(cc) => EventBody::ControlChange {
            group: 0,
            channel,
            cc,
            value: denorm_7bit(norm),
        },
        MidiSource::ChannelPressure => EventBody::ChannelPressure {
            group: 0,
            channel,
            pressure: denorm_7bit(norm),
        },
        MidiSource::ProgramChange => EventBody::ProgramChange {
            group: 0,
            channel,
            program: denorm_7bit(norm),
        },
    }
}

// ---------------------------------------------------------------------------
// MIDI input proxy parameters
//
// VST3 has no input events for channel-level MIDI; hosts deliver CC /
// pitch bend / channel pressure only to a parameter advertised through
// `IMidiMapping`. Explicit `midi_map` bindings cover parameters the
// plugin *wants* as parameters; these hidden proxies cover everything
// else, so event-consuming plugins hear the same MIDI on VST3 as on
// AU / CLAP / LV2 / VST2. Proxies are not real parameters: never
// serialized into state, never visible to `Params`, only an
// `IMidiMapping` target that turns back into the matching `EventBody`.
// ---------------------------------------------------------------------------

/// Base id for the proxy range. Real param ids can't collide: derive
/// hash ids are masked into `0..METER_ID_BASE` (`1 << 24`), meters
/// count up from there, and explicit ids at or above `METER_ID_BASE`
/// are rejected at derive time.
const MIDI_PROXY_ID_BASE: u32 = 1 << 25;
/// Controllers per channel: CC 0..=127, 128 = channel pressure,
/// 129 = pitch bend. Program change (VST3 controller 130) is
/// deliberately not proxied - `kIsProgramChange` parameters interact
/// with unit/program-list metadata, so it stays explicit-binding-only.
const MIDI_PROXY_PER_CHANNEL: u32 = 130;
/// One event-input bus's worth of proxies (16 channels). A plugin
/// gets one bank per declared MIDI input port, so controllers keep
/// their bus attribution - the host queries `IMidiMapping` per bus
/// and a shared id would merge every bus's values into one parameter
/// queue before truce ever saw them.
const MIDI_PROXY_BANK: u32 = 16 * MIDI_PROXY_PER_CHANNEL;
const MIDI_PROXY_PRESSURE: u32 = 128;
const MIDI_PROXY_PITCH_BEND: u32 = 129;

fn midi_proxy_id(port: u8, channel: u8, controller: u32) -> u32 {
    MIDI_PROXY_ID_BASE
        + u32::from(port) * MIDI_PROXY_BANK
        + u32::from(channel.min(15)) * MIDI_PROXY_PER_CHANNEL
        + controller
}

/// `(port, channel, controller)` for a proxy id, `None` for real
/// param ids. Accepts the full 256-bank shape; ids past the plugin's
/// declared port count can't occur in practice because registration
/// and the `IMidiMapping` resolver only hand out ids for real buses.
fn midi_proxy_decode(id: u32) -> Option<(u8, u8, u32)> {
    let rel = id.checked_sub(MIDI_PROXY_ID_BASE)?;
    if rel >= 256 * MIDI_PROXY_BANK {
        return None;
    }
    // Bank / channel indices are bounded to 0..256 / 0..16 by the
    // check above and the modulo.
    #[allow(clippy::cast_possible_truncation)]
    Some((
        (rel / MIDI_PROXY_BANK) as u8,
        (rel % MIDI_PROXY_BANK / MIDI_PROXY_PER_CHANNEL) as u8,
        rel % MIDI_PROXY_PER_CHANNEL,
    ))
}

/// Wheel-centred default for pitch bend, zero for everything else.
fn midi_proxy_default(controller: u32) -> f64 {
    if controller == MIDI_PROXY_PITCH_BEND {
        0.5
    } else {
        0.0
    }
}

/// The MIDI event a proxy change decodes to. `normalized` is the
/// host's `0..=1` wheel/controller position (the shim's denormalize is
/// identity for proxy ids, so the plain value passes through).
fn midi_proxy_event(channel: u8, controller: u32, normalized: f32) -> EventBody {
    match controller {
        MIDI_PROXY_PITCH_BEND => EventBody::PitchBend {
            group: 0,
            channel,
            value: denorm_pitch_bend(normalized * 2.0 - 1.0),
        },
        MIDI_PROXY_PRESSURE => EventBody::ChannelPressure {
            group: 0,
            channel,
            pressure: denorm_7bit(normalized),
        },
        cc => EventBody::ControlChange {
            group: 0,
            channel,
            // Bounded to 0..=127 by `midi_proxy_decode`.
            cc: u8::try_from(cc).unwrap_or(0) & 0x7F,
            value: denorm_7bit(normalized),
        },
    }
}

/// Proxy count for this plugin: the full bank when it accepts MIDI
/// input, zero otherwise (no surface change for non-MIDI plugins).
fn midi_proxy_len<P: PluginExport>() -> usize {
    if P::info().accepts_midi_in {
        MIDI_PROXY_BANK as usize * usize::from(P::info().midi_input_ports)
    } else {
        0
    }
}

unsafe extern "C" fn cb_get_output_event_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        len_u32(
            inst.output_events
                .iter()
                .filter(|e| try_encode_vst3_midi(e).is_some())
                .count(),
        )
    }
}

unsafe extern "C" fn cb_get_output_event<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out: *mut Vst3MidiEvent,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        // Walk the filtered iterator until we hit the index-th
        // encodable event. Out-of-range index leaves `*out`
        // untouched; the C++ shim zero-initialized the buffer before
        // calling, so callers that forget to bounds-check against
        // `cb_get_output_event_count` get a zero packet rather than
        // stale stack data.
        if let Some(packet) = inst
            .output_events
            .iter()
            .filter_map(try_encode_vst3_midi)
            .nth(index as usize)
        {
            *out = packet;
        }
    }
}

// ---------------------------------------------------------------------------
// SysEx callbacks (host → plug-in input, plug-in → host output)
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_push_sysex_input<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_offset: u32,
    port: u8,
    bytes: *const u8,
    len: u32,
) {
    unsafe {
        let inst = &mut *ctx.cast::<Vst3Instance<P>>();
        if bytes.is_null() || len == 0 {
            return;
        }
        if !inst.sysex_inputs_pending {
            inst.event_list.clear();
            inst.sysex_inputs_pending = true;
        }
        let slice = std::slice::from_raw_parts(bytes, len as usize);
        // Pool-full failure: drop the message. SysEx is atomic by
        // spec; truncating would corrupt it. The plug-in surfaces
        // the loss via the `EventList`'s pool usage metrics if it
        // cares.
        let _ = inst
            .event_list
            .push_sysex_on_port(sample_offset, port, slice);
    }
}

unsafe extern "C" fn cb_get_output_sysex_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        len_u32(
            inst.output_events
                .iter()
                .filter(|e| matches!(e.body, EventBody::SysEx { .. }))
                .count(),
        )
    }
}

unsafe extern "C" fn cb_get_output_sysex_event<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out_sample_offset: *mut u32,
    out_port: *mut u8,
    out_bytes: *mut *const u8,
    out_len: *mut u32,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        // Walk the filtered iterator, same shape as
        // `cb_get_output_event`. Bytes point into the plug-in's
        // SysEx pool - valid until the shim's next `process()`
        // clears the `EventList`, which is after the host's
        // `addEvent` has copied them.
        if let Some(event) = inst
            .output_events
            .iter()
            .filter(|e| matches!(e.body, EventBody::SysEx { .. }))
            .nth(index as usize)
        {
            let bytes = inst.output_events.sysex_bytes(&event.body);
            *out_sample_offset = event.sample_offset;
            *out_port = event.port;
            *out_bytes = bytes.as_ptr();
            *out_len = len_u32(bytes.len());
        }
    }
}

/// Map a truce per-note MIDI 2.0 event to a VST3 note-expression tuple
/// `(type_id, note_id, value)`. VST3 has no UMP; per-note richness rides
/// `INoteExpressionController` value events keyed by `note_id`. We key
/// `note_id` deterministically as `(channel << 7) | note` - the shim
/// stamps the plugin's `NoteOn` with the same id, so notes and their
/// expression correlate without any per-instance tracking state. Returns
/// `None` for per-note controllers VST3 has no predefined type for; the
/// value is normalized `0..=1` (VST3's `NoteExpressionValue` domain).
fn note_expression_of(body: &EventBody) -> Option<(u32, i32, f64)> {
    // Predefined VST3 NoteExpressionTypeIDs (reverse of the input map):
    // Volume=0, Pan=1, Tuning=2, Vibrato=3, Expression=4, Brightness=5.
    let (type_id, channel, note, value) = match *body {
        // Registered per-note controllers only: the predefined VST3
        // expression types carry the registered indices' semantics;
        // an assignable index is manufacturer-defined and must not
        // alias onto them.
        EventBody::PerNoteCC {
            channel,
            note,
            cc,
            value,
            registered: true,
            ..
        } => {
            let type_id = match cc {
                7 => 0,
                10 => 1,
                1 => 3,
                11 => 4,
                74 => 5,
                _ => return None,
            };
            (type_id, channel, note, u32_to_unit(value))
        }
        EventBody::PerNotePitchBend {
            channel,
            note,
            value,
            ..
        } => (2, channel, note, wire_to_vst3_tuning(value)),
        _ => return None,
    };
    Some((type_id, vst3_note_id(channel, note), value))
}

/// Deterministic VST3 `noteId` for a truce note: `(channel << 7) | note`.
/// The C++ shim stamps every emitted note-on/off with the same formula,
/// so a plug-in's note-expression events address the live note without
/// any shared correlation state.
fn vst3_note_id(channel: u8, note: u8) -> i32 {
    (i32::from(channel & 0x0F) << 7) | i32::from(note & 0x7F)
}

/// Normalize a wire-native 32-bit per-note value into VST3's `0..=1`
/// `NoteExpressionValue` domain.
fn u32_to_unit(v: u32) -> f64 {
    f64::from(v) / f64::from(u32::MAX)
}

/// VST3's tuning note-expression span: normalized `0..=1` covers
/// `-120..=+120` semitones (`plain = 240 * (norm - 0.5)` per the SDK).
const VST3_TUNING_SPAN_SEMITONES: f64 = 240.0;

/// VST3 tuning norm (`0..=1`, ±120 st) -> wire per-note bend. The wire
/// full-scale is ±48 st, so a wider host bend saturates.
fn vst3_tuning_to_wire(norm: f64) -> u32 {
    per_note_bend_from_semitones((norm - 0.5) * VST3_TUNING_SPAN_SEMITONES)
}

/// Wire per-note bend (±48 st full-scale) -> VST3 tuning norm
/// (`0..=1`, ±120 st), so the same event bends identically on every
/// semitone-denominated host domain.
fn wire_to_vst3_tuning(v: u32) -> f64 {
    0.5 + per_note_bend_semitones(v) / VST3_TUNING_SPAN_SEMITONES
}

/// Inverse of [`u32_to_unit`]: widen a VST3 `NoteExpressionValue` into
/// the wire-native 32-bit per-note domain. Hosts are supposed to stay
/// in `0..=1`, but the value crosses an FFI boundary - clamp first.
fn unit_to_u32(v: f64) -> u32 {
    // Clamped to `0..=u32::MAX` before the cast, so no truncation or
    // sign loss is possible.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scaled = (v.clamp(0.0, 1.0) * f64::from(u32::MAX)).round() as u32;
    scaled
}

unsafe extern "C" fn cb_get_output_note_expression_count<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        len_u32(
            inst.output_events
                .iter()
                .filter(|e| note_expression_of(&e.body).is_some())
                .count(),
        )
    }
}

unsafe extern "C" fn cb_get_output_note_expression<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out_type_id: *mut u32,
    out_note_id: *mut i32,
    out_sample_offset: *mut u32,
    out_value: *mut f64,
    out_port: *mut u8,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(event) = inst
            .output_events
            .iter()
            .filter(|e| note_expression_of(&e.body).is_some())
            .nth(index as usize)
            && let Some((type_id, note_id, value)) = note_expression_of(&event.body)
        {
            *out_type_id = type_id;
            *out_note_id = note_id;
            *out_sample_offset = event.sample_offset;
            *out_value = value;
            // The correlated note-on rode this bus; the shim clamps
            // like the note path so both land on the same one.
            *out_port = event.port;
        }
    }
}

unsafe extern "C" fn cb_get_output_param_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        len_u32(
            inst.output_events
                .iter()
                .filter(|e| matches!(e.body, EventBody::ParamChange { .. }))
                .count(),
        )
    }
}

unsafe extern "C" fn cb_get_output_param<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out_id: *mut u32,
    out_sample_offset: *mut i32,
    out_value: *mut f64,
) {
    unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(event) = inst
            .output_events
            .iter()
            .filter(|e| matches!(e.body, EventBody::ParamChange { .. }))
            .nth(index as usize)
            && let EventBody::ParamChange { id, value } = event.body
        {
            // VST3 output param queues carry normalized values; the
            // plugin emits plain. Fall back to the plain value if the
            // id has no descriptor (shouldn't happen for real params).
            let normalized = inst
                .param_infos
                .iter()
                .find(|i| i.id == id)
                .map_or(value, |i| i.range.normalize(value));
            *out_id = id;
            *out_sample_offset = i32::try_from(event.sample_offset).unwrap_or(0);
            *out_value = normalized;
        }
    }
}

// ---------------------------------------------------------------------------
// GUI callbacks
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_gui_has_editor<P: PluginExport>(ctx: *mut std::ffi::c_void) -> i32 {
    // The editor builder is author code; firewall its lazy construction so
    // a panic there can't unwind across the C ABI (0 = "no editor").
    run_extern_callback_with::<P, i32>("vst3", "gui_has_editor", 0, || unsafe {
        if ctx.is_null() {
            return 0;
        }
        let inst = &mut *ctx.cast::<Vst3Instance<P>>();
        if inst.editor.is_none() {
            // Built from the lock-free param store the wrapper already
            // holds outside the plugin, so opening the GUI never
            // stalls the audio thread.
            inst.editor = (inst.editor_builder)(inst.params_arc.clone());
            // Replay a content scale the host reported before the editor
            // existed (a valid VST3 ordering - `setContentScaleFactor`
            // can precede the editor object). macOS drives Retina through
            // AppKit, not this callback, so `host_scale` stays 1.0 there;
            // pinning it would force 1x rendering, so skip macOS.
            #[cfg(not(target_os = "macos"))]
            {
                let scale = inst.host_scale;
                if let Some(ref mut editor) = inst.editor {
                    editor.set_scale_factor(scale);
                }
            }
        }
        i32::from(inst.editor.is_some())
    })
}

unsafe extern "C" fn cb_gui_get_size<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    w: *mut u32,
    h: *mut u32,
) {
    // `Editor::size` is author code; firewall it so a panic can't unwind
    // across the C ABI.
    run_extern_callback_with::<P, ()>("vst3", "gui_get_size", (), || unsafe {
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        if let Some(ref editor) = inst.editor {
            let (ew, eh) = editor.size();
            // VST3 `ViewRect` is documented as "in pixels". That's literally
            // true on Windows/Linux, where hosts expect physical pixels and
            // may drive the scale via `IPlugViewContentScaleSupport`. On
            // macOS, AppKit handles the Retina backing automatically and
            // hosts expect logical points - scaling here would double the
            // window on Retina displays.
            #[cfg(target_os = "macos")]
            {
                *w = ew;
                *h = eh;
            }
            #[cfg(not(target_os = "macos"))]
            {
                // Round-to-nearest, not truncate - `(w * scale) as u32`
                // would round 199.9 → 199, drifting one pixel on
                // fractional scales. Matches the CLAP / AAX / `to_physical_px`
                // helper used elsewhere. Logical pixel sizes are bounded
                // by `u32::MAX / scale`; in practice no editor exceeds
                // 16384 logical pixels, so the `f64 → u32` truncation
                // and sign casts are safe.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                {
                    *w = (f64::from(ew) * inst.host_scale).round() as u32;
                    *h = (f64::from(eh) * inst.host_scale).round() as u32;
                }
            }
        }
    });
}

unsafe extern "C" fn cb_gui_set_content_scale<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    scale: f64,
) {
    // `Editor::set_scale_factor` is author code; firewall it.
    run_extern_callback_with::<P, ()>("vst3", "gui_set_content_scale", (), || unsafe {
        if ctx.is_null() || !scale.is_finite() || scale <= 0.0 {
            return;
        }
        // Clamp to the same range the GUI cluster's `EditorScale`
        // cell uses. A buggy host passing `f64::MAX` would otherwise
        // propagate to the editor and overflow when the editor
        // multiplies its logical size to physical pixels.
        let scale = scale.clamp(0.25, 8.0);
        let inst = &mut *ctx.cast::<Vst3Instance<P>>();
        inst.host_scale = scale;
        if let Some(ref mut editor) = inst.editor {
            editor.set_scale_factor(scale);
        }
    });
}

/// `IPlugView::canResize` callback. Returns 1 / 0 mapping to
/// `kResultOk` / `kResultFalse` on the shim side.
unsafe extern "C" fn cb_gui_can_resize<P: PluginExport>(ctx: *mut std::ffi::c_void) -> i32 {
    // `Editor::can_resize` is author code; firewall it (0 = "not resizable").
    run_extern_callback_with::<P, i32>("vst3", "gui_can_resize", 0, || unsafe {
        if ctx.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        i32::from(inst.editor.as_ref().is_some_and(|e| e.can_resize()))
    })
}

/// `IPlugView::checkSizeConstraint` callback. Clamps the
/// requested physical width / height in place against the
/// editor's `min_size` / `max_size` / `aspect_ratio`. For
/// fixed-size editors snaps to the editor's current size (JUCE's
/// Ableton-Live workaround pattern).
unsafe extern "C" fn cb_gui_check_size_constraint<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    w: *mut u32,
    h: *mut u32,
) {
    // `Editor::can_resize` / `size` are author code; firewall them.
    run_extern_callback_with::<P, ()>("vst3", "gui_check_size_constraint", (), || unsafe {
        if ctx.is_null() || w.is_null() || h.is_null() {
            return;
        }
        let inst = &*ctx.cast::<Vst3Instance<P>>();
        let Some(ref editor) = inst.editor else {
            return;
        };
        let host_scale = inst.host_scale;
        if editor.can_resize() {
            // Physical -> logical, fit, logical -> physical. Fit the largest
            // on-ratio box *inside* the requested cursor box (never larger on
            // either axis). VST3 hosts drive the drag from the raw cursor and
            // re-assert it every frame, so any size we return that exceeds the
            // cursor (a single-edge "grow the other axis") is honoured for one
            // frame then bounced - the window judders. A size <= the cursor
            // is a fixed point the host converges on.
            let (lw, lh) = phys_to_logical(*w, *h, host_scale);
            let (fw, fh) = fit_logical_size(lw, lh, editor.as_ref());
            let (pw, ph) = logical_to_phys(fw, fh, host_scale);
            *w = pw;
            *h = ph;
        } else {
            // Snap to current size; host-side Live quirk handled
            // identically by JUCE.
            let (cw, ch) = editor.size();
            let (pw, ph) = logical_to_phys(cw, ch, host_scale);
            *w = pw;
            *h = ph;
        }
    });
}

/// `IPlugView::onSize` callback. Host committed a new size; delegate
/// to `Editor::set_size` after scaling physical -> logical. The editor
/// *fills* the committed window (min/max clamp only) rather than
/// re-fitting onto the aspect ratio - that shaping happened earlier in
/// `checkSizeConstraint`, the host's drag-negotiation point, and
/// flooring it again here would leave a 1px letterbox line at the
/// bottom. `onSize` must not request a resize: VST3 forbids
/// `IPlugFrame::resizeView` from inside `onSize`, and a reentrant call
/// judders the drag.
unsafe extern "C" fn cb_gui_set_size<P: PluginExport>(ctx: *mut std::ffi::c_void, w: u32, h: u32) {
    // `Editor::set_size` / `can_resize` are author code; firewall them.
    run_extern_callback_with::<P, ()>("vst3", "gui_set_size", (), || unsafe {
        if ctx.is_null() || w == 0 || h == 0 {
            return;
        }
        let inst = &mut *ctx.cast::<Vst3Instance<P>>();
        let host_scale = inst.host_scale;
        if let Some(ref mut editor) = inst.editor
            && editor.can_resize()
        {
            let (lw, lh) = phys_to_logical(w, h, host_scale);
            let (cw, ch) = clamp_logical_size(lw, lh, editor.as_ref());
            editor.set_size(cw, ch);
        }
    });
}

/// `IMidiMapping::getMidiControllerAssignment` callback. Resolves the
/// host's controller query to a bound parameter id from the static
/// `midi_map` metadata - no plugin instance needed.
//
// `controller as u8` is guarded by the `0..=127` match arm; `channel`
// goes through `try_from` so a negative never wraps.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
unsafe extern "C" fn cb_midi_mapping_get_param_id<P: PluginExport>(
    _ctx: *mut std::ffi::c_void,
    bus_index: i32,
    channel: i16,
    controller: i16,
    out_param_id: *mut u32,
) -> i32 {
    // VST3 `ControllerNumbers`: 0..=127 are CCs; the extended values
    // mirror the output path's encoding (`ivstmidicontrollers.h`).
    let source = match controller {
        0..=127 => MidiSource::Cc(controller as u8),
        128 => MidiSource::ChannelPressure, // kAfterTouch
        129 => MidiSource::PitchBend,
        130 => MidiSource::ProgramChange, // kCtrlProgramChange
        _ => return 0,                    // kResultFalse
    };
    let channel = u8::try_from(channel).unwrap_or(0);
    // Returns a hit-flag (1/0); the shim maps it to kResultOk /
    // kResultFalse for the VST3 boundary. Explicit bindings win on
    // every bus - a bound parameter is one value, so per-port
    // separation doesn't apply to it. Everything unbound falls
    // through to the hidden proxy bank for the queried bus, keeping
    // controllers attributed per port (program change excepted -
    // not proxied).
    if let Some(id) = truce_params::map_source_to_param(&P::param_infos_static(), channel, source) {
        unsafe { out_param_id.write(id) };
        return 1;
    }
    let Ok(port) = u8::try_from(bus_index) else {
        return 0;
    };
    if P::info().accepts_midi_in
        && port < P::info().midi_input_ports
        && channel < 16
        && let Ok(controller) = u32::try_from(controller)
        && controller < MIDI_PROXY_PER_CHANNEL
    {
        unsafe { out_param_id.write(midi_proxy_id(port, channel, controller)) };
        return 1;
    }
    0
}

/// Convert physical pixels (what the VST3 host speaks) to logical
/// points (what `Editor` works in). Identity when `host_scale` is
/// 1.0 or invalid.
fn phys_to_logical(pw: u32, ph: u32, host_scale: f64) -> (u32, u32) {
    if host_scale <= 0.0 || (host_scale - 1.0).abs() < f64::EPSILON {
        return (pw, ph);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lw = (f64::from(pw) / host_scale).round() as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lh = (f64::from(ph) / host_scale).round() as u32;
    (lw.max(1), lh.max(1))
}

/// Inverse of `phys_to_logical`.
fn logical_to_phys(lw: u32, lh: u32, host_scale: f64) -> (u32, u32) {
    if host_scale <= 0.0 || (host_scale - 1.0).abs() < f64::EPSILON {
        return (lw, lh);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pw = (f64::from(lw) * host_scale).round() as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ph = (f64::from(lh) * host_scale).round() as u32;
    (pw.max(1), ph.max(1))
}

unsafe extern "C" fn cb_gui_open<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    parent: *mut std::ffi::c_void,
) {
    // `editor.open` runs author GUI-construction code that can panic;
    // firewall it so the panic can't unwind across the C ABI.
    run_extern_callback_with::<P, ()>("vst3", "gui_open", (), || unsafe {
        let inst = &mut *ctx.cast::<Vst3Instance<P>>();
        if let Some(ref mut editor) = inst.editor {
            let params = Arc::clone(&inst.params_arc);
            let meter_store = Arc::clone(&inst.meter_store);
            let snapshot = Arc::clone(&inst.snapshot);
            let ctx_raw = SendPtr::new(ctx);
            let params_for_set = params.clone();
            let params_for_get = params.clone();
            let params_for_plain = params.clone();
            let params_for_fmt = params.clone();
            let params_for_ctx = params.clone();
            let task_spawner_for_ctx = inst.task_spawner.clone();
            let pending_state_for_set = inst.pending_state.clone();
            let transport_slot = inst.transport_slot.clone();
            let context = PluginContext::from_closures(
                ClosureBridge {
                    begin_edit: Box::new(move |id| {
                        ffi::truce_vst3_begin_edit(ctx_raw.as_ptr().cast_mut(), id);
                    }),
                    set_param: Box::new(move |id, value| {
                        // Single trait dispatch: same value-then-readback
                        // pattern collapsed via the trait helper. The
                        // post-clamp normalized value is what the host
                        // expects for `IComponentHandler::performEdit`.
                        let norm = params_for_set.set_normalized_returning_normalized(id, value);
                        ffi::truce_vst3_perform_edit(ctx_raw.as_ptr().cast_mut(), id, norm);
                    }),
                    end_edit: Box::new(move |id| {
                        ffi::truce_vst3_end_edit(ctx_raw.as_ptr().cast_mut(), id);
                    }),
                    request_resize: Box::new(move |w, h| {
                        // SAFETY: `ctx_raw` is the live
                        // `Vst3Instance` pointer the shim holds in
                        // its ctx -> TruceComponent table. The
                        // closure runs on the GUI thread, same as
                        // `cb_gui_set_content_scale` which is the
                        // only writer of `host_scale`. Routing
                        // through the shim's component (rather
                        // than holding a plug view pointer) avoids
                        // UAF across host editor recreations.
                        let host_scale = (*ctx_raw.as_ptr().cast::<Vst3Instance<P>>()).host_scale;
                        // VST3 hosts speak physical points;
                        // `Editor` speaks logical.
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let pw = (f64::from(w) * host_scale).round() as u32;
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let ph = (f64::from(h) * host_scale).round() as u32;
                        ffi::truce_vst3_request_resize(ctx_raw.as_ptr().cast_mut(), pw, ph) != 0
                    }),
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
                    get_meter: Box::new(move |id| meter_store.read(id)),
                    get_state: Box::new(move || {
                        // Editor state read: lock-free, reads the snapshot
                        // the audio thread publishes each block. Never
                        // touches the plugin, so an editor read can't
                        // stall audio.
                        save_extra(&snapshot)
                    }),
                    set_state: Box::new(move |bytes| {
                        // The editor sends RAW custom-state bytes -
                        // exactly what `save_state()` emits and
                        // `get_state` above returns - NOT a full
                        // `serialize_state` envelope. Route them to the
                        // plugin's `load_state` on the audio thread via
                        // the same handoff queue the host load path uses
                        // (the queue is what avoids aliasing
                        // `process()`'s `&mut plugin`). No params ride
                        // along: the editor mutates params through
                        // `set_param`.
                        let _ = pending_state_for_set.force_push(state::DeserializedState {
                            params: Vec::new(),
                            extra: Some(bytes),
                            persist: Vec::new(),
                        });
                    }),
                    transport: Box::new(move || transport_slot.read()),
                },
                params_for_ctx,
            )
            .with_tasks(task_spawner_for_ctx);
            #[cfg(target_os = "macos")]
            let handle = RawWindowHandle::AppKit(parent);
            #[cfg(target_os = "windows")]
            let handle = RawWindowHandle::Win32(parent);
            #[cfg(target_os = "linux")]
            let handle = RawWindowHandle::X11(parent as u64);

            editor.open(handle, context);
        }
    });
}

unsafe extern "C" fn cb_gui_close<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    // `editor.close` runs author teardown code that can panic; firewall it.
    run_extern_callback_with::<P, ()>("vst3", "gui_close", (), || unsafe {
        let inst = &mut *ctx.cast::<Vst3Instance<P>>();
        if let Some(ref mut editor) = inst.editor {
            editor.close();
        }
    });
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Plugin display-name surfaced as `PClassInfo::name`. Reads
/// `truce.toml`'s `vst3_name` (baked into `PluginInfo` by
/// `truce::plugin_info!`), falling back to `PluginInfo::name`.
fn resolved_plugin_name(info: &PluginInfo) -> &'static str {
    resolve_name_override(info.vst3_name, info.name)
}

/// Per-direction channel-pointer capacity of the shim's `process()`
/// arrays (`kMaxProcChannels` in `vst3_shim.cpp`). That path is the
/// audio callback, so the arrays are fixed rather than heap-sized; a
/// plugin whose widest declared layout exceeds this can't be rendered
/// without silently truncating channels. Registration rejects it here
/// rather than advertise a layout the audio path can't honor. Bus
/// *counts* have no such limit - `setBusArrangements()` sizes its
/// per-bus arrays dynamically.
const VST3_MAX_CHANNELS_PER_DIRECTION: u32 = 32;

/// Largest total input and output channel counts across all declared
/// layouts - the widest the host can negotiate, hence the widest the
/// process path must handle.
fn max_layout_channels(layouts: &[BusLayout]) -> (u32, u32) {
    layouts.iter().fold((0, 0), |(mi, mo), l| {
        (
            mi.max(l.total_input_channels()),
            mo.max(l.total_output_channels()),
        )
    })
}

pub fn register_vst3<P: PluginExport>() {
    // Called from the export macro's `extern "C" fn init()` static
    // initializer. Catch any panic so it doesn't cross the FFI
    // boundary and abort the host process.
    run_register::<P>("VST3", || {
        let Some((num_inputs, num_outputs)) = default_io_channels::<P>() else {
            log_missing_bus_layout::<P>("VST3");
            return;
        };
        let (max_in, max_out) = max_layout_channels(&P::bus_layouts());
        if max_in > VST3_MAX_CHANNELS_PER_DIRECTION || max_out > VST3_MAX_CHANNELS_PER_DIRECTION {
            eprintln!(
                "[truce VST3] {} declares up to {max_in} input / {max_out} output channels, \
                 exceeding the shim's {VST3_MAX_CHANNELS_PER_DIRECTION}-channel-per-direction \
                 process limit - plugin will not register.",
                std::any::type_name::<P>(),
            );
            return;
        }
        register_vst3_inner::<P>(num_inputs, num_outputs);
    });
}

// VST3 `ParameterInfo::ParameterFlags`. Only these bits are defined by
// the SDK (3.7); the previous code set a reserved `1 << 8`, which no host
// interprets, and left the read-only / hidden / list bits unmapped.
const VST3_PARAM_CAN_AUTOMATE: i32 = 1 << 0;
const VST3_PARAM_IS_READ_ONLY: i32 = 1 << 1;
const VST3_PARAM_IS_LIST: i32 = 1 << 3;
/// `kIsHidden` (SDK 3.7+): the parameter is not shown in generic editors
/// or automation pickers. Hosts still write to hidden ids through
/// `IParameterChanges`, which is how the MIDI proxy bank receives its
/// `IMidiMapping`-resolved controllers.
const VST3_PARAM_IS_HIDDEN: i32 = 1 << 4;
const VST3_PARAM_IS_BYPASS: i32 = 1 << 16;

// Assembles the descriptor, param descriptors, and callback table in one
// linear pass; splitting it further would scatter the one-time registration
// wiring across helpers that each read once.
#[allow(clippy::too_many_lines)]
fn register_vst3_inner<P: PluginExport>(num_inputs: u32, num_outputs: u32) {
    let info = P::info();
    // Static metadata path: derive emits a `LazyLock`-cached
    // `Vec<ParamInfo>` so registration skips the
    // `Self::create().params().param_infos()` walk and the plugin
    // construction it implies. Hand-written `PluginExport` impls
    // without a `Params::param_infos_static` override fall back to
    // the historical runtime path inside `PluginExport`'s default
    // impl.
    let param_infos = P::param_infos_static();

    let mut param_descs: Vec<Vst3ParamDescriptor> = Vec::with_capacity(param_infos.len());
    for pi in &param_infos {
        let cs = ParamCStrings::from_info(pi);

        let mut flags: i32 = 0;
        if pi.flags.contains(ParamFlags::AUTOMATABLE) {
            flags |= VST3_PARAM_CAN_AUTOMATE;
        }
        if pi.flags.contains(ParamFlags::READONLY) {
            flags |= VST3_PARAM_IS_READ_ONLY;
        }
        if pi.flags.contains(ParamFlags::HIDDEN) {
            flags |= VST3_PARAM_IS_HIDDEN;
        }
        if pi.flags.contains(ParamFlags::IS_BYPASS) {
            flags |= VST3_PARAM_IS_BYPASS;
        }
        // An enum is a named, indexed value list: `kIsList` makes the host
        // render a value dropdown (populated via `getParamStringByValue`)
        // instead of a knob. Discreteness for int / discrete-float params
        // is carried by the `step_count` descriptor field below, which is
        // what VST3 reads for step navigation - there's no separate flag.
        if matches!(pi.range, ParamRange::Enum { .. }) {
            flags |= VST3_PARAM_IS_LIST;
        }
        let step_count = pi.range.step_count();

        param_descs.push(Vst3ParamDescriptor {
            id: pi.id,
            name: cs.name.into_raw(),
            short_name: cs.short_name.into_raw(),
            units: cs.unit.into_raw(),
            min: pi.range.min(),
            max: pi.range.max(),
            default_normalized: pi.range.normalize(pi.default_plain),
            // Param step counts come from `IntParam`/`EnumParam` ranges,
            // bounded well below i32::MAX in practice.
            #[allow(clippy::cast_possible_wrap)]
            step_count: step_count.map_or(0, |n| n.get() as i32),
            flags,
            group: cs.group.into_raw(),
        });
    }

    // Hidden MIDI input proxies (see the MIDI-proxy block above):
    // appended *after* the real params so the shim's index-based
    // structures (unit table, ParameterInfo enumeration) keep their
    // positions. One bank per declared MIDI input port so multi-port
    // plugins keep controllers attributed per bus. `kIsHidden` (with
    // `kCanAutomate` clear) keeps the bank out of generic editors and
    // automation pickers - flags 0 alone left thousands of "MIDI Ch N
    // CC M" rows listed; hosts still deliver `IMidiMapping`-resolved
    // changes to hidden ids. Identity 0..=1 range, grouped under a
    // "MIDI" unit. The CStrings intentionally leak - registration
    // runs once per process, matching the real params' `into_raw`
    // pattern.
    if info.accepts_midi_in {
        let empty_units = || CString::default().into_raw();
        for port in 0..info.midi_input_ports {
            // Single-port plugins keep the unprefixed names hosts
            // already display; only multi-port names carry the bus.
            let (name_prefix, short_prefix) = if info.midi_input_ports > 1 {
                (format!("MIDI In {} ", port + 1), format!("I{}", port + 1))
            } else {
                (String::from("MIDI "), String::new())
            };
            for channel in 0u8..16 {
                for controller in 0..MIDI_PROXY_PER_CHANNEL {
                    let (name, short) = match controller {
                        MIDI_PROXY_PITCH_BEND => (
                            format!("{name_prefix}Ch {} Pitch Bend", channel + 1),
                            format!("{short_prefix}M{}PB", channel + 1),
                        ),
                        MIDI_PROXY_PRESSURE => (
                            format!("{name_prefix}Ch {} Pressure", channel + 1),
                            format!("{short_prefix}M{}Pr", channel + 1),
                        ),
                        cc => (
                            format!("{name_prefix}Ch {} CC {cc}", channel + 1),
                            format!("{short_prefix}M{}C{cc}", channel + 1),
                        ),
                    };
                    param_descs.push(Vst3ParamDescriptor {
                        id: midi_proxy_id(port, channel, controller),
                        name: CString::new(name).unwrap_or_default().into_raw(),
                        short_name: CString::new(short).unwrap_or_default().into_raw(),
                        units: empty_units(),
                        min: 0.0,
                        max: 1.0,
                        default_normalized: midi_proxy_default(controller),
                        step_count: 0,
                        flags: VST3_PARAM_IS_HIDDEN,
                        group: CString::new("MIDI").unwrap_or_default().into_raw(),
                    });
                }
            }
        }
    }

    let name = CString::new(resolved_plugin_name(&info)).unwrap_or_default();
    let vendor = CString::new(info.vendor).unwrap_or_default();
    let url = CString::new(info.url).unwrap_or_default();
    let version = CString::new(info.version).unwrap_or_default();
    let category = CString::new("Audio Module Class").unwrap_or_default();
    // VST3 "Plugin Type Categories": Cubase (and other VST3 hosts)
    // route plugins into submenus based on a `<primary>|<secondary>`
    // pair from the SDK's published vocabulary. `Fx` alone advertises
    // the plug-in as "an effect of unspecified kind" and falls back
    // to the "Other" bucket; a secondary token like `Delay`, `Reverb`,
    // `EQ`, `Modulation`, etc. routes to the matching submenu.
    //
    // The Analyzer / NoteEffect / Tool categories already carry their
    // own implicit secondary token (`Fx|Analyzer`, `Fx|Event`,
    // `Fx|Tools`). For instruments and generic effects, the secondary
    // is opt-in via `truce.toml`'s `vst3_subcategory`. When unset the
    // wrapper ships the bare primary so the plug-in still loads, just
    // unbucketed.
    let subcategory_str = match (info.category, info.vst3_subcategory) {
        (PluginCategory::Instrument, Some(sub)) => format!("Instrument|{sub}"),
        (PluginCategory::Instrument, None) => "Instrument|Synth".to_string(),
        (PluginCategory::Effect, Some(sub)) => format!("Fx|{sub}"),
        (PluginCategory::Effect, None) => "Fx".to_string(),
        (PluginCategory::NoteEffect, _) => "Fx|Event".to_string(),
        (PluginCategory::Analyzer, _) => "Fx|Analyzer".to_string(),
        (PluginCategory::Tool, _) => "Fx|Tools".to_string(),
    };
    let subcategories = CString::new(subcategory_str).unwrap_or_default();

    // MIDI port counts are decided once on `PluginInfo` (category
    // default, overridable via `midi_input` / `midi_output` /
    // `midi_input_ports` / `midi_output_ports` in truce.toml). The shim
    // advertises this many event buses per direction.
    let midi_output_ports = i32::from(info.midi_output_ports);
    let midi_input_ports = i32::from(info.midi_input_ports);

    // Per-bus structure from the first declared layout (bus count + kind
    // are consistent across a plugin's layouts; only widths vary).
    let (num_input_buses, num_output_buses, input_bus_kinds, output_bus_kinds) =
        descriptor_buses::<P>();

    let descriptor = Box::leak(Box::new(Vst3PluginDescriptor {
        name: name.into_raw(),
        vendor: vendor.into_raw(),
        url: url.into_raw(),
        email: std::ptr::null(),
        version: version.into_raw(),
        cid: state::vst3_cid(info.vst3_id),
        category: category.into_raw(),
        subcategories: subcategories.into_raw(),
        num_inputs,
        num_outputs,
        num_input_buses,
        num_output_buses,
        input_bus_kinds,
        output_bus_kinds,
        midi_output_ports,
        midi_input_ports,
        supports_f64: i32::from(<P as PluginRuntime>::Sample::IS_F64),
    }));

    let callbacks = Box::leak(Box::new(Vst3Callbacks {
        create: cb_create::<P>,
        destroy: cb_destroy::<P>,
        reset: cb_reset::<P>,
        process: cb_process::<P>,
        process_f64: cb_process_f64::<P>,
        param_count: cb_param_count::<P>,
        param_get_value: cb_param_get_value::<P>,
        param_set_value: cb_param_set_value::<P>,
        param_normalize: cb_param_normalize::<P>,
        param_denormalize: cb_param_denormalize::<P>,
        param_format: cb_param_format::<P>,
        param_parse: cb_param_parse::<P>,
        state_save: cb_state_save::<P>,
        state_load: cb_state_load::<P>,
        state_free: cb_state_free,
        get_latency: cb_get_latency::<P>,
        get_tail: cb_get_tail::<P>,
        get_output_event_count: cb_get_output_event_count::<P>,
        get_output_event: cb_get_output_event::<P>,
        push_sysex_input: cb_push_sysex_input::<P>,
        get_output_sysex_count: cb_get_output_sysex_count::<P>,
        get_output_sysex_event: cb_get_output_sysex_event::<P>,
        get_output_note_expression_count: cb_get_output_note_expression_count::<P>,
        get_output_note_expression: cb_get_output_note_expression::<P>,
        gui_has_editor: cb_gui_has_editor::<P>,
        gui_get_size: cb_gui_get_size::<P>,
        gui_open: cb_gui_open::<P>,
        gui_close: cb_gui_close::<P>,
        gui_set_content_scale: cb_gui_set_content_scale::<P>,
        gui_can_resize: cb_gui_can_resize::<P>,
        gui_check_size_constraint: cb_gui_check_size_constraint::<P>,
        gui_set_size: cb_gui_set_size::<P>,
        midi_mapping_get_param_id: cb_midi_mapping_get_param_id::<P>,
        get_output_param_count: cb_get_output_param_count::<P>,
        get_output_param: cb_get_output_param::<P>,
        set_active: cb_set_active::<P>,
        match_bus_layout: cb_match_bus_layout::<P>,
        layout_bus_channels: cb_layout_bus_channels::<P>,
        match_bus_layout_perbus: cb_match_bus_layout_perbus::<P>,
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
            std::ptr::from_ref::<Vst3PluginDescriptor>(descriptor),
            std::ptr::from_ref::<Vst3Callbacks>(callbacks),
            param_descs.as_ptr(),
            len_u32(param_descs.len()),
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

#[cfg(test)]
mod midi_proxy_tests {
    use super::{
        MIDI_PROXY_ID_BASE, MIDI_PROXY_PER_CHANNEL, MIDI_PROXY_PITCH_BEND, MIDI_PROXY_PRESSURE,
        midi_proxy_decode, midi_proxy_default, midi_proxy_event, midi_proxy_id,
    };
    use truce_core::events::EventBody;

    #[test]
    fn id_round_trips_across_the_banks() {
        // Every (port, channel, controller) triple survives the trip -
        // multi-timbral hosts rely on the port dimension to keep each
        // bus's controllers separate.
        for port in [0u8, 1, 3, 255] {
            for channel in 0u8..16 {
                for controller in 0..MIDI_PROXY_PER_CHANNEL {
                    let id = midi_proxy_id(port, channel, controller);
                    assert_eq!(midi_proxy_decode(id), Some((port, channel, controller)));
                }
            }
        }
    }

    #[test]
    fn ports_get_distinct_ids() {
        // The whole point of per-port banks: the same (channel, cc)
        // on two buses must be two parameter queues host-side.
        assert_ne!(midi_proxy_id(0, 4, 74), midi_proxy_id(1, 4, 74));
    }

    #[test]
    fn real_param_ids_never_decode() {
        // Hash ids live below METER_ID_BASE, meters just above it -
        // both far under the proxy base.
        const _: () = assert!(MIDI_PROXY_ID_BASE > truce_params::METER_ID_BASE);
        assert_eq!(midi_proxy_decode(0), None);
        assert_eq!(midi_proxy_decode(truce_params::METER_ID_BASE), None);
        assert_eq!(midi_proxy_decode(MIDI_PROXY_ID_BASE - 1), None);
        // One past the last bank is out again.
        assert_eq!(
            midi_proxy_decode(midi_proxy_id(255, 15, MIDI_PROXY_PER_CHANNEL - 1) + 1),
            None
        );
    }

    #[test]
    fn pitch_bend_endpoints_and_center() {
        let bend = |norm: f32| match midi_proxy_event(3, MIDI_PROXY_PITCH_BEND, norm) {
            EventBody::PitchBend { channel, value, .. } => {
                assert_eq!(channel, 3);
                value
            }
            other => panic!("expected PitchBend, got {other:?}"),
        };
        assert_eq!(bend(0.0), 0);
        assert_eq!(bend(0.5), 8192);
        assert_eq!(bend(1.0), 16383);
    }

    #[test]
    fn cc_and_pressure_decode_to_their_events() {
        match midi_proxy_event(0, 74, 1.0) {
            EventBody::ControlChange { cc, value, .. } => {
                assert_eq!(cc, 74);
                assert_eq!(value, 127);
            }
            other => panic!("expected ControlChange, got {other:?}"),
        }
        match midi_proxy_event(9, MIDI_PROXY_PRESSURE, 0.0) {
            EventBody::ChannelPressure {
                channel, pressure, ..
            } => {
                assert_eq!(channel, 9);
                assert_eq!(pressure, 0);
            }
            other => panic!("expected ChannelPressure, got {other:?}"),
        }
    }

    #[test]
    fn defaults_center_only_the_wheel() {
        assert!((midi_proxy_default(MIDI_PROXY_PITCH_BEND) - 0.5).abs() < f64::EPSILON);
        assert!(midi_proxy_default(0).abs() < f64::EPSILON);
        assert!(midi_proxy_default(MIDI_PROXY_PRESSURE).abs() < f64::EPSILON);
    }
}

#[cfg(test)]
mod channel_limit_tests {
    use super::{VST3_MAX_CHANNELS_PER_DIRECTION, max_layout_channels};
    use truce_core::bus::{BusLayout, ChannelConfig};

    #[test]
    fn widest_layout_wins() {
        let layouts = [BusLayout::stereo(), BusLayout::mono()];
        assert_eq!(max_layout_channels(&layouts), (2, 2));
    }

    #[test]
    fn sums_channels_across_buses() {
        // Main stereo + stereo sidechain = 4 input channels, 2 output.
        let layouts =
            [BusLayout::stereo().with_sidechain_input("Sidechain", ChannelConfig::Stereo)];
        assert_eq!(max_layout_channels(&layouts), (4, 2));
    }

    #[test]
    fn many_mono_buses_stay_within_limit() {
        // 17 mono input buses = 17 channels, under the 32-channel cap - so a
        // high bus count alone never trips the guard, and (with the dynamic
        // setBusArrangements arrays) such a plugin negotiates all its buses.
        let mut layout = BusLayout::new()
            .with_input("Main", ChannelConfig::Mono)
            .with_output("Main", ChannelConfig::Mono);
        for _ in 1..17 {
            layout = layout.with_sidechain_input("Aux", ChannelConfig::Mono);
        }
        let (max_in, _) = max_layout_channels(&[layout]);
        assert_eq!(max_in, 17);
        assert!(max_in <= VST3_MAX_CHANNELS_PER_DIRECTION);
    }

    #[test]
    fn oversized_layout_exceeds_limit() {
        // 17 stereo buses = 34 channels > 32: registration rejects this.
        let mut layout = BusLayout::new()
            .with_input("Main", ChannelConfig::Stereo)
            .with_output("Main", ChannelConfig::Stereo);
        for _ in 1..17 {
            layout = layout.with_sidechain_input("Aux", ChannelConfig::Stereo);
        }
        let (max_in, _) = max_layout_channels(&[layout]);
        assert_eq!(max_in, 34);
        assert!(max_in > VST3_MAX_CHANNELS_PER_DIRECTION);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use truce_core::events::EventBody;
    use truce_params::{MidiSource, ParamFlags, ParamUnit, ParamValueKind};

    fn info(range: ParamRange, midi_map: Option<MidiSource>) -> ParamInfo {
        ParamInfo {
            id: 1,
            name: "p",
            short_name: "p",
            group: "",
            range,
            default_plain: 0.0,
            flags: ParamFlags::AUTOMATABLE,
            unit: ParamUnit::None,
            kind: ParamValueKind::Float,
            midi_map,
            midi_channel: None,
        }
    }

    /// Bridge a param change the way `process_block` does: only a param
    /// with a `midi_map` produces an event.
    fn bridge(info: &ParamInfo, plain: f64) -> Option<EventBody> {
        MidiMap::from_param(info).map(|m| midi_event_from_map(&m, plain))
    }

    #[test]
    fn unmapped_param_does_not_bridge() {
        let i = info(ParamRange::Linear { min: 0.0, max: 1.0 }, None);
        assert!(bridge(&i, 0.5).is_none());
    }

    #[test]
    fn midi_map_cache_holds_only_mapped_ids_and_binary_searches() {
        // The `process_block` fast path: build the sorted cache the way
        // `cb_create` does, then look up by id. Unmapped params are
        // absent, so their ids (and unknown ids) miss.
        let range = ParamRange::Linear {
            min: 0.0,
            max: 127.0,
        };
        let mut mapped_cc = info(range, Some(MidiSource::Cc(74)));
        mapped_cc.id = 5;
        let mut unmapped = info(range, None);
        unmapped.id = 2;
        let mut mapped_bend = info(range, Some(MidiSource::PitchBend));
        mapped_bend.id = 9;

        let mut cache: Vec<(u32, MidiMap)> = [&mapped_cc, &unmapped, &mapped_bend]
            .into_iter()
            .filter_map(|i| MidiMap::from_param(i).map(|m| (i.id, m)))
            .collect();
        cache.sort_by_key(|(id, _)| *id);

        assert_eq!(cache.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [5, 9]);

        let find = |id: u32| cache.binary_search_by_key(&id, |(i, _)| *i);
        // The mapped CC bridges to a ControlChange on its number.
        let idx = find(5).expect("mapped id 5 present");
        assert!(matches!(
            midi_event_from_map(&cache[idx].1, 127.0),
            EventBody::ControlChange { cc: 74, .. }
        ));
        assert!(find(9).is_ok(), "mapped id 9 present");
        assert!(find(2).is_err(), "unmapped id absent");
        assert!(find(999).is_err(), "unknown id absent");
    }

    #[test]
    fn note_expression_maps_per_note_cc_and_bend() {
        // Volume CC (7) -> VST3 type 0; noteId = (channel<<7)|note.
        let (type_id, note_id, value) = note_expression_of(&EventBody::PerNoteCC {
            group: 0,
            channel: 2,
            note: 60,
            cc: 7,
            value: u32::MAX,
            registered: true,
        })
        .expect("volume maps");
        assert_eq!(type_id, 0);
        assert_eq!(note_id, vst3_note_id(2, 60));
        assert!((value - 1.0).abs() < 1e-9);

        // Pitch bend -> tuning (type 2), center value ~0.5.
        let (type_id, _, value) = note_expression_of(&EventBody::PerNotePitchBend {
            group: 0,
            channel: 0,
            note: 64,
            value: 0x8000_0000,
        })
        .expect("bend maps");
        assert_eq!(type_id, 2);
        assert!((value - 0.5).abs() < 1e-3);

        // Full-scale wire bend is ±48 st; VST3's tuning norm spans
        // ±120 st, so it must land at 0.5 + 48/240 = 0.7, not 1.0.
        let (_, _, value) = note_expression_of(&EventBody::PerNotePitchBend {
            group: 0,
            channel: 0,
            note: 64,
            value: u32::MAX,
        })
        .expect("bend maps");
        assert!((value - 0.7).abs() < 1e-6);

        // A CC with no predefined VST3 note-expression type is skipped.
        assert!(
            note_expression_of(&EventBody::PerNoteCC {
                group: 0,
                channel: 0,
                note: 60,
                cc: 20,
                value: 0,
                registered: true,
            })
            .is_none()
        );
    }

    #[test]
    fn assignable_per_note_cc_is_not_an_expression() {
        // Only registered per-note indices carry the predefined
        // expression semantics; an assignable index 7 is not volume.
        assert!(
            note_expression_of(&EventBody::PerNoteCC {
                group: 0,
                channel: 0,
                note: 60,
                cc: 7,
                value: u32::MAX,
                registered: false,
            })
            .is_none()
        );
    }

    #[test]
    fn note_id_is_deterministic() {
        assert_eq!(vst3_note_id(0, 0), 0);
        assert_eq!(vst3_note_id(2, 60), 0x013C); // (2 << 7) | 60
        assert_eq!(vst3_note_id(15, 127), 0x07FF); // (15 << 7) | 127
    }

    #[test]
    fn midi_event_layout_matches_shim() {
        // The C++ shim static_asserts the same shape; a drift on either
        // side fails its build or this test.
        assert_eq!(std::mem::size_of::<Vst3MidiEvent>(), 24);
        assert_eq!(std::mem::align_of::<Vst3MidiEvent>(), 8);
    }

    #[test]
    fn note_id_map_scopes_ids_per_port() {
        let mut map = NoteIdMap::new();
        // Host counters are arbitrary - nothing like the pitch - and
        // scoped per event bus: the same id on two buses is two
        // distinct voices.
        map.insert(0, 90210, 3, 64);
        map.insert(1, 90210, 5, 72);
        assert_eq!(map.lookup(0, 90210), Some((3, 64)));
        assert_eq!(map.lookup(1, 90210), Some((5, 72)));
        assert_eq!(map.lookup(2, 90210), None);
        assert_eq!(map.lookup(0, 64), None); // pitch is not a key
        // Unassigned ids never enter the map.
        map.insert(0, -1, 0, 60);
        assert_eq!(map.lookup(0, -1), None);
        // A reset drops every correlation.
        map.clear();
        assert_eq!(map.lookup(0, 90210), None);
        assert_eq!(map.lookup(1, 90210), None);
    }

    #[test]
    fn note_id_map_overflow_overwrites_oldest() {
        let mut map = NoteIdMap::new();
        for i in 0..NoteIdMap::CAPACITY {
            // Bounded by CAPACITY = 128, fits in both domains.
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            map.insert(0, 1000 + i as i32, 0, i as u8);
        }
        // Full map: the next insert takes the round-robin slot rather
        // than being dropped, and the newest entry resolves.
        map.insert(0, 5000, 1, 72);
        assert_eq!(map.lookup(0, 5000), Some((1, 72)));
        // Entries outlive their note-off by design, so a full map of
        // released voices still can't wedge it: stale ids fall to the
        // round-robin overwrite.
        for i in 0..NoteIdMap::CAPACITY {
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            map.insert(0, 6000 + i as i32, 2, i as u8);
        }
        assert_eq!(map.lookup(0, 1000), None);
    }

    #[test]
    fn tuning_norm_round_trips_and_saturates() {
        // Center and mid-range survive the domain re-scale both ways.
        assert_eq!(vst3_tuning_to_wire(0.5), 0x8000_0000);
        assert!((wire_to_vst3_tuning(0x8000_0000) - 0.5).abs() < 1e-9);
        let wire = vst3_tuning_to_wire(0.55); // +12 st
        assert!((wire_to_vst3_tuning(wire) - 0.55).abs() < 1e-6);
        // A host bend past the wire's ±48 st saturates.
        assert_eq!(vst3_tuning_to_wire(1.0), u32::MAX);
        assert_eq!(vst3_tuning_to_wire(0.0), 0);
    }

    #[test]
    fn unmapped_per_note_cc_degrades_to_channel_cc() {
        // No predefined VST3 expression type for cc 20 - it must fall
        // through to the 1.0 downconvert as a channel CC (matching
        // CLAP), not vanish.
        let event = Event::new(
            0,
            EventBody::PerNoteCC {
                group: 0,
                channel: 3,
                note: 60,
                cc: 20,
                value: u32::MAX,
                registered: true,
            },
        );
        let packet = try_encode_vst3_midi(&event).expect("degrades to channel CC");
        assert_eq!(packet.status, 0xB3);
        assert_eq!(packet.data1, 20);
        assert_eq!(packet.data2, 127);

        // Mapped per-note events ride note expression instead - the
        // MIDI encoder must skip them or they'd double-emit.
        let mapped = Event::new(
            0,
            EventBody::PerNoteCC {
                group: 0,
                channel: 0,
                note: 60,
                cc: 7,
                value: 0,
                registered: true,
            },
        );
        assert!(try_encode_vst3_midi(&mapped).is_none());
        let bend = Event::new(
            0,
            EventBody::PerNotePitchBend {
                group: 0,
                channel: 0,
                note: 60,
                value: 0,
            },
        );
        assert!(try_encode_vst3_midi(&bend).is_none());
    }

    #[test]
    fn unit_conversion_round_trips_full_precision() {
        // A centered tuning value must survive the crossing exactly -
        // the old 7-bit path decoded 0.5 as ~0.496 (about a semitone
        // flat over the +/-120 st tuning domain).
        let center = unit_to_u32(0.5);
        assert!((u32_to_unit(center) - 0.5).abs() < 1e-9);
        assert_eq!(unit_to_u32(0.0), 0);
        assert_eq!(unit_to_u32(1.0), u32::MAX);
        // FFI hygiene: out-of-domain hosts get clamped, not wrapped.
        assert_eq!(unit_to_u32(-0.25), 0);
        assert_eq!(unit_to_u32(1.5), u32::MAX);
        assert_eq!(unit_to_u32(f64::NAN), 0);
    }

    #[test]
    fn output_encode_carries_port() {
        // The plug-in stamps an outbound event's MIDI port; the shim
        // reads it back off `Vst3MidiEvent::port` to pick the event bus.
        let event = Event::on_port(
            5,
            2,
            EventBody::NoteOn {
                group: 0,
                channel: 0,
                note: 60,
                velocity: 100,
            },
        );
        let packet = try_encode_vst3_midi(&event).expect("note-on encodes");
        assert_eq!(packet.port, 2);
    }

    #[test]
    fn pitch_bend_maps_wheel_position_to_14bit() {
        // The synth's binding range: -1..1, where the host's
        // normalized 0/0.5/1 wheel positions land on plain -1/0/1.
        let i = info(
            ParamRange::Linear {
                min: -1.0,
                max: 1.0,
            },
            Some(MidiSource::PitchBend),
        );

        // Center wheel -> 8192.
        assert!(matches!(
            bridge(&i, 0.0),
            Some(EventBody::PitchBend { value: 8192, .. })
        ));
        // Full down -> 0, full up -> 16383.
        assert!(matches!(
            bridge(&i, -1.0),
            Some(EventBody::PitchBend { value: 0, .. })
        ));
        assert!(matches!(
            bridge(&i, 1.0),
            Some(EventBody::PitchBend { value: 16383, .. })
        ));
    }

    #[test]
    fn cc_and_pressure_and_program_map_to_7bit() {
        let cc = info(
            ParamRange::Linear { min: 0.0, max: 1.0 },
            Some(MidiSource::Cc(74)),
        );
        assert!(matches!(
            bridge(&cc, 1.0),
            Some(EventBody::ControlChange {
                cc: 74,
                value: 127,
                ..
            })
        ));

        let pressure = info(
            ParamRange::Linear { min: 0.0, max: 1.0 },
            Some(MidiSource::ChannelPressure),
        );
        assert!(matches!(
            bridge(&pressure, 0.0),
            Some(EventBody::ChannelPressure { pressure: 0, .. })
        ));

        let program = info(
            ParamRange::Linear { min: 0.0, max: 1.0 },
            Some(MidiSource::ProgramChange),
        );
        assert!(matches!(
            bridge(&program, 1.0),
            Some(EventBody::ProgramChange { program: 127, .. })
        ));
    }
}
