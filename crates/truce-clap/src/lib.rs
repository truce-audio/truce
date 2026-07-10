//! CLAP format wrapper for the truce framework.
//!
//! Provides the [`export_clap!`] macro to expose any
//! `PluginExport` implementation as a CLAP plugin.

// Several `extern "C" fn`s in the CLAP vtable carry a `<P: PluginExport>`
// type parameter even though they don't use `P`. The vtable is built per-`P`
// inside the `export_clap!` macro and uniformity across the table simplifies
// the macro; removing `P` from individual entries would make the macro
// branch on which functions are generic.
#![allow(clippy::extra_unused_type_parameters)]
// CLAP event headers are 4-byte aligned but the extended event types
// (note, param_value, transport, …) require 8-byte alignment. The
// host guarantees the underlying buffer is allocated with the
// extended event's alignment; the `header.cast::<…>()` is reading
// through that promise.
#![allow(clippy::cast_ptr_alignment)]

#[doc(hidden)]
pub mod __macro_deps {
    pub use truce_core;
}

pub mod presets;

use std::ffi::{CStr, CString, c_char, c_void};
use std::marker::PhantomData;
use std::mem::transmute;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_IS_LIVE, CLAP_EVENT_MIDI, CLAP_EVENT_MIDI_SYSEX,
    CLAP_EVENT_MIDI2, CLAP_EVENT_NOTE_CHOKE, CLAP_EVENT_NOTE_EXPRESSION, CLAP_EVENT_NOTE_OFF,
    CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END,
    CLAP_EVENT_PARAM_MOD, CLAP_EVENT_PARAM_VALUE, CLAP_EVENT_TRANSPORT,
    CLAP_NOTE_EXPRESSION_BRIGHTNESS, CLAP_NOTE_EXPRESSION_EXPRESSION, CLAP_NOTE_EXPRESSION_PAN,
    CLAP_NOTE_EXPRESSION_PRESSURE, CLAP_NOTE_EXPRESSION_TUNING, CLAP_NOTE_EXPRESSION_VIBRATO,
    CLAP_NOTE_EXPRESSION_VOLUME, CLAP_TRANSPORT_HAS_BEATS_TIMELINE,
    CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, CLAP_TRANSPORT_HAS_TEMPO,
    CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_LOOP_ACTIVE, CLAP_TRANSPORT_IS_PLAYING,
    CLAP_TRANSPORT_IS_RECORDING, clap_event_header, clap_event_midi, clap_event_midi_sysex,
    clap_event_midi2, clap_event_note, clap_event_note_expression, clap_event_param_gesture,
    clap_event_param_value, clap_event_transport, clap_input_events, clap_output_events,
};
use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORT_IS_MAIN, CLAP_AUDIO_PORT_PREFERS_64BITS, CLAP_AUDIO_PORT_SUPPORTS_64BITS,
    CLAP_EXT_AUDIO_PORTS, CLAP_PORT_MONO, CLAP_PORT_STEREO, clap_audio_port_info,
    clap_plugin_audio_ports,
};
use clap_sys::ext::audio_ports_config::{
    CLAP_EXT_AUDIO_PORTS_CONFIG, clap_audio_ports_config, clap_plugin_audio_ports_config,
};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_host_latency, clap_plugin_latency};
use clap_sys::ext::note_ports::{
    CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, CLAP_NOTE_DIALECT_MIDI2,
    clap_note_port_info, clap_plugin_note_ports,
};
use clap_sys::ext::params::{
    CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE, CLAP_PARAM_IS_BYPASS, CLAP_PARAM_IS_ENUM,
    CLAP_PARAM_IS_HIDDEN, CLAP_PARAM_IS_READONLY, CLAP_PARAM_IS_STEPPED, clap_param_info,
    clap_plugin_params,
};
use clap_sys::ext::params::{CLAP_PARAM_RESCAN_VALUES, clap_host_params};
use clap_sys::ext::preset_load::{
    CLAP_EXT_PRESET_LOAD, CLAP_EXT_PRESET_LOAD_COMPAT, clap_host_preset_load,
    clap_plugin_preset_load,
};
use clap_sys::ext::render::{
    CLAP_EXT_RENDER, CLAP_RENDER_OFFLINE, clap_plugin_render, clap_plugin_render_mode,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_plugin_tail};
use clap_sys::factory::preset_discovery::{
    CLAP_PRESET_DISCOVERY_LOCATION_FILE, clap_preset_discovery_location_kind,
};
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR};
use clap_sys::host::clap_host;
use clap_sys::id::{CLAP_INVALID_ID, clap_id};
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::plugin_features::{
    CLAP_PLUGIN_FEATURE_AUDIO_EFFECT, CLAP_PLUGIN_FEATURE_INSTRUMENT,
    CLAP_PLUGIN_FEATURE_NOTE_EFFECT, CLAP_PLUGIN_FEATURE_SYNTHESIZER,
};
use clap_sys::process::{
    CLAP_PROCESS_CONTINUE, CLAP_PROCESS_CONTINUE_IF_NOT_QUIET, CLAP_PROCESS_ERROR,
    CLAP_PROCESS_SLEEP, CLAP_PROCESS_TAIL, clap_process,
};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::string_sizes::{CLAP_NAME_SIZE, CLAP_PATH_SIZE};
use clap_sys::version::CLAP_VERSION;

use truce_core::TransportSlot;
use truce_core::buffer::AudioBuffer;
use truce_core::bus::ChannelConfig;
use truce_core::cast::{len_u32, size_of_u32};
use truce_core::chunked_process::{ChunkedProcess, process_chunked};
use truce_core::config::{AudioConfig, ProcessMode};
use truce_core::editor::{
    ClosureBridge, Editor, EditorBuilder, PluginContext, RawWindowHandle, SendPtr, fit_logical_size,
};
use truce_core::events::{EVENT_LIST_PREALLOC, Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::{MidiDialect, PluginCategory, PluginInfo, resolve_name_override};
use truce_core::meters::MeterStore;
use truce_core::midi::{
    PER_NOTE_VOLUME_MAX_GAIN, decode_short_message, denorm_7bit, downconvert_to_midi1,
    event_to_midi1, per_note_bend_from_semitones, per_note_bend_semitones, pitch_bend_to_bytes,
    route_midi_port,
};
use truce_core::plugin::PluginRuntime;
use truce_core::presets::parse_preset_file;
use truce_core::process::ProcessStatus;
use truce_core::rt::{RtSection, audit};
use truce_core::snapshot::SnapshotSlot;
use truce_core::state;
use truce_core::state::PluginFormat;
use truce_core::tasks::AnyTaskSpawner;
use truce_core::ump::decode_ump_channel_voice_2;
use truce_core::wrapper::{
    SharedPlugin, enter_plugin, run_audio_block_with, run_extern_callback_with, save_extra,
    shared_plugin,
};
use truce_core::{Float, Sample};
use truce_params::Params;
use truce_params::{ParamFlags, ParamInfo, ParamRange};

// ---------------------------------------------------------------------------
// GUI → host parameter change queue
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum GuiParamChange {
    GestureBegin(u32),
    Value(u32, f64), // (param_id, plain_value)
    GestureEnd(u32),
}

/// Wait-free bounded queue for GUI-initiated parameter changes.
///
/// GUI thread pushes, audio thread drains. Every push and pop is O(1)
/// and allocation-free, so the audio thread never blocks on the GUI
/// thread.
///
/// Capacity sized for the worst case "user wiggles every param at
/// once during MIDI-learn": ~64 widgets × (begin + value + end) per
/// gesture, with several blocks of headroom before the audio thread
/// next drains. Overflow drops the change - blocking or panicking
/// from the audio path is worse, and the host's next automation tick
/// recovers the lost values via the param tree. Per-instance memory
/// is `CAPACITY * sizeof(GuiParamChange)` (≈ 32 KB), one-time at
/// instance creation.
const GUI_QUEUE_CAPACITY: usize = 1024;
type GuiChangeQueue = crossbeam_queue::ArrayQueue<GuiParamChange>;

/// Bounded handoff slot for state loads. Capacity 1: presets don't
/// arrive faster than the audio thread completes a block, and on
/// overflow we want most-recent-wins (`force_push`) so a rapid
/// double-recall doesn't get the audio thread to apply a stale state
/// after the host already moved on.
type StateLoadQueue = crossbeam_queue::ArrayQueue<state::DeserializedState>;

// ---------------------------------------------------------------------------
// Internal wrapper struct held as plugin_data
// ---------------------------------------------------------------------------

struct ClapPluginData<P: PluginExport> {
    /// The user's plugin instance in the wrapper-standard ownership
    /// cell: the audio thread owns it per block, host lifecycle
    /// callbacks own it while processing is stopped, and the two never
    /// overlap. `state_save` and the editor's `get_state` read the
    /// lock-free snapshot instead, so they never touch it. See
    /// `truce_core::wrapper::SharedPlugin`.
    plugin: SharedPlugin<P>,
    /// Stable handle to the params Arc, set once at instance creation.
    /// Host-thread callbacks (`params_get_value`, `params_value_to_text`,
    /// `params_text_to_value`) read params through this handle so a
    /// param query never touches the plugin. Params are
    /// atomic-backed and `Sync`.
    params_arc: Arc<P::Params>,
    /// Shared meter storage, set once at instance creation. The
    /// editor's `get_meter` closure reads these atomic slots instead
    /// of the plugin instance.
    meter_store: Arc<MeterStore>,
    /// Lock-free custom-state slot the audio thread publishes into, read
    /// by `save_state` so a snapshot-capable plugin's save never touches
    /// the plugin. Cached here on the instance, like `params_arc`.
    snapshot: Arc<SnapshotSlot>,
    /// The plugin's background-task spawner (`None` unless it wired
    /// `tasks:`), cached at creation so the editor schedules without
    /// touching the plugin.
    task_spawner: Option<AnyTaskSpawner>,
    /// Lock-free editor factory, cached at creation. Building the editor
    /// through this never touches the plugin (`--shell` builds rebuild
    /// from the reloaded dylib, so GUI edits hot-reload).
    editor_builder: EditorBuilder<P::Params>,
    /// Atomic snapshots of the plugin's most recent `latency()` /
    /// `tail()`. Updated by the audio thread (or `init`/`reset`) so
    /// `latency_get` / `tail_get` read the value without touching
    /// `data.plugin`.
    latency_cache: AtomicU32,
    tail_cache: AtomicU32,
    /// Index into `P::bus_layouts()` of the port config the host last
    /// selected through `clap.audio-ports-config` (0 = the default first
    /// layout). Read by the audio-ports extension so a config switch is
    /// reflected in the ports the host sees. Set on the main thread while
    /// the plugin is deactivated; the audio scratch is pre-sized to the
    /// widest layout, so a wider selection never allocates in `process`.
    selected_config: AtomicU32,
    /// Re-usable event list for converting CLAP events each process call.
    event_list: EventList,
    /// Sounding-note tracker backing wildcard `NOTE_OFF` / `NOTE_CHOKE`
    /// expansion (see [`SoundingNotes`]). Audio-thread only.
    sounding_notes: SoundingNotes,
    /// Re-usable output event list for the process context.
    output_events: EventList,
    /// Per-sub-block scratch the chunker writes rebased events into
    /// while walking the audio block. Pre-allocated to the same
    /// capacity as `event_list` so steady-state operation stays
    /// allocation-free.
    sub_event_scratch: EventList,
    /// Cached parameter infos (built once at init).
    param_infos: Vec<ParamInfo>,
    /// Current sample rate.
    sample_rate: f64,
    /// Current max block size.
    max_block_size: usize,
    /// Cached plugin info. Read by the chunker each block for
    /// `automation.min_subblock_samples` and otherwise unused; the
    /// rest of the wrapper consumes `PluginInfo` from the C ABI
    /// surface we build at registration.
    info: PluginInfo,
    /// Pre-hashed plugin ID for state serialization.
    plugin_id_hash: u64,
    /// GUI editor (created by the plugin, if it implements `editor()`).
    editor: Option<Box<dyn Editor>>,
    /// Whether the GUI has been created via the gui extension.
    gui_created: bool,
    /// Host pointer (for querying host extensions).
    host: *const clap_host,
    /// Host params extension (for `request_flush`).
    host_params: *const clap_host_params,
    /// Host latency extension (for `changed`). Null if the host doesn't
    /// expose `clap.latency`.
    host_latency: *const clap_host_latency,
    /// Set on the audio thread when `latency()` changes; drained on the
    /// main thread, which notifies the host. Coalesces a burst of
    /// changes into one host notification per main-thread callback.
    latency_dirty: AtomicBool,
    /// Queue of GUI-initiated parameter changes to emit as output events.
    gui_changes: Arc<GuiChangeQueue>,
    /// Bounded SPSC handoff for state loads. Host (`state_load`) and
    /// editor (`set_state` callback) deserialize on their thread and
    /// push the result; the audio thread pops at the top of
    /// `clap_plugin_process` and calls [`state::apply_state`] under
    /// its exclusive `&mut plugin`. The queue is what makes the
    /// transfer race-free: no GUI-thread `&mut plugin` is ever
    /// constructed.
    pending_state: Arc<StateLoadQueue>,
    /// `true` between `activate` and `deactivate`. `state_load` reads it
    /// to decide whether the audio thread will drain `pending_state`: if
    /// inactive, no `process` runs, so it applies the custom-state blob
    /// synchronously instead of leaving it stranded in the queue (which
    /// would let a following `get_state` re-serialize stale extra
    /// state). Main-thread-only (`activate` / `deactivate` / `state_load`
    /// are serialized by the host).
    active: AtomicBool,
    /// Current render mode as a [`ProcessMode`] discriminant. The host
    /// sets it through the `clap.render` extension on the main thread;
    /// the audio thread reads it each block. Most hosts deactivate
    /// before switching to offline, so `activate` re-preps with the new
    /// mode; a mode change while active still reaches `process` through
    /// the per-block `ProcessContext`, but reallocation waits for the
    /// next `activate`.
    render_mode: AtomicU8,
    /// Flag: GUI changed params, need rescan on main thread.
    needs_rescan: Arc<AtomicBool>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    transport_slot: Arc<TransportSlot>,
    /// Host-reported GUI scale (via `clap_plugin_gui::set_scale`).
    /// Sources of truth, by platform:
    /// - **macOS**: ignored at `gui_get_size` (`AppKit` handles backing
    ///   scale through the parent `NSView`; we report logical points
    ///   and let the OS scale). Stored only for editors that consume
    ///   it directly via `set_scale_factor`.
    /// - **Windows / Linux**: used at `gui_get_size` to convert
    ///   logical→physical. Default `1.0` is correct for hosts that
    ///   never call `set_scale` (which by convention are non-DPI-aware
    ///   and want logical points anyway). HiDPI-aware hosts call
    ///   `set_scale` before `gui_get_size`; `host_scale_set_by_host`
    ///   records that and stops a stray future re-init from clobbering
    ///   the host-supplied value.
    host_scale: f64,
    host_scale_set_by_host: bool,
    /// Persistent input/output channel-slice scratch reused across
    /// process callbacks so the audio thread doesn't allocate per
    /// block. The `'static` annotation is fictional - the slices
    /// actually point into the host's per-block buffers; each
    /// `process` call rebuilds them and clears them on exit so no
    /// dangling pointer lives between blocks.
    input_slices: Vec<&'static [<P as PluginRuntime>::Sample]>,
    output_slices: Vec<&'static mut [<P as PluginRuntime>::Sample]>,
    /// Per-channel input conversion scratch. A slot stays empty when
    /// the host wire for its port matches `P::Sample` (the slice in
    /// `input_slices` points straight into host memory); on a
    /// precision mismatch the channel is converted into the matching
    /// slot here and the slice points there.
    input_widen: Vec<Vec<<P as PluginRuntime>::Sample>>,
    /// Per-channel output conversion scratch. Same shape: only used
    /// on a precision mismatch, in which case the plugin writes here
    /// and the wrapper copies + converts back to the host's output
    /// pointers after `process()` returns.
    output_narrow: Vec<Vec<<P as PluginRuntime>::Sample>>,
    /// Cached pointers to host output channels, captured at slice
    /// build time so the post-`process` convert-back loop can copy
    /// without re-walking the CLAP bus structures. Each entry is
    /// tagged with the wire precision the host picked for its port.
    host_out_ptrs: Vec<HostOutPtr>,
}

/// Host output channel pointer, tagged with the port's wire
/// precision (the host picks 32- or 64-bit per port; 64-bit only on
/// ports that advertised `CLAP_AUDIO_PORT_SUPPORTS_64BITS`).
#[derive(Copy, Clone)]
enum HostOutPtr {
    Null,
    F32(*mut f32),
    F64(*mut f64),
}

/// Build the plugin-facing input slice for one host channel of wire
/// precision `H`: zero-copy when `H` matches the plugin's sample
/// type `S`, otherwise converted into `scratch` (f64 round-trip,
/// lossless in the widening direction).
///
/// # Safety
/// A non-null `host_ptr` must address `num_frames` readable samples
/// that stay valid for the block; the returned slice's `'static`
/// lifetime is fictional (same convention as `input_slices`).
unsafe fn input_channel_slice<S: Sample, H: Sample>(
    host_ptr: *const H,
    num_frames: usize,
    scratch: &mut Vec<S>,
) -> &'static [S] {
    if host_ptr.is_null() {
        return &[];
    }
    unsafe {
        if S::IS_F64 == H::IS_F64 {
            // SAFETY: sealed traits - equal IS_F64 means S == H.
            return std::slice::from_raw_parts(host_ptr.cast::<S>(), num_frames);
        }
        scratch.clear();
        scratch.reserve(num_frames);
        let host = std::slice::from_raw_parts(host_ptr, num_frames);
        for &h in host {
            scratch.push(S::from_f64(h.to_f64()));
        }
        std::slice::from_raw_parts(scratch.as_ptr(), num_frames)
    }
}

/// Output twin of [`input_channel_slice`]: zero-copy into host
/// memory when precisions match, otherwise the plugin renders into
/// `scratch` and the post-`process` loop converts back to the host
/// pointer.
///
/// # Safety
/// Same contract as [`input_channel_slice`], with `num_frames`
/// writable samples.
unsafe fn output_channel_slice<S: Sample, H: Sample>(
    host_ptr: *mut H,
    num_frames: usize,
    scratch: &mut Vec<S>,
) -> &'static mut [S] {
    if host_ptr.is_null() {
        return &mut [];
    }
    unsafe {
        if S::IS_F64 == H::IS_F64 {
            // SAFETY: sealed traits - equal IS_F64 means S == H.
            return std::slice::from_raw_parts_mut(host_ptr.cast::<S>(), num_frames);
        }
        scratch.clear();
        scratch.resize(num_frames, S::default());
        std::slice::from_raw_parts_mut(scratch.as_mut_ptr(), num_frames)
    }
}

// ---------------------------------------------------------------------------
// Descriptor management
// ---------------------------------------------------------------------------

/// Holds all the C strings and the descriptor itself. Lives for the process
/// lifetime via a `static` produced by the macro.
pub struct DescriptorHolder {
    pub descriptor: clap_plugin_descriptor,
    // Prevent dropping CStrings that the descriptor points into.
    _id: CString,
    _name: CString,
    _vendor: CString,
    _url: CString,
    _version: CString,
    _features: Vec<*const c_char>,
    _features_storage: Vec<&'static CStr>,
}

unsafe impl Send for DescriptorHolder {}
unsafe impl Sync for DescriptorHolder {}

/// Plugin display-name in host browsers. Reads `truce.toml`'s
/// `clap_name` (baked into `PluginInfo` by `truce::plugin_info!`),
/// falling back to `PluginInfo::name`.
fn resolved_name(info: &PluginInfo) -> &'static str {
    resolve_name_override(info.clap_name, info.name)
}

impl DescriptorHolder {
    #[must_use]
    pub fn new(info: &PluginInfo) -> Self {
        let id = CString::new(info.clap_id).unwrap_or_default();
        let name = CString::new(resolved_name(info)).unwrap_or_default();
        let vendor = CString::new(info.vendor).unwrap_or_default();
        let url = CString::new(info.url).unwrap_or_default();
        let version = CString::new(info.version).unwrap_or_default();

        let features_storage: Vec<&'static CStr> = match info.category {
            PluginCategory::Instrument => {
                vec![
                    CLAP_PLUGIN_FEATURE_INSTRUMENT,
                    CLAP_PLUGIN_FEATURE_SYNTHESIZER,
                ]
            }
            PluginCategory::NoteEffect => vec![CLAP_PLUGIN_FEATURE_NOTE_EFFECT],
            PluginCategory::Effect | PluginCategory::Analyzer | PluginCategory::Tool => {
                vec![CLAP_PLUGIN_FEATURE_AUDIO_EFFECT]
            }
        };

        let mut features: Vec<*const c_char> =
            features_storage.iter().map(|f| f.as_ptr()).collect();
        features.push(ptr::null());

        let descriptor = clap_plugin_descriptor {
            clap_version: CLAP_VERSION,
            id: id.as_ptr(),
            name: name.as_ptr(),
            vendor: vendor.as_ptr(),
            url: url.as_ptr(),
            manual_url: ptr::null(),
            support_url: url.as_ptr(),
            version: version.as_ptr(),
            description: ptr::null(),
            features: features.as_ptr(),
        };

        Self {
            descriptor,
            _id: id,
            _name: name,
            _vendor: vendor,
            _url: url,
            _version: version,
            _features: features,
            _features_storage: features_storage,
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: copy a Rust &str into a fixed-size [c_char; N] array
// ---------------------------------------------------------------------------

fn copy_str_to_buf(dst: &mut [c_char], src: &str) {
    let bytes = src.as_bytes();
    let len = bytes.len().min(dst.len() - 1);
    for (i, &b) in bytes[..len].iter().enumerate() {
        // `c_char` is signed on most platforms; bytes ≥ 128 wrap to
        // negative values and round-trip correctly through the FFI.
        #[allow(clippy::cast_possible_wrap)]
        let c = b as c_char;
        dst[i] = c;
    }
    dst[len] = 0;
}

// ---------------------------------------------------------------------------
// Helper: get &mut ClapPluginData<P> from a *const clap_plugin
// ---------------------------------------------------------------------------

unsafe fn data_from_plugin<P: PluginExport>(
    plugin: *const clap_plugin,
) -> &'static mut ClapPluginData<P> {
    unsafe { &mut *(*plugin).plugin_data.cast::<ClapPluginData<P>>() }
}

// ---------------------------------------------------------------------------
// Plugin callbacks
//
// SAFETY for all unsafe extern "C" fn in this file:
// - `plugin` is the clap_plugin pointer returned by create_plugin_instance().
// - `(*plugin).plugin_data` is a Box::into_raw'd ClapPluginData<P>,
//   valid for the plugin's lifetime. The host guarantees it is not
//   freed until after clap_plugin.destroy() returns.
// - Audio-thread callbacks (process, start/stop_processing) have
//   exclusive access - the host never calls them concurrently.
// - Main-thread callbacks (init, destroy, activate, deactivate,
//   gui_*, params on main thread) are serialized by the host.
// - params_flush may be called from the audio thread while process
//   is not active, or from the main thread - never concurrently
//   with process().
// - Audio buffer pointers (inputs/outputs in clap_process) are
//   valid for the declared channel count × frame count. The host
//   guarantees non-aliasing between input and output buffers.
// ---------------------------------------------------------------------------

unsafe extern "C" fn clap_plugin_init<P: PluginExport>(plugin: *const clap_plugin) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        {
            let mut instance = enter_plugin(&data.plugin);
            instance.init();
            data.param_infos = instance.params().param_infos();
        }
        // Query host params extension for request_flush support
        if !data.host.is_null()
            && let Some(get_ext) = (*data.host).get_extension
        {
            let ext = get_ext(data.host, CLAP_EXT_PARAMS.as_ptr());
            if !ext.is_null() {
                data.host_params = ext.cast::<clap_host_params>();
            }
            let lat_ext = get_ext(data.host, CLAP_EXT_LATENCY.as_ptr());
            if !lat_ext.is_null() {
                data.host_latency = lat_ext.cast::<clap_host_latency>();
            }
        }
        true
    }
}

unsafe extern "C" fn clap_plugin_destroy<P: PluginExport>(plugin: *const clap_plugin) {
    unsafe {
        // Wrap the drop in `catch_unwind`. Dropping the
        // `ClapPluginData` cascades into the editor's `Drop`,
        // which tears down the wgpu surface / `NSView` /
        // baseview / runloop timers. A panic anywhere in that
        // chain propagates across this `extern "C"` boundary as
        // UB - hosts catch it as an Obj-C exception,
        // `objc_exception_rethrow` can't recover, and
        // `std::terminate` aborts the host on quit (the REAPER /
        // Cubase quit-time SIGABRT pattern). Catching here keeps
        // the host alive; the process is going away anyway so
        // swallowing the panic is fine.
        let plugin_ptr = plugin.cast_mut();
        let data_ptr = (*plugin).plugin_data.cast::<ClapPluginData<P>>();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(Box::from_raw(data_ptr));
            drop(Box::from_raw(plugin_ptr));
        }));
    }
}

unsafe extern "C" fn clap_plugin_activate<P: PluginExport>(
    plugin: *const clap_plugin,
    sample_rate: f64,
    _min_frames_count: u32,
    max_frames_count: u32,
) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        data.sample_rate = sample_rate;
        let max_block = max_frames_count as usize;
        data.max_block_size = max_block;
        let mode = ProcessMode::from_u8(data.render_mode.load(Ordering::Relaxed));
        {
            let mut instance = enter_plugin(&data.plugin);
            instance.reset(&AudioConfig::new(sample_rate, max_block).with_process_mode(mode));
        }

        // Pre-grow the widening / narrowing scratch so no host wire /
        // plugin precision combination allocates on the audio thread.
        // Without this, the first block after `activate` hits the global
        // allocator inside `clap_plugin_process` to grow the outer Vec and
        // each channel's inner Vec - a real RT hazard. The outer-Vec
        // capacity is already reserved in `create_plugin`; here we push the
        // inner per-channel `Vec<P::Sample>`s up to `max_block_size` frames
        // so the per-block `.clear() + .reserve()` path never allocates.
        //
        // Unconditional, both directions: an f64 plugin converts whenever a
        // host sends f32, and an f32 plugin converts if a (non-compliant)
        // host sends f64 to a port that never advertised
        // `SUPPORTS_64BITS`. Pre-growing for the f32 plugin keeps that
        // defensive path allocation-free too; its scratch just stays unused
        // on the common zero-copy path.
        {
            let max_in = data.input_widen.capacity();
            let max_out = data.output_narrow.capacity();
            while data.input_widen.len() < max_in {
                data.input_widen.push(Vec::with_capacity(max_block));
            }
            for buf in &mut data.input_widen {
                if buf.capacity() < max_block {
                    buf.reserve_exact(max_block - buf.capacity());
                }
            }
            while data.output_narrow.len() < max_out {
                data.output_narrow.push(Vec::with_capacity(max_block));
            }
            for buf in &mut data.output_narrow {
                if buf.capacity() < max_block {
                    buf.reserve_exact(max_block - buf.capacity());
                }
            }
            while data.host_out_ptrs.len() < max_out {
                data.host_out_ptrs.push(HostOutPtr::Null);
            }
        }

        data.active.store(true, Ordering::Relaxed);
        true
    }
}

unsafe extern "C" fn clap_plugin_deactivate<P: PluginExport>(plugin: *const clap_plugin) {
    unsafe {
        data_from_plugin::<P>(plugin)
            .active
            .store(false, Ordering::Relaxed);
    }
}

unsafe extern "C" fn clap_plugin_start_processing<P: PluginExport>(
    _plugin: *const clap_plugin,
) -> bool {
    true
}

unsafe extern "C" fn clap_plugin_stop_processing<P: PluginExport>(_plugin: *const clap_plugin) {
    // Nothing to do.
}

unsafe extern "C" fn clap_plugin_reset<P: PluginExport>(plugin: *const clap_plugin) {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        data.sounding_notes.clear_all();
        let mode = ProcessMode::from_u8(data.render_mode.load(Ordering::Relaxed));
        let mut instance = enter_plugin(&data.plugin);
        instance.reset(
            &AudioConfig::new(data.sample_rate, data.max_block_size).with_process_mode(mode),
        );
    }
}

unsafe extern "C" fn clap_plugin_on_main_thread<P: PluginExport>(plugin: *const clap_plugin) {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        if data.needs_rescan.swap(false, Ordering::Relaxed)
            && !data.host_params.is_null()
            && !data.host.is_null()
            && let Some(rescan) = (*data.host_params).rescan
        {
            rescan(data.host, CLAP_PARAM_RESCAN_VALUES);
        }

        // Latency changed on the audio thread: tell the host here, off
        // the audio thread. `changed()` re-reads our reported latency;
        // an active plugin additionally needs `request_restart` to apply
        // the new delay compensation.
        if data.latency_dirty.swap(false, Ordering::Relaxed) && !data.host.is_null() {
            if !data.host_latency.is_null()
                && let Some(changed) = (*data.host_latency).changed
            {
                changed(data.host);
            }
            if data.active.load(Ordering::Relaxed)
                && let Some(req_restart) = (*data.host).request_restart
            {
                req_restart(data.host);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Event conversion: CLAP input events -> EventList
// ---------------------------------------------------------------------------

/// 32-bit wire value -> CLAP `0..1` note-expression value.
fn unit_from_u32(v: u32) -> f64 {
    f64::from(v) / f64::from(u32::MAX)
}

/// CLAP `0..1` note-expression value -> 32-bit wire value.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn u32_from_unit(v: f64) -> u32 {
    (v.clamp(0.0, 1.0) * f64::from(u32::MAX)).round() as u32
}

/// Map a truce per-note 2.0 event to a CLAP note expression
/// `(expression_id, channel, note, value)`. `TUNING` is in semitones;
/// the rest are `0..1`.
fn clap_note_expression_of(body: &EventBody) -> Option<(i32, u8, u8, f64)> {
    match *body {
        // Registered per-note controllers only: the predefined CLAP
        // expression ids carry the registered indices' semantics
        // (7 = volume, 74 = brightness, ...); an assignable index is
        // manufacturer-defined and must not alias onto them.
        EventBody::PerNoteCC {
            channel,
            note,
            cc,
            value,
            registered: true,
            ..
        } => {
            let id = match cc {
                7 => CLAP_NOTE_EXPRESSION_VOLUME,
                10 => CLAP_NOTE_EXPRESSION_PAN,
                1 => CLAP_NOTE_EXPRESSION_VIBRATO,
                11 => CLAP_NOTE_EXPRESSION_EXPRESSION,
                74 => CLAP_NOTE_EXPRESSION_BRIGHTNESS,
                _ => return None,
            };
            // CLAP `VOLUME` is plain linear gain `0..=4` (the wire's
            // quarter point is unity); the other ids are `0..=1`.
            let value = if id == CLAP_NOTE_EXPRESSION_VOLUME {
                PER_NOTE_VOLUME_MAX_GAIN * unit_from_u32(value)
            } else {
                unit_from_u32(value)
            };
            Some((id, channel, note, value))
        }
        EventBody::PerNotePitchBend {
            channel,
            note,
            value,
            ..
        } => Some((
            CLAP_NOTE_EXPRESSION_TUNING,
            channel,
            note,
            per_note_bend_semitones(value),
        )),
        EventBody::PolyPressure2 {
            channel,
            note,
            pressure,
            ..
        } => Some((
            CLAP_NOTE_EXPRESSION_PRESSURE,
            channel,
            note,
            unit_from_u32(pressure),
        )),
        _ => None,
    }
}

/// Resolve a CLAP note event's `(channel, note)` address. CLAP carries
/// both as `i16` where `-1` is a "match all" wildcard; truce's
/// `EventBody` speaks concrete MIDI addresses (`channel 0..=15`,
/// `note 0..=127`) and plugins index tables by them, so anything
/// outside the domain - wildcards included - is dropped rather than
/// delivered mislabeled. (Wildcard `NOTE_OFF` / `NOTE_CHOKE` don't take
/// this path; see [`SoundingNotes`].)
fn clap_note_address(ne: &clap_event_note) -> Option<(u8, u8)> {
    let channel = u8::try_from(ne.channel).ok().filter(|c| *c <= 15)?;
    let note = u8::try_from(ne.key).ok().filter(|k| *k <= 127)?;
    Some((channel, note))
}

/// One wildcardable axis of a CLAP note address: `-1` matches all,
/// a concrete in-domain value matches itself, out-of-domain junk
/// matches nothing (the event drops, same as the concrete path).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoteAxis {
    All,
    One(u8),
    Invalid,
}

impl NoteAxis {
    fn parse(raw: i16, domain: u8) -> Self {
        if raw == -1 {
            return Self::All;
        }
        match u8::try_from(raw) {
            Ok(v) if v < domain => Self::One(v),
            _ => Self::Invalid,
        }
    }

    /// Concrete filter for [`SoundingNotes::drain_matching`]; `All`
    /// filters nothing.
    fn filter(self) -> Option<u8> {
        match self {
            Self::One(v) => Some(v),
            Self::All | Self::Invalid => None,
        }
    }
}

/// Bitset of currently-sounding `(channel, key)` pairs, one 16x128
/// set per MIDI input port, maintained from the decoded CLAP note
/// events. Wildcard (`-1`) `NOTE_OFF` / `NOTE_CHOKE` addresses are
/// spec-legal (`note_id`-addressing hosts release notes that way) but
/// truce's `EventBody` speaks concrete addresses only - the set
/// expands a wildcard to `NoteOff`s for exactly the sounding notes it
/// matches, so a voice can't ring forever and the expansion stays
/// bounded by real polyphony instead of the 2048-slot address space.
/// Notes started through raw-MIDI or UMP events aren't tracked; hosts
/// that address notes with wildcards start them with note events.
///
/// Known limitations, both spec-legal: the host's `note_id` is not
/// round-tripped onto output events (truce emits `note_id: -1`), and
/// `CLAP_EVENT_NOTE_END` is never emitted - so a polyphonic-modulation
/// host can't correlate a truce plugin's output notes with its own
/// voice bookkeeping, and it reclaims voices by its own timeout
/// rather than on the plugin's say-so.
struct SoundingNotes {
    /// 2048 bits (16 channels x 128 keys) per input port.
    ports: Vec<[u64; 32]>,
}

impl SoundingNotes {
    fn new(midi_input_ports: u8) -> Self {
        Self {
            ports: vec![[0u64; 32]; usize::from(midi_input_ports.max(1))],
        }
    }

    fn index(channel: u8, key: u8) -> (usize, u64) {
        let bit = usize::from(channel & 0x0F) * 128 + usize::from(key & 0x7F);
        (bit / 64, 1u64 << (bit % 64))
    }

    fn set(&mut self, port: u8, channel: u8, key: u8) {
        if let Some(bits) = self.ports.get_mut(usize::from(port)) {
            let (word, mask) = Self::index(channel, key);
            bits[word] |= mask;
        }
    }

    fn clear(&mut self, port: u8, channel: u8, key: u8) {
        if let Some(bits) = self.ports.get_mut(usize::from(port)) {
            let (word, mask) = Self::index(channel, key);
            bits[word] &= !mask;
        }
    }

    /// Forget every sounding note - the host reset all playing state,
    /// so stale bits would make a later wildcard note-off emit
    /// releases for notes that no longer exist.
    fn clear_all(&mut self) {
        for bits in &mut self.ports {
            bits.fill(0);
        }
    }

    /// Visit and clear every sounding `(channel, key)` on `port` that
    /// matches the (possibly wildcard) axis filters.
    fn drain_matching(
        &mut self,
        port: u8,
        channel: Option<u8>,
        key: Option<u8>,
        f: impl FnMut(u8, u8),
    ) {
        self.visit_matching(port, channel, key, true, f);
    }

    /// Like [`Self::drain_matching`] but leaves the visited notes
    /// sounding - expression fan-out addresses voices without
    /// releasing them.
    fn for_each_matching(
        &mut self,
        port: u8,
        channel: Option<u8>,
        key: Option<u8>,
        f: impl FnMut(u8, u8),
    ) {
        self.visit_matching(port, channel, key, false, f);
    }

    fn visit_matching(
        &mut self,
        port: u8,
        channel: Option<u8>,
        key: Option<u8>,
        clear: bool,
        mut f: impl FnMut(u8, u8),
    ) {
        let Some(bits) = self.ports.get_mut(usize::from(port)) else {
            return;
        };
        for (word_index, word) in bits.iter_mut().enumerate() {
            let mut live = *word;
            while live != 0 {
                let bit = live.trailing_zeros() as usize;
                live &= live - 1;
                let flat = word_index * 64 + bit;
                // Flat index is 0..2048, so both halves fit u8.
                #[allow(clippy::cast_possible_truncation)]
                let (ch, k) = ((flat / 128) as u8, (flat % 128) as u8);
                if channel.is_some_and(|c| c != ch) || key.is_some_and(|n| n != k) {
                    continue;
                }
                if clear {
                    *word &= !(1u64 << bit);
                }
                f(ch, k);
            }
        }
    }
}

/// Decode a CLAP note expression into its truce per-note 2.0 event,
/// addressed to one concrete `(channel, note)` voice. The event's own
/// (possibly wildcard) axes are resolved by the caller - see
/// [`push_note_expressions`].
fn note_expression_body(
    ne: &clap_event_note_expression,
    channel: u8,
    note: u8,
) -> Option<EventBody> {
    let cc = match ne.expression_id {
        CLAP_NOTE_EXPRESSION_TUNING => {
            return Some(EventBody::PerNotePitchBend {
                group: 0,
                channel,
                note,
                value: per_note_bend_from_semitones(ne.value),
            });
        }
        CLAP_NOTE_EXPRESSION_VOLUME => 7,
        CLAP_NOTE_EXPRESSION_PAN => 10,
        CLAP_NOTE_EXPRESSION_VIBRATO => 1,
        CLAP_NOTE_EXPRESSION_EXPRESSION => 11,
        CLAP_NOTE_EXPRESSION_BRIGHTNESS => 74,
        CLAP_NOTE_EXPRESSION_PRESSURE => {
            return Some(EventBody::PolyPressure2 {
                group: 0,
                channel,
                note,
                pressure: u32_from_unit(ne.value),
            });
        }
        _ => return None,
    };
    // CLAP `VOLUME` arrives as plain linear gain `0..=4`; normalize
    // into the wire domain so unity gain lands on the quarter point.
    let value = if ne.expression_id == CLAP_NOTE_EXPRESSION_VOLUME {
        u32_from_unit(ne.value / PER_NOTE_VOLUME_MAX_GAIN)
    } else {
        u32_from_unit(ne.value)
    };
    Some(EventBody::PerNoteCC {
        group: 0,
        channel,
        note,
        cc,
        value,
        registered: true,
    })
}

/// Build a `TransportInfo` from a CLAP transport event/struct.
///
/// Same flag-driven decoding is needed in two places - the
/// `CLAP_EVENT_TRANSPORT` arm of `convert_input_events` (which sees a
/// `clap_event_transport` arriving as an input event mid-block) and
/// the per-process `clap_process::transport` field. Hosts deliver
/// transport state through whichever channel they prefer; the bit
/// layout is identical, so the decode is too.
// CLAP transport positions arrive as `i64` fixed-point counts that
// must be divided into `f64` seconds/beats; the `i64 as f64`
// narrowing is bounded in practice by song-length (well below 2^52).
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn build_transport_info(t: &clap_event_transport, sample_rate: f64) -> TransportInfo {
    let flags = t.flags;
    let beats_timeline = flags & CLAP_TRANSPORT_HAS_BEATS_TIMELINE != 0;
    let has_time_sig = flags & CLAP_TRANSPORT_HAS_TIME_SIGNATURE != 0;
    let position_seconds = if flags & CLAP_TRANSPORT_HAS_SECONDS_TIMELINE != 0 {
        t.song_pos_seconds as f64 / CLAP_SECTIME_FACTOR as f64
    } else {
        0.0
    };
    TransportInfo {
        playing: flags & CLAP_TRANSPORT_IS_PLAYING != 0,
        recording: flags & CLAP_TRANSPORT_IS_RECORDING != 0,
        tempo: if flags & CLAP_TRANSPORT_HAS_TEMPO != 0 {
            t.tempo
        } else {
            120.0
        },
        // CLAP delivers `tsig_num` / `tsig_denom` as `i16`; the
        // narrowing is bounded by the MIDI domain (≤ 255).
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        time_sig_num: if has_time_sig { t.tsig_num as u8 } else { 4 },
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        time_sig_den: if has_time_sig { t.tsig_denom as u8 } else { 4 },
        // CLAP transport carries no sample position; derive it from the
        // seconds timeline (rounded to the nearest sample), the same way
        // the host computes it. 0 when no seconds timeline is provided.
        position_samples: (position_seconds * sample_rate).round() as i64,
        position_seconds,
        position_beats: if beats_timeline {
            t.song_pos_beats as f64 / CLAP_BEATTIME_FACTOR as f64
        } else {
            0.0
        },
        bar_start_beats: if beats_timeline {
            t.bar_start as f64 / CLAP_BEATTIME_FACTOR as f64
        } else {
            0.0
        },
        loop_active: flags & CLAP_TRANSPORT_IS_LOOP_ACTIVE != 0,
        loop_start_beats: if beats_timeline {
            t.loop_start_beats as f64 / CLAP_BEATTIME_FACTOR as f64
        } else {
            0.0
        },
        loop_end_beats: if beats_timeline {
            t.loop_end_beats as f64 / CLAP_BEATTIME_FACTOR as f64
        } else {
            0.0
        },
    }
}

/// MIDI port an inbound CLAP event arrived on, read from the event's
/// `port_index`. Non-MIDI events (params, transport) report `0`. The
/// wildcard-capable events (notes, note expressions) resolve a `-1`
/// port in their own fan-out paths ([`push_note_offs`],
/// [`push_note_expressions`]) - the collapse to `0` here only feeds
/// concrete-address routing, and the raw MIDI / `SysEx` events carry
/// an unsigned `port_index` with no wildcard to lose.
unsafe fn clap_input_port(header: *const clap_event_header, type_: u16) -> u8 {
    unsafe {
        let idx: i32 = match type_ {
            CLAP_EVENT_NOTE_ON | CLAP_EVENT_NOTE_OFF | CLAP_EVENT_NOTE_CHOKE => {
                i32::from((*header.cast::<clap_event_note>()).port_index)
            }
            CLAP_EVENT_NOTE_EXPRESSION => {
                i32::from((*header.cast::<clap_event_note_expression>()).port_index)
            }
            CLAP_EVENT_MIDI => i32::from((*header.cast::<clap_event_midi>()).port_index),
            CLAP_EVENT_MIDI2 => i32::from((*header.cast::<clap_event_midi2>()).port_index),
            CLAP_EVENT_MIDI_SYSEX => {
                i32::from((*header.cast::<clap_event_midi_sysex>()).port_index)
            }
            _ => 0,
        };
        u8::try_from(idx).unwrap_or(0)
    }
}

/// Deliver a `NOTE_OFF` / `NOTE_CHOKE` as concrete `NoteOff`s. A
/// concrete address is one event per matched port (clearing its
/// sounding bit); a wildcard axis (`-1`, spec-legal from
/// `note_id`-addressing hosts) expands to the sounding notes it
/// matches - dropping it would leave those voices ringing forever.
/// The port is an axis too: `port_index == -1` matches every input
/// port, so the expansion walks each port's sounding set instead of
/// collapsing onto the routed port. Out-of-domain junk on the channel
/// or key axis drops the event, matching the concrete path's
/// hostile-host guard.
fn push_note_offs<P: PluginExport>(
    data: &mut ClapPluginData<P>,
    note_event: &clap_event_note,
    sample_offset: u32,
    port: u8,
    velocity: u8,
) {
    let channel = NoteAxis::parse(note_event.channel, 16);
    let key = NoteAxis::parse(note_event.key, 128);
    if channel == NoteAxis::Invalid || key == NoteAxis::Invalid {
        return;
    }
    let (first_port, last_port) = if note_event.port_index == -1 {
        (0, data.info.midi_input_ports.max(1) - 1)
    } else {
        (port, port)
    };
    // Destructure so the tracker and the event list borrow disjointly.
    let ClapPluginData {
        sounding_notes,
        event_list,
        ..
    } = data;
    for p in first_port..=last_port {
        if let (NoteAxis::One(channel), NoteAxis::One(note)) = (channel, key) {
            sounding_notes.clear(p, channel, note);
            event_list.push(Event::on_port(
                sample_offset,
                p,
                EventBody::NoteOff {
                    group: 0,
                    channel,
                    note,
                    velocity,
                },
            ));
        } else {
            sounding_notes.drain_matching(p, channel.filter(), key.filter(), |channel, note| {
                event_list.push(Event::on_port(
                    sample_offset,
                    p,
                    EventBody::NoteOff {
                        group: 0,
                        channel,
                        note,
                        velocity,
                    },
                ));
            });
        }
    }
}

/// Deliver a note expression to the voices it addresses. A concrete
/// address is one event on the routed port; a wildcard axis (`-1`,
/// spec-legal from `note_id`-addressing hosts) fans out to the
/// sounding notes it matches, and the port is an axis too - dropping
/// a wildcard would silence expression for exactly the hosts that
/// rely on it. Out-of-domain junk on the channel or key axis drops
/// the event, matching [`push_note_offs`].
fn push_note_expressions<P: PluginExport>(
    data: &mut ClapPluginData<P>,
    ne: &clap_event_note_expression,
    sample_offset: u32,
    port: u8,
) {
    let channel = NoteAxis::parse(ne.channel, 16);
    let key = NoteAxis::parse(ne.key, 128);
    if channel == NoteAxis::Invalid || key == NoteAxis::Invalid {
        return;
    }
    let (first_port, last_port) = if ne.port_index == -1 {
        (0, data.info.midi_input_ports.max(1) - 1)
    } else {
        (port, port)
    };
    let midi2 = data.info.midi_input_dialect == MidiDialect::Midi2;
    // Destructure so the tracker and the event list borrow disjointly.
    let ClapPluginData {
        sounding_notes,
        event_list,
        ..
    } = data;
    let mut push = |p: u8, channel: u8, note: u8| {
        let Some(decoded) = note_expression_body(ne, channel, note) else {
            return;
        };
        let body = if midi2 {
            Some(decoded)
        } else {
            downconvert_to_midi1(&decoded)
        };
        if let Some(body) = body {
            event_list.push(Event::on_port(sample_offset, p, body));
        }
    };
    for p in first_port..=last_port {
        if let (NoteAxis::One(channel), NoteAxis::One(note)) = (channel, key) {
            push(p, channel, note);
        } else {
            sounding_notes.for_each_matching(p, channel.filter(), key.filter(), |channel, note| {
                push(p, channel, note);
            });
        }
    }
}

/// `sort` controls whether the resulting `event_list` gets a stable
/// sort by sample offset. `process` needs sorted events (the plugin
/// iterates them in time order); `params_flush` discards the events
/// after extracting param/GUI updates and doesn't care about order, so
/// it passes `false` to skip the sort.
#[allow(clippy::too_many_lines)]
unsafe fn convert_input_events<P: PluginExport>(
    data: &mut ClapPluginData<P>,
    in_events: *const clap_input_events,
    sort: bool,
    state_loaded: bool,
) {
    unsafe {
        data.event_list.clear();

        if in_events.is_null() {
            return;
        }

        let Some(size_fn) = (*in_events).size else {
            return;
        };
        let Some(get_fn) = (*in_events).get else {
            return;
        };

        let count = size_fn(in_events);

        for i in 0..count {
            let header = get_fn(in_events, i);
            if header.is_null() {
                continue;
            }

            if (*header).space_id != CLAP_CORE_EVENT_SPACE_ID {
                continue;
            }

            let sample_offset = (*header).time;
            // Stamp each event with the MIDI port it arrived on; an
            // event on a port the plugin doesn't expose routes to 0.
            // Non-MIDI events report 0. Single-port plugins always
            // get 0.
            let port = route_midi_port(
                clap_input_port(header, (*header).type_),
                data.info.midi_input_ports,
            );

            match (*header).type_ {
                CLAP_EVENT_NOTE_ON => {
                    let note_event = &*header.cast::<clap_event_note>();
                    // The spec requires concrete addresses on NOTE_ON;
                    // a wildcard port drops like the wildcard channel /
                    // key axes (`clap_note_address`) instead of
                    // masquerading as port 0.
                    if note_event.port_index < 0 {
                        continue;
                    }
                    let Some((channel, note)) = clap_note_address(note_event) else {
                        continue;
                    };
                    data.sounding_notes.set(port, channel, note);
                    // CLAP's f64 velocity is a normalized [0, 1]; truce
                    // exposes it as a wire-native 7-bit value to match
                    // every other format. Plugins that want CLAP's full
                    // float precision can handle `NoteOn2` from
                    // `CLAP_EVENT_MIDI2` (when the host emits that path).
                    data.event_list.push(Event::on_port(
                        sample_offset,
                        port,
                        EventBody::NoteOn {
                            group: 0,
                            channel,
                            note,
                            velocity: denorm_7bit(f32::from_f64(note_event.velocity)),
                        },
                    ));
                }
                CLAP_EVENT_NOTE_OFF => {
                    let note_event = &*header.cast::<clap_event_note>();
                    let velocity = denorm_7bit(f32::from_f64(note_event.velocity));
                    push_note_offs(data, note_event, sample_offset, port, velocity);
                }
                CLAP_EVENT_NOTE_CHOKE => {
                    // A choke is an immediate voice cut (drum choke
                    // groups, edit re-triggers). `EventBody` has no
                    // choke variant, so deliver a `NoteOff`: a release
                    // tail beats a voice hanging forever.
                    let note_event = &*header.cast::<clap_event_note>();
                    push_note_offs(data, note_event, sample_offset, port, 0);
                }
                CLAP_EVENT_PARAM_VALUE => {
                    // When a state load was applied at the head of
                    // this block, param-change events queued by the
                    // host predate that intent (typical case: a clip-
                    // edge re-trigger sends preset-B automation in
                    // the same block as the preset-A state recall).
                    // Drop them so the just-restored preset isn't
                    // partly overwritten by stale automation.
                    if state_loaded {
                        continue;
                    }
                    let param_event = &*header.cast::<clap_event_param_value>();
                    // `set_plain` is deferred to the per-sub-block
                    // `apply_pending_events` pass in
                    // `chunked_process::process_chunked` - that way
                    // the smoother sees `set_target` at the event's
                    // sample, not at the head of the audio block.
                    data.event_list.push(Event::on_port(
                        sample_offset,
                        port,
                        EventBody::ParamChange {
                            id: param_event.param_id,
                            value: param_event.value,
                        },
                    ));
                }
                CLAP_EVENT_PARAM_MOD => {
                    // Same rationale as PARAM_VALUE above: drop
                    // pre-state-load mod packets.
                    if state_loaded {
                        continue;
                    }
                    let mod_event = &*header.cast::<clap_event_param_value>();
                    data.event_list.push(Event::on_port(
                        sample_offset,
                        port,
                        EventBody::ParamMod {
                            id: mod_event.param_id,
                            note_id: mod_event.note_id,
                            value: mod_event.value,
                        },
                    ));
                }
                CLAP_EVENT_TRANSPORT => {
                    let transport = &*header.cast::<clap_event_transport>();
                    data.event_list.push(Event::new(
                        sample_offset,
                        EventBody::Transport(build_transport_info(transport, data.sample_rate)),
                    ));
                }
                CLAP_EVENT_MIDI => {
                    // CLAP carries MIDI 1.0 channel-voice messages as
                    // 3-byte packets. Demux back into the typed
                    // `EventBody` variants the plugin sees on every
                    // other format. Without this, hosts that route
                    // raw MIDI (`CLAP_NOTE_DIALECT_MIDI` ports)
                    // silently drop CC / PitchBend / Aftertouch /
                    // ChannelPressure / ProgramChange at the wrapper.
                    let midi = &*header.cast::<clap_event_midi>();
                    if let Some(body) =
                        decode_short_message(midi.data[0], midi.data[1], midi.data[2])
                    {
                        data.event_list
                            .push(Event::on_port(sample_offset, port, body));
                    }
                }
                CLAP_EVENT_MIDI_SYSEX => {
                    // CLAP delivers `SysEx` payloads as a pointer +
                    // length owned by the host for the duration of
                    // this `process()` call. Copy into our pool
                    // immediately - the bytes can't be assumed valid
                    // after we return. `push_sysex` is fail-closed
                    // when the pool is exhausted; we drop the
                    // message and keep going (a corrupt-by-split
                    // alternative is never the right answer for
                    // `SysEx`).
                    let sysex = &*header.cast::<clap_event_midi_sysex>();
                    let bytes = if sysex.buffer.is_null() || sysex.size == 0 {
                        &[][..]
                    } else {
                        std::slice::from_raw_parts(sysex.buffer, sysex.size as usize)
                    };
                    let _ = data.event_list.push_sysex(sample_offset, bytes);
                }
                // Decode the UMP packet. A `Midi2`-dialect plugin gets
                // the native 2.0 `EventBody` variants (group nibble
                // included); a plugin that didn't opt into MIDI 2.0 gets
                // the 1.0 down-conversion instead of a dropped event -
                // hosts don't always honor the advertised dialect, so a
                // 2.0 packet can still arrive at a 1.0 plugin.
                CLAP_EVENT_MIDI2 => {
                    let midi2 = &*header.cast::<clap_event_midi2>();
                    if let Some(decoded) = decode_ump_channel_voice_2(midi2.data) {
                        let body = if data.info.midi_input_dialect == MidiDialect::Midi2 {
                            Some(decoded)
                        } else {
                            downconvert_to_midi1(&decoded)
                        };
                        if let Some(body) = body {
                            data.event_list
                                .push(Event::on_port(sample_offset, port, body));
                        }
                    }
                }
                CLAP_EVENT_NOTE_EXPRESSION => {
                    // Per-note expression (hosts send these for MPE-style
                    // input). Decode to the 2.0 per-note event - fanned
                    // out across wildcard axes - with the down-converted
                    // channel form for plugins that didn't opt into 2.0.
                    let ne = &*header.cast::<clap_event_note_expression>();
                    push_note_expressions(data, ne, sample_offset, port);
                }
                _ => {
                    // Unsupported event type (system real-time, utility)
                    // - skip silently.
                }
            }
        }

        if sort {
            data.event_list.ensure_sorted_by_offset();
        }
    }
}

// ---------------------------------------------------------------------------
// Flush GUI-initiated param changes to CLAP output events
// ---------------------------------------------------------------------------

unsafe fn flush_gui_changes<P: PluginExport>(
    data: &mut ClapPluginData<P>,
    out_events: *const clap_output_events,
) {
    unsafe {
        if out_events.is_null() {
            return;
        }
        let Some(try_push) = (*out_events).try_push else {
            return;
        };

        while let Some(change) = data.gui_changes.pop() {
            match change {
                GuiParamChange::GestureBegin(id) => {
                    let event = clap_event_param_gesture {
                        header: clap_event_header {
                            size: size_of_u32::<clap_event_param_gesture>(),
                            time: 0,
                            space_id: CLAP_CORE_EVENT_SPACE_ID,
                            type_: CLAP_EVENT_PARAM_GESTURE_BEGIN,
                            flags: CLAP_EVENT_IS_LIVE,
                        },
                        param_id: id,
                    };
                    try_push(out_events, &raw const event.header);
                }
                GuiParamChange::Value(id, plain) => {
                    let event = clap_event_param_value {
                        header: clap_event_header {
                            size: size_of_u32::<clap_event_param_value>(),
                            time: 0,
                            space_id: CLAP_CORE_EVENT_SPACE_ID,
                            type_: CLAP_EVENT_PARAM_VALUE,
                            flags: CLAP_EVENT_IS_LIVE,
                        },
                        param_id: id,
                        cookie: ptr::null_mut(),
                        note_id: -1,
                        port_index: -1,
                        channel: -1,
                        key: -1,
                        value: plain,
                    };
                    try_push(out_events, &raw const event.header);
                }
                GuiParamChange::GestureEnd(id) => {
                    let event = clap_event_param_gesture {
                        header: clap_event_header {
                            size: size_of_u32::<clap_event_param_gesture>(),
                            time: 0,
                            space_id: CLAP_CORE_EVENT_SPACE_ID,
                            type_: CLAP_EVENT_PARAM_GESTURE_END,
                            flags: CLAP_EVENT_IS_LIVE,
                        },
                        param_id: id,
                    };
                    try_push(out_events, &raw const event.header);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Process callback
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
unsafe extern "C" fn clap_plugin_process<P: PluginExport>(
    plugin: *const clap_plugin,
    process: *const clap_process,
) -> i32 {
    run_audio_block_with::<P, i32>("CLAP", CLAP_PROCESS_ERROR, || unsafe {
        if process.is_null() {
            return CLAP_PROCESS_ERROR;
        }

        let proc = &*process;
        let data = data_from_plugin::<P>(plugin);
        let num_frames = proc.frames_count as usize;

        if num_frames == 0 {
            return CLAP_PROCESS_CONTINUE;
        }

        // Take ownership of the plugin for the whole block: an
        // uncontended `Acquire`, never a wait, since the host contract
        // keeps `process` from overlapping a lifecycle callback and host
        // saves read the snapshot instead of the plugin. Enter through a
        // local Arc clone so the guard doesn't pin a borrow of `data`
        // (later per-block work needs `&mut data`).
        let plugin_arc = Arc::clone(&data.plugin);
        let mut instance = enter_plugin(&plugin_arc);

        // Apply any state-load that the host or editor handed us
        // since the last block. Runs before per-block work so the
        // plugin sees consistent params for the entire block. The
        // single-slot queue means a rapid double-recall lands the
        // newest blob and the older one is dropped - preferred to
        // the audio thread chasing stale state across blocks.
        let state_loaded = data.pending_state.pop().is_some_and(|state| {
            state::apply_state(&mut *instance, &state);
            true
        });

        // Paranoid allocation check (the `rt-paranoid` feature): guard the
        // wrapper's per-block glue - event conversion, transport, process,
        // output encode, snapshot publish - as well as the plugin. Placed
        // after the state-load apply above, since `load_state` legitimately
        // allocates. No-op and zero-sized when the feature is off.
        let _rt = RtSection::enter();

        // Convert CLAP input events to our EventList - sort by
        // sample offset so the plugin sees them in time order.
        // `state_loaded` causes ParamValue/ParamMod events to be
        // dropped because they predate the state-load intent.
        convert_input_events::<P>(data, proc.in_events, true, state_loaded);

        // Build transport info from the CLAP transport event (or default).
        let transport = if proc.transport.is_null() {
            TransportInfo::default()
        } else {
            build_transport_info(&*proc.transport, data.sample_rate)
        };

        // Build AudioBuffer from CLAP audio buffers.
        //
        // Three soundness considerations matching the format-wrapper
        // pattern in `RawBufferScratch::build`:
        //
        // 1. **No per-block heap allocation.** We reuse `data.input_slices`
        //    and `data.output_slices` (cleared each call) so the audio
        //    thread doesn't `Vec::new()` per process.
        // 2. **Channel indexing preserved.** A null channel pointer
        //    becomes an empty slice at the same flat-channel index
        //    rather than being dropped; densifying the channel list
        //    would silently re-map indices when only some channels
        //    were null.
        // 3. **No auto input→output copy.** Plugins that want
        //    pass-through must do `output.copy_from_slice(input)`
        //    themselves; auto-copying clobbers the previous-block tail
        //    that delay/reverb feedback paths read back from the output.
        debug_assert!(
            num_frames <= data.max_block_size,
            "host violated CLAP contract: process() got {num_frames} frames \
             but activate() declared max {}",
            data.max_block_size
        );

        // Build per-channel slices preserving channel index across
        // every bus. A null bus (neither data pointer set) emits
        // empty slices for each of its declared channels rather than
        // being skipped - skipping would shift downstream buses'
        // channel indices and silently re-route audio onto the wrong
        // bus for multi-bus plugins.
        //
        // The host picks the wire precision per port: `data64` is
        // only set on ports that advertised
        // `CLAP_AUDIO_PORT_SUPPORTS_64BITS` (f64 plugins). When the
        // wire matches `P::Sample`, slices point straight at host
        // memory (zero-copy); otherwise the channel is converted
        // through the matching `input_widen` / `output_narrow` slot
        // and copied back after `process()` returns.
        data.input_slices.clear();
        // Reset each inner scratch buffer's length to 0 (preserves
        // its heap allocation), don't `.clear()` the outer
        // `Vec<Vec<_>>` - that would drop every inner Vec and force
        // the per-channel push below to re-allocate every block,
        // defeating the activate-time pre-grow.
        for buf in &mut data.input_widen {
            buf.clear();
        }
        // The outer Vec is pre-sized in `clap_plugin_activate`; the
        // while-loops below only run as a fallback if the pre-grow
        // didn't cover the bus layout the host actually picked.
        // `Vec::new()` keeps the fallback allocation-free; the inner
        // scratch only allocates if that channel actually converts.
        let mut flat_in_idx = 0usize;
        for bus_idx in 0..proc.audio_inputs_count {
            let buf = &*proc.audio_inputs.add(bus_idx as usize);
            let bus_is_f64 = !buf.data64.is_null();
            for ch in 0..buf.channel_count {
                while data.input_widen.len() <= flat_in_idx {
                    data.input_widen.push(Vec::new());
                }
                let scratch = &mut data.input_widen[flat_in_idx];
                let slice: &'static [P::Sample] = if bus_is_f64 {
                    let host_ptr: *const f64 = *buf.data64.add(ch as usize);
                    input_channel_slice::<P::Sample, f64>(host_ptr, num_frames, scratch)
                } else if buf.data32.is_null() {
                    &[]
                } else {
                    let host_ptr: *const f32 = *buf.data32.add(ch as usize);
                    input_channel_slice::<P::Sample, f32>(host_ptr, num_frames, scratch)
                };
                data.input_slices.push(slice);
                flat_in_idx += 1;
            }
        }

        data.output_slices.clear();
        // Same reasoning as `input_widen` above: clear each inner
        // scratch buffer in place so its heap allocation survives.
        for buf in &mut data.output_narrow {
            buf.clear();
        }
        data.host_out_ptrs.clear();
        let mut flat_out_idx = 0usize;
        for bus_idx in 0..proc.audio_outputs_count {
            let buf = &mut *proc.audio_outputs.add(bus_idx as usize);
            let bus_is_f64 = !buf.data64.is_null();
            for ch in 0..buf.channel_count {
                while data.output_narrow.len() <= flat_out_idx {
                    data.output_narrow.push(Vec::new());
                }
                let scratch = &mut data.output_narrow[flat_out_idx];
                let slice: &'static mut [P::Sample] = if bus_is_f64 {
                    let host_ptr: *mut f64 = *buf.data64.add(ch as usize);
                    data.host_out_ptrs.push(if host_ptr.is_null() {
                        HostOutPtr::Null
                    } else {
                        HostOutPtr::F64(host_ptr)
                    });
                    output_channel_slice::<P::Sample, f64>(host_ptr, num_frames, scratch)
                } else {
                    let host_ptr: *mut f32 = if buf.data32.is_null() {
                        std::ptr::null_mut()
                    } else {
                        *buf.data32.add(ch as usize)
                    };
                    data.host_out_ptrs.push(if host_ptr.is_null() {
                        HostOutPtr::Null
                    } else {
                        HostOutPtr::F32(host_ptr)
                    });
                    output_channel_slice::<P::Sample, f32>(host_ptr, num_frames, scratch)
                };
                data.output_slices.push(slice);
                flat_out_idx += 1;
            }
        }

        // Construct the AudioBuffer with a borrow scope tied to this
        // call only. Without the transmute, the borrow checker
        // propagates the `'static` lifetimes inside `input_slices`
        // out to the AudioBuffer's lifetime parameter - which would
        // pin `data` mutably for the rest of the function. Same
        // pattern as `RawBufferScratch::build`.
        let data_ptr: *mut ClapPluginData<P> = data;
        let s = &mut *data_ptr;
        let mut audio_buffer =
            transmute::<AudioBuffer<'static, P::Sample>, AudioBuffer<'_, P::Sample>>(
                AudioBuffer::from_slices(&s.input_slices, &mut s.output_slices, num_frames),
            );

        data.output_events.clear();

        // Publish transport to the editor slot before the plugin runs.
        data.transport_slot.write(&transport);

        // Sample-accurate parameter chunking: split the audio block
        // at every chunkable param-change / transport event, deferring
        // the `set_plain` apply to the sub-block boundary the event
        // sits on. On blocks with no chunkable events this runs
        // `plugin.process` exactly once - the splitting machinery is
        // inert. See `chunked_process` for the per-sub-block contract.
        let mut transport_snap = transport;
        let chunk_args = ChunkedProcess {
            events: &data.event_list,
            sub_event_scratch: &mut data.sub_event_scratch,
            transport: &mut transport_snap,
            sample_rate: data.sample_rate,
            process_mode: ProcessMode::from_u8(data.render_mode.load(Ordering::Relaxed)),
            output_events: &mut data.output_events,
            params_fn: None,
            meters_fn: None,
            param_infos: &data.param_infos,
            min_subblock_samples: data.info.automation.min_subblock_samples,
        };
        let status = process_chunked(
            &mut *instance,
            data.params_arc.as_ref() as &dyn Params,
            &mut audio_buffer,
            chunk_args,
        );

        // Convert + copy back to host outputs for every channel that
        // rendered into scratch (wire precision differed from
        // `P::Sample`). Zero-copy channels leave their scratch slot
        // empty and are skipped - the plugin already wrote host
        // memory. `zip` over the two vectors instead of indexing -
        // if either is shorter (it shouldn't be, but a future drift
        // would hit this), iteration stops at the min cleanly rather
        // than panicking on an out-of-bounds index.
        for (host_ptr, plugin) in data.host_out_ptrs.iter().zip(data.output_narrow.iter()) {
            if plugin.is_empty() {
                continue;
            }
            match *host_ptr {
                HostOutPtr::Null => {}
                HostOutPtr::F32(p) => {
                    let host = std::slice::from_raw_parts_mut(p, num_frames);
                    for (h, &s) in host.iter_mut().zip(plugin.iter()) {
                        *h = s.to_f32();
                    }
                }
                HostOutPtr::F64(p) => {
                    let host = std::slice::from_raw_parts_mut(p, num_frames);
                    for (h, &s) in host.iter_mut().zip(plugin.iter()) {
                        *h = s.to_f64();
                    }
                }
            }
        }

        // Refresh latency / tail caches so the host's main-thread
        // queries don't have to touch the plugin. On an actual
        // latency change, flag it and wake the main thread, which
        // notifies the host (`clap.latency` requires the call off the
        // audio thread).
        let new_latency = instance.latency();
        if data.latency_cache.swap(new_latency, Ordering::Relaxed) != new_latency
            && !data.latency_dirty.swap(true, Ordering::Relaxed)
            && !data.host.is_null()
            && let Some(req_cb) = (*data.host).request_callback
        {
            req_cb(data.host);
        }
        data.tail_cache.store(instance.tail(), Ordering::Relaxed);

        // Flush GUI-initiated param changes to host output events
        flush_gui_changes::<P>(data, proc.out_events);

        // Forward plugin output events (MIDI output from instruments/effects)
        if !proc.out_events.is_null() && !data.output_events.is_empty() {
            let Some(try_push) = (*proc.out_events).try_push else {
                return CLAP_PROCESS_CONTINUE;
            };
            // CLAP requires the output queue sorted by time; a plugin
            // that pushes block-level events (an LFO, a mode-switch
            // sweep) after per-event ones would otherwise hand the
            // host an unsorted queue.
            data.output_events.ensure_sorted_by_offset();
            // Route each output event to the note port the plugin
            // stamped it with; an out-of-range port routes to 0.
            for event in data.output_events.iter() {
                let out_port = route_midi_port(event.port, data.info.midi_output_ports);
                match &event.body {
                    EventBody::NoteOn {
                        channel,
                        note,
                        velocity,
                        ..
                    } => {
                        let ev = clap_event_note {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_note>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_NOTE_ON,
                                flags: 0,
                            },
                            note_id: -1,
                            port_index: i16::from(out_port),
                            channel: i16::from(*channel),
                            key: i16::from(*note),
                            velocity: f64::from(*velocity) / 127.0,
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    EventBody::NoteOff {
                        channel,
                        note,
                        velocity,
                        ..
                    } => {
                        let ev = clap_event_note {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_note>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_NOTE_OFF,
                                flags: 0,
                            },
                            note_id: -1,
                            port_index: i16::from(out_port),
                            channel: i16::from(*channel),
                            key: i16::from(*note),
                            velocity: f64::from(*velocity) / 127.0,
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    // CLAP carries MIDI 1.0 control / channel events as
                    // `CLAP_EVENT_MIDI` 3-byte packets. The host
                    // demuxes them on the receiving side; we just
                    // build the standard MIDI status byte and pass
                    // the data bytes through.
                    EventBody::ControlChange {
                        channel, cc, value, ..
                    } => {
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_midi>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI,
                                flags: 0,
                            },
                            port_index: u16::from(out_port),
                            data: [0xB0 | (channel & 0x0F), *cc, *value],
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    EventBody::Aftertouch {
                        channel,
                        note,
                        pressure,
                        ..
                    } => {
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_midi>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI,
                                flags: 0,
                            },
                            port_index: u16::from(out_port),
                            data: [0xA0 | (channel & 0x0F), *note, *pressure],
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    EventBody::ChannelPressure {
                        channel, pressure, ..
                    } => {
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_midi>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI,
                                flags: 0,
                            },
                            port_index: u16::from(out_port),
                            data: [0xD0 | (channel & 0x0F), *pressure, 0],
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    EventBody::PitchBend { channel, value, .. } => {
                        let (lsb, msb) = pitch_bend_to_bytes(*value);
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_midi>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI,
                                flags: 0,
                            },
                            port_index: u16::from(out_port),
                            data: [0xE0 | (channel & 0x0F), lsb, msb],
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    EventBody::ProgramChange {
                        channel, program, ..
                    } => {
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_midi>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI,
                                flags: 0,
                            },
                            port_index: u16::from(out_port),
                            data: [0xC0 | (channel & 0x0F), *program, 0],
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    EventBody::ParamChange { id, value } => {
                        // CLAP params are global, not tied to a specific
                        // note/audio port - every key uses the `-1`
                        // wildcard so hosts that route automation by
                        // port_index match GUI-driven and process-driven
                        // param events on the same key. The
                        // `flush_gui_changes` path uses the same shape.
                        let ev = clap_event_param_value {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_param_value>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_PARAM_VALUE,
                                flags: 0,
                            },
                            param_id: *id,
                            cookie: ptr::null_mut(),
                            note_id: -1,
                            port_index: -1,
                            channel: -1,
                            key: -1,
                            value: *value,
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    EventBody::SysEx { .. } => {
                        // CLAP's contract for the output direction
                        // is narrower than the input's: the
                        // `buffer` pointer needs to stay valid for
                        // the duration of the `try_push` call only
                        // (the host is expected to copy before
                        // returning). Our pool is cleared at the
                        // *start* of the next block, so pointing
                        // the host at `pool_offset` satisfies that
                        // strictly - and any future host that
                        // defers the copy until later in
                        // `process()` is still fine because the
                        // pool stays valid through the whole block.
                        let bytes = data.output_events.sysex_bytes(&event.body);
                        let ev = clap_event_midi_sysex {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_midi_sysex>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI_SYSEX,
                                flags: 0,
                            },
                            port_index: u16::from(out_port),
                            buffer: bytes.as_ptr(),
                            size: len_u32(bytes.len()),
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    // MIDI 2.0 channel-voice + per-note events. Emitted
                    // CLAP-native so every host reads them: notes as
                    // `clap_event_note` (full 16-bit velocity via the f64
                    // field), per-note control as note expressions, and
                    // channel-level control down-converted to raw MIDI.
                    // Native `CLAP_EVENT_MIDI2` output is only consumed by
                    // UMP-aware hosts; note graphs (Bitwig, ...) read notes
                    // + expressions. ParamMod / Transport have no wire form
                    // and fall through the encoder as `None`.
                    EventBody::NoteOn2 {
                        channel,
                        note,
                        velocity,
                        ..
                    } => {
                        let ev = clap_event_note {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_note>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_NOTE_ON,
                                flags: 0,
                            },
                            note_id: -1,
                            port_index: i16::from(out_port),
                            channel: i16::from(*channel),
                            key: i16::from(*note),
                            velocity: f64::from(*velocity) / f64::from(u16::MAX),
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    EventBody::NoteOff2 {
                        channel,
                        note,
                        velocity,
                        ..
                    } => {
                        let ev = clap_event_note {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_note>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_NOTE_OFF,
                                flags: 0,
                            },
                            note_id: -1,
                            port_index: i16::from(out_port),
                            channel: i16::from(*channel),
                            key: i16::from(*note),
                            velocity: f64::from(*velocity) / f64::from(u16::MAX),
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    body if clap_note_expression_of(body).is_some() => {
                        let (expression_id, channel, note, value) =
                            clap_note_expression_of(body).unwrap();
                        let ev = clap_event_note_expression {
                            header: clap_event_header {
                                size: size_of_u32::<clap_event_note_expression>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_NOTE_EXPRESSION,
                                flags: 0,
                            },
                            expression_id,
                            note_id: -1,
                            port_index: i16::from(out_port),
                            channel: i16::from(channel),
                            key: i16::from(note),
                            value,
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    body => {
                        // Channel-level 2.0 (ControlChange2 / PitchBend2 /
                        // ChannelPressure2 / ProgramChange2): down-convert
                        // to a 1.0 short message and emit as raw MIDI.
                        if let Some(m1) = downconvert_to_midi1(body)
                            && let Some((_, data)) = event_to_midi1(&m1)
                        {
                            let ev = clap_event_midi {
                                header: clap_event_header {
                                    size: size_of_u32::<clap_event_midi>(),
                                    time: event.sample_offset,
                                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                                    type_: CLAP_EVENT_MIDI,
                                    flags: 0,
                                },
                                port_index: u16::from(out_port),
                                data,
                            };
                            try_push(proc.out_events, &raw const ev.header);
                        }
                    }
                }
            }
        }

        // Drop the channel-slice borrows we transmuted to `'static`
        // for the duration of this call. The host's `audio_inputs` /
        // `audio_outputs` buffers may be reused or freed once
        // `clap_plugin_process` returns; leaving the slice scratch
        // populated would pin dangling pointers across blocks. Clear
        // does NOT shrink capacity, so the next block reuses the
        // same allocation without touching the global allocator.
        data.input_slices.clear();
        data.output_slices.clear();

        match status {
            ProcessStatus::Normal => CLAP_PROCESS_CONTINUE,
            ProcessStatus::Tail(0) => CLAP_PROCESS_SLEEP,
            ProcessStatus::Tail(_) => CLAP_PROCESS_TAIL,
            ProcessStatus::KeepAlive => CLAP_PROCESS_CONTINUE_IF_NOT_QUIET,
        }
    })
}

/// Test-only smoke helper for the `rt-paranoid` CI gate: drives a few
/// real CLAP `process` callbacks through this wrapper's per-block glue
/// (event conversion, transport, `process_chunked`, output narrow,
/// snapshot publish) via the full plugin vtable, and returns the
/// audio-thread allocation count of a steady-state block (0 = clean).
/// Small stereo buffers, no input events. Vacuously 0 unless the
/// `rt-paranoid` feature installs the checking allocator. Not public API.
#[doc(hidden)]
#[must_use]
pub fn rt_paranoid_smoke<P: PluginExport>() -> u32 {
    const FRAMES: u32 = 512;
    const CHANNELS: u32 = 2;

    // `clap_plugin_init` queries the params extension through the host's
    // `get_extension`, so that pointer must be a live stub; the rest of
    // the host callbacks go unused on this path.
    unsafe extern "C" fn no_extension(
        _host: *const clap_host,
        _id: *const c_char,
    ) -> *const c_void {
        ptr::null()
    }
    // No-op output-event sink. The wrapper null-checks `out_events` and
    // only calls `try_push` when the plugin emits events (this passthrough
    // does not), but a live sink keeps the harness honest if that changes.
    unsafe extern "C" fn no_push(
        _list: *const clap_output_events,
        _event: *const clap_event_header,
    ) -> bool {
        true
    }

    let frames = FRAMES as usize;
    // Leaked so their addresses outlive the plugin instance below.
    let descriptor: &'static clap_plugin_descriptor = Box::leak(Box::new(clap_plugin_descriptor {
        clap_version: CLAP_VERSION,
        id: ptr::null(),
        name: ptr::null(),
        vendor: ptr::null(),
        url: ptr::null(),
        manual_url: ptr::null(),
        support_url: ptr::null(),
        version: ptr::null(),
        description: ptr::null(),
        features: ptr::null(),
    }));
    let host: &'static clap_host = Box::leak(Box::new(clap_host {
        clap_version: CLAP_VERSION,
        host_data: ptr::null_mut(),
        name: ptr::null(),
        vendor: ptr::null(),
        url: ptr::null(),
        version: ptr::null(),
        get_extension: Some(no_extension),
        request_restart: None,
        request_process: None,
        request_callback: None,
    }));

    // SAFETY: constructs, drives, and destroys its own instance through
    // the plugin vtable. Every pointer handed to `process` (audio
    // buffers, the channel-pointer arrays, the output-event sink)
    // outlives each call, and the buffers are sized to `FRAMES`.
    unsafe {
        let plugin = create_plugin_instance::<P>(descriptor, host);
        let vtable = &*plugin;
        (vtable.init.unwrap())(plugin);
        (vtable.activate.unwrap())(plugin, 48_000.0, 1, FRAMES);
        (vtable.start_processing.unwrap())(plugin);

        // Non-zero constant input so a working passthrough-times-gain
        // renders non-zero output; the assert below fails loudly if a
        // regressed harness never actually ran `process`. CLAP's
        // `clap_audio_buffer::data32` is `*mut *mut f32`, so the input
        // storage is mutable too even though the wrapper only reads it.
        let mut in_left = vec![0.5f32; frames];
        let mut in_right = vec![0.5f32; frames];
        let mut out_left = vec![0f32; frames];
        let mut out_right = vec![0f32; frames];
        let mut in_ptrs: [*mut f32; 2] = [in_left.as_mut_ptr(), in_right.as_mut_ptr()];
        let mut out_ptrs: [*mut f32; 2] = [out_left.as_mut_ptr(), out_right.as_mut_ptr()];

        let input_bus = clap_audio_buffer {
            data32: in_ptrs.as_mut_ptr(),
            data64: ptr::null_mut(),
            channel_count: CHANNELS,
            latency: 0,
            constant_mask: 0,
        };
        let mut output_bus = clap_audio_buffer {
            data32: out_ptrs.as_mut_ptr(),
            data64: ptr::null_mut(),
            channel_count: CHANNELS,
            latency: 0,
            constant_mask: 0,
        };
        let sink = clap_output_events {
            ctx: ptr::null_mut(),
            try_push: Some(no_push),
        };
        let process = clap_process {
            steady_time: 0,
            frames_count: FRAMES,
            transport: ptr::null(),
            audio_inputs: &raw const input_bus,
            audio_outputs: &raw mut output_bus,
            audio_inputs_count: 1,
            audio_outputs_count: 1,
            in_events: ptr::null(),
            out_events: &raw const sink,
        };

        let mut count = 0;
        // A few blocks so any legitimate first-block warmup is behind us;
        // the last block is the steady-state measurement.
        for _ in 0..3 {
            let ((), n) = audit(|| {
                (vtable.process.unwrap())(plugin, &raw const process);
            });
            count = n;
        }

        (vtable.stop_processing.unwrap())(plugin);
        (vtable.deactivate.unwrap())(plugin);
        (vtable.destroy.unwrap())(plugin);

        // A no-op harness (process never ran) leaves the output silent;
        // a working passthrough-times-gain does not.
        debug_assert!(
            out_left
                .iter()
                .chain(out_right.iter())
                .any(|s| s.abs() > 0.0),
            "CLAP smoke harness produced silent output - process did not run"
        );
        count
    }
}

// ---------------------------------------------------------------------------
// Extension: params
// ---------------------------------------------------------------------------

unsafe extern "C" fn params_count<P: PluginExport>(plugin: *const clap_plugin) -> u32 {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        len_u32(data.param_infos.len())
    }
}

unsafe extern "C" fn params_get_info<P: PluginExport>(
    plugin: *const clap_plugin,
    param_index: u32,
    out: *mut clap_param_info,
) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        let infos = &data.param_infos;

        if param_index as usize >= infos.len() {
            return false;
        }

        let info = &infos[param_index as usize];
        let out = &mut *out;

        out.id = info.id;
        out.cookie = ptr::null_mut();

        // Convert flags
        let mut flags: u32 = 0;
        if info.flags.contains(ParamFlags::AUTOMATABLE) {
            flags |= CLAP_PARAM_IS_AUTOMATABLE;
        }
        if info.flags.contains(ParamFlags::HIDDEN) {
            flags |= CLAP_PARAM_IS_HIDDEN;
        }
        if info.flags.contains(ParamFlags::READONLY) {
            flags |= CLAP_PARAM_IS_READONLY;
        }
        if info.flags.contains(ParamFlags::IS_BYPASS) {
            flags |= CLAP_PARAM_IS_BYPASS;
        }
        match &info.range {
            ParamRange::Enum { .. } => {
                flags |= CLAP_PARAM_IS_STEPPED | CLAP_PARAM_IS_ENUM;
            }
            ParamRange::Discrete { .. } => {
                flags |= CLAP_PARAM_IS_STEPPED;
            }
            _ => {}
        }
        out.flags = flags;

        out.min_value = info.range.min();
        out.max_value = info.range.max();
        out.default_value = info.default_plain;

        // Name
        out.name = [0; CLAP_NAME_SIZE];
        copy_str_to_buf(&mut out.name, info.name);

        // Module path (use group if non-empty)
        out.module = [0; CLAP_PATH_SIZE];
        if !info.group.is_empty() {
            copy_str_to_buf(&mut out.module, info.group);
        }

        true
    }
}

unsafe extern "C" fn params_get_value<P: PluginExport>(
    plugin: *const clap_plugin,
    param_id: clap_id,
    out_value: *mut f64,
) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        match data.params_arc.get_plain(param_id) {
            Some(v) => {
                *out_value = v;
                true
            }
            None => false,
        }
    }
}

unsafe extern "C" fn params_value_to_text<P: PluginExport>(
    plugin: *const clap_plugin,
    param_id: clap_id,
    value: f64,
    out_buffer: *mut c_char,
    out_buffer_capacity: u32,
) -> bool {
    unsafe {
        // Same `out_len == 0` / null-buffer guard the VST3/VST2/AU/AAX
        // wrappers gained in the host-crash-fixes pass: a zero
        // capacity makes `cap - 1` underflow (caught here by
        // `saturating_sub`) and a null `out_buffer` plus non-zero
        // capacity would still write the trailing NUL. Treat both as
        // "host wants nothing" and return.
        if out_buffer_capacity == 0 || out_buffer.is_null() {
            return false;
        }
        let data = data_from_plugin::<P>(plugin);
        match data.params_arc.format_value(param_id, value) {
            Some(text) => {
                let bytes = text.as_bytes();
                let cap = out_buffer_capacity as usize;
                let len = bytes.len().min(cap - 1);
                ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out_buffer, len);
                *out_buffer.add(len) = 0;
                true
            }
            None => false,
        }
    }
}

unsafe extern "C" fn params_text_to_value<P: PluginExport>(
    plugin: *const clap_plugin,
    param_id: clap_id,
    param_value_text: *const c_char,
    out_value: *mut f64,
) -> bool {
    unsafe {
        if param_value_text.is_null() {
            return false;
        }
        let data = data_from_plugin::<P>(plugin);
        let Ok(text) = CStr::from_ptr(param_value_text).to_str() else {
            return false;
        };
        match data.params_arc.parse_value(param_id, text) {
            Some(v) => {
                *out_value = v;
                true
            }
            None => false,
        }
    }
}

unsafe extern "C" fn params_flush<P: PluginExport>(
    plugin: *const clap_plugin,
    in_events: *const clap_input_events,
    out_events: *const clap_output_events,
) {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        // params_flush only forwards param values to the plugin and
        // sweeps GUI-driven changes outward; it doesn't iterate the
        // event list in time order, so skip the sort.
        // params_flush is a non-audio-thread param sweep; no state-load
        // race possible here, so the drain flag stays false.
        convert_input_events::<P>(data, in_events, false, false);
        // `flush` doesn't enter `process()`, so the chunker's deferred
        // `set_plain` apply pass never runs. Apply `ParamChange` events
        // synchronously here so host-thread param sweeps (preset
        // recalls, host automation while transport is stopped, the
        // clap-validator `state-reproducibility-flush` check) take
        // effect. Mirrors what the audio-thread chunker does inside
        // `apply_pending_events`.
        let params = data.params_arc.as_ref() as &dyn Params;
        for ev in data.event_list.iter() {
            if let EventBody::ParamChange { id, value } = ev.body {
                params.set_plain(id, value);
            }
        }
        flush_gui_changes::<P>(data, out_events);
    }
}

fn make_params_extension<P: PluginExport>() -> clap_plugin_params {
    clap_plugin_params {
        count: Some(params_count::<P>),
        get_info: Some(params_get_info::<P>),
        get_value: Some(params_get_value::<P>),
        value_to_text: Some(params_value_to_text::<P>),
        text_to_value: Some(params_text_to_value::<P>),
        flush: Some(params_flush::<P>),
    }
}

// ---------------------------------------------------------------------------
// Extension: state
// ---------------------------------------------------------------------------

unsafe extern "C" fn state_save<P: PluginExport>(
    plugin: *const clap_plugin,
    stream: *const clap_ostream,
) -> bool {
    // Guard the user's `save_state()` against panics so a stray
    // `unwrap` in custom-state code degrades to "save failed" instead
    // of aborting the host across this `extern "C"` boundary.
    run_extern_callback_with::<P, bool>("CLAP", "save_state", false, || unsafe {
        let data = data_from_plugin::<P>(plugin);
        let (ids, values) = data.params_arc.collect_values();
        // Read the custom state from the lock-free snapshot the audio
        // thread publishes each block. Never touches the plugin, so it
        // can't stall a block in flight.
        let extra = save_extra(&data.snapshot);
        let persist = data.params_arc.serialize_persist();
        let blob = state::serialize_state(data.plugin_id_hash, &ids, &values, &extra, &persist);

        // Write to the CLAP output stream
        let Some(write_fn) = (*stream).write else {
            return false;
        };

        let mut offset = 0usize;
        while offset < blob.len() {
            let written = write_fn(
                stream,
                blob[offset..].as_ptr().cast::<c_void>(),
                (blob.len() - offset) as u64,
            );
            if written <= 0 {
                return false;
            }
            // `written > 0` checked above; on 32-bit targets the cast
            // narrows but blob.len() also fits in usize.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let n = written as usize;
            offset += n;
        }

        true
    })
}

unsafe extern "C" fn state_load<P: PluginExport>(
    plugin: *const clap_plugin,
    stream: *const clap_istream,
) -> bool {
    // Guard the user's `load_state()` / custom deserialize against
    // panics so a malformed blob degrades to "load failed" instead of
    // aborting the host across this `extern "C"` boundary.
    run_extern_callback_with::<P, bool>("CLAP", "load_state", false, || unsafe {
        let data = data_from_plugin::<P>(plugin);

        let Some(read_fn) = (*stream).read else {
            return false;
        };

        // Read all data from stream
        let mut blob = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let read = read_fn(stream, buf.as_mut_ptr().cast::<c_void>(), buf.len() as u64);
            if read <= 0 {
                break;
            }
            // `read > 0` checked above; CLAP plugin state blob fits
            // in usize on every supported target.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let n = read as usize;
            blob.extend_from_slice(&buf[..n]);
        }

        if blob.is_empty() {
            return false;
        }

        // Not this plugin's envelope? Offer the bytes to the plugin's
        // `migrate_state` hook (legacy sessions from a pre-truce
        // build); `None` fails the load honestly.
        let Some(deserialized) = state::parse_or_migrate::<P>(
            &blob,
            data.plugin_id_hash,
            state::PluginFormat::Clap,
            None,
        ) else {
            return false;
        };

        // Apply params synchronously on the host thread (atomic-safe)
        // so host queries that read parameter values right after
        // `clap_plugin_state.load` see the restored values without
        // first running a process block - clap-validator reads back
        // immediately after a load round-trip.
        state::apply_params(&*data.params_arc, &deserialized);

        if data.active.load(Ordering::Relaxed) {
            // Active: the audio thread will drain `pending_state` at the
            // top of the next block and apply the custom-state blob
            // under its exclusive `&mut plugin`. `force_push` overwrites
            // any older pending blob - see the `pending_state` field
            // comment for why "newest wins" is right.
            let _ = data.pending_state.force_push(deserialized);
        } else {
            // Inactive: no `process` will run, so the queue would never
            // drain and the plugin's custom state would stay stale until
            // the next activate. Apply the full state (params + extra)
            // synchronously under exclusive plugin ownership - uncontended here
            // since no audio thread is processing.
            let mut instance = enter_plugin(&data.plugin);
            state::apply_state(&mut *instance, &deserialized);
            // No `process` will publish, so refresh the snapshot slot now
            // - a save that follows while still inactive reads live state.
            instance.republish_snapshot();
        }

        if let Some(ref mut editor) = data.editor {
            editor.state_changed();
        }

        true
    })
}

fn make_state_extension<P: PluginExport>() -> clap_plugin_state {
    clap_plugin_state {
        save: Some(state_save::<P>),
        load: Some(state_load::<P>),
    }
}

// ---------------------------------------------------------------------------
// Extension: preset-load
// ---------------------------------------------------------------------------

/// `CLAP_EXT_PRESET_LOAD.from_location`: read a `.trucepreset` file
/// the discovery provider surfaced and apply its embedded state
/// envelope - the same host-thread-apply + audio-thread-handoff path
/// `state_load` takes, so preset recall and session restore are one
/// code path from here down.
unsafe extern "C" fn preset_load_from_location<P: PluginExport>(
    plugin: *const clap_plugin,
    location_kind: clap_preset_discovery_location_kind,
    location: *const c_char,
    load_key: *const c_char,
) -> bool {
    unsafe {
        if location_kind != CLAP_PRESET_DISCOVERY_LOCATION_FILE || location.is_null() {
            return false;
        }
        let Ok(path) = CStr::from_ptr(location).to_str() else {
            return false;
        };
        let data = data_from_plugin::<P>(plugin);

        // Same classification as a session load: an envelope under a
        // different plugin id (a pre-identity-change save) is offered
        // to the plugin's `migrate_state` hook instead of refused
        // outright. Discovery only lists same-id presets, but a host
        // can hand any path here (drag-drop, saved location).
        let Some(bytes) = std::fs::read(Path::new(path)).ok() else {
            return false;
        };
        let Some((_, blob)) = parse_preset_file(&bytes) else {
            return false;
        };
        let Some(deserialized) =
            state::parse_or_migrate::<P>(&blob, data.plugin_id_hash, PluginFormat::Clap, None)
        else {
            return false;
        };

        state::apply_params(&*data.params_arc, &deserialized);
        let _ = data.pending_state.force_push(deserialized);
        if let Some(ref mut editor) = data.editor {
            editor.state_changed();
        }

        // Tell the host the preset landed so it can update its
        // preset chrome (Bitwig's preset name display reads this).
        if !data.host.is_null()
            && let Some(get_ext) = (*data.host).get_extension
        {
            let host_ext =
                get_ext(data.host, CLAP_EXT_PRESET_LOAD.as_ptr()).cast::<clap_host_preset_load>();
            if !host_ext.is_null()
                && let Some(loaded) = (*host_ext).loaded
            {
                loaded(data.host, location_kind, location, load_key);
            }
        }
        true
    }
}

fn make_preset_load_extension<P: PluginExport>() -> clap_plugin_preset_load {
    clap_plugin_preset_load {
        from_location: Some(preset_load_from_location::<P>),
    }
}

// ---------------------------------------------------------------------------
// Extension: audio_ports
// ---------------------------------------------------------------------------

/// The `bus_layouts()` index the host selected through
/// `clap.audio-ports-config`, clamped in range. Falls back to 0 for a
/// single-layout plugin or a null handle.
unsafe fn selected_layout_index<P: PluginExport>(
    plugin: *const clap_plugin,
    layouts_len: usize,
) -> usize {
    if layouts_len <= 1 || plugin.is_null() {
        return 0;
    }
    let data = unsafe { data_from_plugin::<P>(plugin) };
    (data.selected_config.load(Ordering::Relaxed) as usize).min(layouts_len - 1)
}

/// The CLAP main-port-type string for a channel config (`null` for a
/// custom count, which CLAP leaves untyped).
fn clap_port_type_ptr(channels: ChannelConfig) -> *const c_char {
    match channels {
        ChannelConfig::Mono => CLAP_PORT_MONO.as_ptr(),
        ChannelConfig::Stereo => CLAP_PORT_STEREO.as_ptr(),
        ChannelConfig::Custom(_) => ptr::null(),
    }
}

unsafe extern "C" fn audio_ports_count<P: PluginExport>(
    plugin: *const clap_plugin,
    is_input: bool,
) -> u32 {
    let layouts = P::bus_layouts();
    let idx = unsafe { selected_layout_index::<P>(plugin, layouts.len()) };
    let Some(layout) = layouts.get(idx) else {
        return 0;
    };
    if is_input {
        len_u32(layout.inputs.len())
    } else {
        len_u32(layout.outputs.len())
    }
}

unsafe extern "C" fn audio_ports_get<P: PluginExport>(
    plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_audio_port_info,
) -> bool {
    unsafe {
        let layouts = P::bus_layouts();
        let idx = selected_layout_index::<P>(plugin, layouts.len());
        let Some(layout) = layouts.get(idx) else {
            return false;
        };

        let buses = if is_input {
            &layout.inputs
        } else {
            &layout.outputs
        };

        let Some(bus) = buses.get(index as usize) else {
            return false;
        };

        let out = &mut *info;
        out.id = index;
        out.name = [0; CLAP_NAME_SIZE];
        copy_str_to_buf(&mut out.name, bus.name);
        out.channel_count = bus.channels.channel_count();
        out.flags = if index == 0 {
            CLAP_AUDIO_PORT_IS_MAIN
        } else {
            0
        };
        // f64 plugins take the host's 64-bit wire directly (zero
        // copy, no precision loss at the boundary); the process loop
        // reads whichever of data32/data64 the host picked per port.
        if <P as PluginRuntime>::Sample::IS_F64 {
            out.flags |= CLAP_AUDIO_PORT_SUPPORTS_64BITS | CLAP_AUDIO_PORT_PREFERS_64BITS;
        }
        out.port_type = match bus.channels {
            ChannelConfig::Mono => CLAP_PORT_MONO.as_ptr(),
            ChannelConfig::Stereo => CLAP_PORT_STEREO.as_ptr(),
            ChannelConfig::Custom(_) => ptr::null(),
        };
        out.in_place_pair = CLAP_INVALID_ID;

        true
    }
}

fn make_audio_ports_extension<P: PluginExport>() -> clap_plugin_audio_ports {
    clap_plugin_audio_ports {
        count: Some(audio_ports_count::<P>),
        get: Some(audio_ports_get::<P>),
    }
}

// ---------------------------------------------------------------------------
// Extension: audio_ports_config (switch between the plugin's bus layouts)
// ---------------------------------------------------------------------------

unsafe extern "C" fn audio_ports_config_count<P: PluginExport>(_plugin: *const clap_plugin) -> u32 {
    len_u32(P::bus_layouts().len())
}

unsafe extern "C" fn audio_ports_config_get<P: PluginExport>(
    _plugin: *const clap_plugin,
    index: u32,
    config: *mut clap_audio_ports_config,
) -> bool {
    unsafe {
        let layouts = P::bus_layouts();
        let Some(layout) = layouts.get(index as usize) else {
            return false;
        };
        let out = &mut *config;
        // Config id == index into `bus_layouts()`; `select` maps back.
        out.id = index;
        out.name = [0; CLAP_NAME_SIZE];
        let name = format!(
            "{} in / {} out",
            layout.total_input_channels(),
            layout.total_output_channels()
        );
        copy_str_to_buf(&mut out.name, &name);
        out.input_port_count = len_u32(layout.inputs.len());
        out.output_port_count = len_u32(layout.outputs.len());
        out.has_main_input = !layout.inputs.is_empty();
        out.main_input_channel_count = layout
            .inputs
            .first()
            .map_or(0, |b| b.channels.channel_count());
        out.main_input_port_type = layout
            .inputs
            .first()
            .map_or(ptr::null(), |b| clap_port_type_ptr(b.channels));
        out.has_main_output = !layout.outputs.is_empty();
        out.main_output_channel_count = layout
            .outputs
            .first()
            .map_or(0, |b| b.channels.channel_count());
        out.main_output_port_type = layout
            .outputs
            .first()
            .map_or(ptr::null(), |b| clap_port_type_ptr(b.channels));
        true
    }
}

unsafe extern "C" fn audio_ports_config_select<P: PluginExport>(
    plugin: *const clap_plugin,
    config_id: clap_id,
) -> bool {
    unsafe {
        if (config_id as usize) >= P::bus_layouts().len() {
            return false;
        }
        // Called on the main thread while deactivated; the host rescans
        // `audio_ports` afterward, which now reports this layout.
        data_from_plugin::<P>(plugin)
            .selected_config
            .store(config_id, Ordering::Relaxed);
        true
    }
}

fn make_audio_ports_config_extension<P: PluginExport>() -> clap_plugin_audio_ports_config {
    clap_plugin_audio_ports_config {
        count: Some(audio_ports_config_count::<P>),
        get: Some(audio_ports_config_get::<P>),
        select: Some(audio_ports_config_select::<P>),
    }
}

// ---------------------------------------------------------------------------
// Extension: note_ports (only for instruments)
// ---------------------------------------------------------------------------

unsafe extern "C" fn note_ports_count<P: PluginExport>(
    _plugin: *const clap_plugin,
    is_input: bool,
) -> u32 {
    // Declare the plugin's MIDI ports for this direction. Count comes
    // from `PluginInfo` (category default of 0-or-1, raised by
    // `midi_input_ports` / `midi_output_ports`). A plain audio effect
    // declares neither direction.
    let info = P::info();
    let ports = if is_input {
        info.midi_input_ports
    } else {
        info.midi_output_ports
    };
    u32::from(ports)
}

unsafe extern "C" fn note_ports_get<P: PluginExport>(
    _plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_note_port_info,
) -> bool {
    unsafe {
        let in_ports = u32::from(P::info().midi_input_ports);
        let out_ports = u32::from(P::info().midi_output_ports);
        let (ports, opposite) = if is_input {
            (in_ports, out_ports)
        } else {
            (out_ports, in_ports)
        };
        // clap-validator's output-port sweep queries with
        // `is_input = true` (its note_ports.rs), so a plugin with more
        // note outputs than inputs fails every test that fetches the
        // note-port config. Answer an out-of-range query with the
        // matching port of the *other* direction - what the sweep meant
        // to ask. `ports` flips along with the direction so the name
        // below numbers by the resolved direction's count (the same
        // port id must always answer with the same name). Compliant
        // hosts iterate `0..count(direction)` and can never reach
        // this; remove when the validator queries the direction it
        // iterates.
        let (is_input, ports) = if index < ports {
            (is_input, ports)
        } else if index < opposite {
            (!is_input, opposite)
        } else {
            return false;
        };

        let out = &mut *info;
        // Port ids must be unique across the plugin. Fold the direction
        // bit and the index together: input k -> 2k, output k -> 2k+1.
        out.id = (index << 1) | u32::from(!is_input);
        // Notes go out as `CLAP_EVENT_NOTE`, but every other
        // channel-voice message and SysEx go out as `CLAP_EVENT_MIDI`
        // (raw MIDI dialect), so advertise both dialects or a
        // dialect-routing host drops the raw-MIDI events. Keep CLAP
        // preferred: our own note events are CLAP-dialect, and
        // preferring MIDI would also pull host input through the
        // raw-MIDI path.
        out.supported_dialects = CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI;
        out.preferred_dialect = CLAP_NOTE_DIALECT_CLAP;
        // A port that opted into MIDI 2.0 also carries UMP: advertise
        // the MIDI2 dialect so the host routes `CLAP_EVENT_MIDI2` to us
        // (in) and reads it back (out). Still CLAP-preferred - MIDI2 is
        // the opt-in native path, not the default.
        let dialect = if is_input {
            P::info().midi_input_dialect
        } else {
            P::info().midi_output_dialect
        };
        if dialect == MidiDialect::Midi2 {
            out.supported_dialects |= CLAP_NOTE_DIALECT_MIDI2;
        }
        out.name = [0; CLAP_NAME_SIZE];
        let base = if is_input {
            "Note Input"
        } else {
            "Note Output"
        };
        // Number the ports only when there's more than one, so the
        // single-port case keeps its plain label. Not the audio thread -
        // allocation here is fine.
        let name = if ports > 1 {
            format!("{base} {}", index + 1)
        } else {
            base.to_string()
        };
        copy_str_to_buf(&mut out.name, &name);

        true
    }
}

fn make_note_ports_extension<P: PluginExport>() -> clap_plugin_note_ports {
    clap_plugin_note_ports {
        count: Some(note_ports_count::<P>),
        get: Some(note_ports_get::<P>),
    }
}

// ---------------------------------------------------------------------------
// GUI extension
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
use clap_sys::ext::gui::CLAP_WINDOW_API_COCOA;
#[cfg(target_os = "windows")]
use clap_sys::ext::gui::CLAP_WINDOW_API_WIN32;
#[cfg(target_os = "linux")]
use clap_sys::ext::gui::CLAP_WINDOW_API_X11;
use clap_sys::ext::gui::{
    CLAP_EXT_GUI, clap_gui_resize_hints, clap_host_gui, clap_plugin_gui, clap_window,
};

unsafe extern "C" fn gui_is_api_supported<P: PluginExport>(
    _plugin: *const clap_plugin,
    api: *const c_char,
    is_floating: bool,
) -> bool {
    unsafe {
        if is_floating {
            return false;
        }
        let api = CStr::from_ptr(api);
        #[cfg(target_os = "macos")]
        if api == CLAP_WINDOW_API_COCOA {
            return true;
        }
        #[cfg(target_os = "windows")]
        if api == CLAP_WINDOW_API_WIN32 {
            return true;
        }
        #[cfg(target_os = "linux")]
        if api == CLAP_WINDOW_API_X11 {
            return true;
        }
        false
    }
}

unsafe extern "C" fn gui_get_preferred_api<P: PluginExport>(
    _plugin: *const clap_plugin,
    api: *mut *const c_char,
    is_floating: *mut bool,
) -> bool {
    unsafe {
        #[cfg(target_os = "macos")]
        {
            *api = CLAP_WINDOW_API_COCOA.as_ptr();
            *is_floating = false;
            return true;
        }
        #[cfg(target_os = "windows")]
        {
            *api = CLAP_WINDOW_API_WIN32.as_ptr();
            *is_floating = false;
            return true;
        }
        #[cfg(target_os = "linux")]
        {
            *api = CLAP_WINDOW_API_X11.as_ptr();
            *is_floating = false;
            return true;
        }
        #[allow(unreachable_code)]
        false
    }
}

unsafe extern "C" fn gui_create<P: PluginExport>(
    plugin: *const clap_plugin,
    _api: *const c_char,
    _is_floating: bool,
) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        if data.gui_created {
            return true;
        }
        // Built through the cached lock-free editor factory, so opening
        // the GUI never stalls the audio thread (and `--shell` rebuilds
        // from the reloaded dylib).
        data.editor = (data.editor_builder)(data.params_arc.clone());
        data.gui_created = data.editor.is_some();
        data.gui_created
    }
}

unsafe extern "C" fn gui_destroy<P: PluginExport>(plugin: *const clap_plugin) {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        if let Some(editor) = data.editor.as_mut() {
            // Same FFI-boundary protection as `clap_plugin_destroy`:
            // any panic during `editor.close()` (wgpu surface
            // drop, NSView teardown, baseview window close) would
            // otherwise become an unhandled Obj-C exception in
            // the host.
            let editor_ptr: *mut dyn Editor = editor.as_mut();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (*editor_ptr).close();
            }));
        }
        data.editor = None;
        data.gui_created = false;
    }
}

unsafe extern "C" fn gui_set_scale<P: PluginExport>(
    plugin: *const clap_plugin,
    scale: f64,
) -> bool {
    unsafe {
        if !scale.is_finite() || scale <= 0.0 {
            return false;
        }
        let data = data_from_plugin::<P>(plugin);
        data.host_scale = scale;
        data.host_scale_set_by_host = true;
        if let Some(ref mut editor) = data.editor {
            editor.set_scale_factor(scale);
        }
        true
    }
}

unsafe extern "C" fn gui_get_size<P: PluginExport>(
    plugin: *const clap_plugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        if let Some(ref editor) = data.editor {
            let (w, h) = editor.size();
            // Like VST3, the CLAP spec describes gui size as pixels, but
            // macOS AppKit handles Retina backing automatically. On macOS
            // we report logical points and let the host / OS scale; on
            // Windows/Linux we multiply by the host-reported scale (default
            // 1.0 if the host never called `gui.set_scale`).
            #[cfg(target_os = "macos")]
            {
                *width = w;
                *height = h;
            }
            #[cfg(not(target_os = "macos"))]
            {
                // Round-to-nearest, not truncate - `(w * scale) as u32`
                // would round 199.9 → 199, drifting one pixel on
                // fractional scales. Matches VST3 / AAX / the
                // `to_physical_px` helper used elsewhere. Logical
                // pixel sizes are bounded by `u32::MAX / scale`; in
                // practice no editor exceeds 16384 logical pixels, so
                // the `f64 → u32` truncation/sign casts are safe.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                {
                    *width = (f64::from(w) * data.host_scale).round() as u32;
                    *height = (f64::from(h) * data.host_scale).round() as u32;
                }
            }
            return true;
        }
        false
    }
}

unsafe extern "C" fn gui_can_resize<P: PluginExport>(plugin: *const clap_plugin) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        data.editor.as_ref().is_some_and(|e| e.can_resize())
    }
}

unsafe extern "C" fn gui_set_parent<P: PluginExport>(
    plugin: *const clap_plugin,
    window: *const clap_window,
) -> bool {
    unsafe {
        // Wrap in catch_unwind to prevent panics from aborting the host.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gui_set_parent_inner::<P>(plugin, window)
        }));
        match result {
            Ok(v) => v,
            Err(e) => {
                let msg = if let Some(s) = e.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = e.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                log::error!("clap gui_set_parent panic swallowed: {msg}");
                false
            }
        }
    }
}

unsafe fn gui_set_parent_inner<P: PluginExport>(
    plugin: *const clap_plugin,
    window: *const clap_window,
) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        let Some(editor) = data.editor.as_mut() else {
            return false;
        };

        #[cfg(target_os = "macos")]
        let parent_ptr = (*window).specific.cocoa;
        #[cfg(target_os = "windows")]
        let parent_ptr = (*window).specific.win32;
        #[cfg(target_os = "linux")]
        let parent_ptr = (*window).specific.ptr;

        if parent_ptr.is_null() {
            return false;
        }

        let params = Arc::clone(&data.params_arc);
        let meter_store = Arc::clone(&data.meter_store);
        let snapshot = Arc::clone(&data.snapshot);
        let gui_changes = data.gui_changes.clone();
        let gui_changes2 = data.gui_changes.clone();
        let gui_changes3 = data.gui_changes.clone();
        let host = SendPtr::new(data.host);
        let host_params = SendPtr::new(data.host_params);
        let request_flush = move || {
            // `host_params` is null when the host omits the optional
            // `clap_host_params` extension (the spec marks it optional);
            // dereferencing it would crash hosts that don't implement
            // params.
            let hp = host_params.as_ptr();
            if hp.is_null() {
                return;
            }
            if let Some(f) = (*hp).request_flush {
                f(host.as_ptr());
            }
        };
        // `request_flush` is a `move ||` over only `Copy` captures, so the closure
        // itself is `Copy` and we re-bind rather than `.clone()`.
        let request_flush2 = request_flush;
        let request_flush3 = request_flush;
        let needs_rescan = data.needs_rescan.clone();
        let host_for_callback = SendPtr::new(data.host);
        let params_for_set = params.clone();
        let params_for_get = params.clone();
        let params_for_plain = params.clone();
        let params_for_fmt = params.clone();
        let params_for_ctx = params.clone();
        let task_spawner_for_ctx = data.task_spawner.clone();
        let pending_state_for_set = data.pending_state.clone();
        let transport_slot = data.transport_slot.clone();
        let context = PluginContext::from_closures(
            ClosureBridge {
                begin_edit: Box::new(move |id| {
                    // Push can fail only if the audio thread hasn't
                    // drained for `GUI_QUEUE_CAPACITY` gestures.
                    // request_flush below pokes the host to call
                    // back; needs_rescan in set_param ensures the
                    // param tree picks up the latest plain values
                    // even if a begin/end pair gets dropped.
                    let _ = gui_changes.push(GuiParamChange::GestureBegin(id));
                    request_flush();
                }),
                set_param: Box::new(move |id, value| {
                    let plain = params_for_set.set_normalized_returning_plain(id, value);
                    let _ = gui_changes2.push(GuiParamChange::Value(id, plain));
                    request_flush2();
                    // Symmetry with the host-pointer null guards at
                    // `:297, :365, :1404`. CLAP guarantees a valid
                    // host on init, but a host that creates a plugin
                    // without ever calling `clap_plugin_init`-style
                    // setup (rare validators) could leave `data.host`
                    // null; the deref below would crash inside the
                    // GUI thread.
                    let host_ptr = host_for_callback.as_ptr();
                    if !needs_rescan.swap(true, Ordering::Relaxed)
                        && !host_ptr.is_null()
                        && let Some(req_cb) = (*host_ptr).request_callback
                    {
                        req_cb(host_ptr);
                    }
                }),
                end_edit: Box::new(move |id| {
                    let _ = gui_changes3.push(GuiParamChange::GestureEnd(id));
                    request_flush3();
                }),
                request_resize: Box::new({
                    let host_ptr = SendPtr::new(data.host);
                    let host_scale_for_resize = data.host_scale;
                    move |lw, lh| {
                        request_host_resize(host_ptr.as_ptr(), host_scale_for_resize, lw, lh)
                    }
                }),
                get_param: Box::new(move |id| params_for_get.get_normalized(id).unwrap_or(0.0)),
                get_param_plain: Box::new(move |id| params_for_plain.get_plain(id).unwrap_or(0.0)),
                format_param: Box::new(move |id| {
                    let plain = params_for_fmt.get_plain(id).unwrap_or(0.0);
                    params_for_fmt
                        .format_value(id, plain)
                        .unwrap_or_else(|| format!("{plain:.1}"))
                }),
                get_meter: Box::new(move |id| meter_store.read(id)),
                get_state: Box::new(move || {
                    // Editor state read: lock-free, reads the snapshot the
                    // audio thread publishes each block. Never touches the
                    // plugin, so an editor read can't stall audio.
                    save_extra(&snapshot)
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
                        persist: Vec::new(),
                    });
                }),
                transport: Box::new(move || transport_slot.read()),
            },
            params_for_ctx,
        )
        .with_tasks(task_spawner_for_ctx);

        #[cfg(target_os = "macos")]
        let handle = RawWindowHandle::AppKit(parent_ptr);
        #[cfg(target_os = "windows")]
        let handle = RawWindowHandle::Win32(parent_ptr);
        #[cfg(target_os = "linux")]
        let handle = RawWindowHandle::X11(parent_ptr as u64);

        editor.open(handle, context);
        // baseview attaches its child NSView at the parent's
        // `(0, 0)`. NSView's coordinate system is unflipped, so
        // `(0, 0)` is the parent's bottom-left - which renders the
        // editor anchored to the bottom-left of Reaper's plugin
        // area when Reaper opens the window at a size larger than
        // the editor's natural dimensions (the common case for
        // backends whose `editor.set_size` doesn't actually apply,
        // e.g. vizia: Reaper falls back to a default plugin-window
        // size and our child sits in the corner of all that extra
        // space). Re-anchor the child to the parent's top with a
        // fully-fixed autoresize mask (`0`) - the parent grows the
        // empty space below/right of the child rather than dragging
        // the child along. We re-pin per-frame from the editor
        // (`reanchor_to_superview_top` in `on_frame`) since
        // baseview's `setFrameSize:` notifications during embed can
        // override the origin we set here.
        #[cfg(target_os = "macos")]
        anchor_child_to_top(parent_ptr);
        true
    }
}

/// Move every direct subview of `parent` so its top edge sits at
/// the parent's top in unflipped Cocoa coordinates (where
/// `origin.y = 0` is the bottom edge). Pinning is via the
/// autoresize mask `NSViewMinYMargin | NSViewMaxXMargin`, which
/// keeps the gap between child-bottom and parent-bottom flexible
/// (so a taller parent grows the empty space below the child, not
/// above it) and the gap between child-right and parent-right
/// flexible (so a wider parent grows the empty space to the right
/// of the child). The child stays at its built size; `AppKit` just
/// repositions it as the parent resizes.
///
/// Mirrors `truce-lv2`'s `anchor_child_to_top` (the non-resizable
/// arm of `anchor_child_for_resize`). No `objc` dep needed in
/// truce-clap until the autoresize-mask install path comes back -
/// we use a local `class!`-free `msg_send!` via `objc2_app_kit`
/// types instead.
#[cfg(target_os = "macos")]
unsafe fn anchor_child_to_top(parent: *mut c_void) {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};
    // `MinYMargin | MaxXMargin`: bottom margin and right margin
    // elastic, view size + top/left margins fixed. As the parent
    // resizes, AppKit grows the empty space below/right of the
    // child rather than dragging the child along. Per-frame
    // `reanchor_to_superview_top` (from `on_frame`) catches any
    // host-driven setFrame that bypasses autoresize.
    const NSVIEW_MIN_Y_MARGIN: u64 = 8;
    const NSVIEW_MAX_X_MARGIN: u64 = 4;
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsPoint {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsSize {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsRect {
        origin: NsPoint,
        size: NsSize,
    }
    if parent.is_null() {
        return;
    }
    let parent_obj = parent.cast::<Object>();
    let parent_frame: NsRect = msg_send![parent_obj, frame];
    let subviews: *mut Object = msg_send![parent_obj, subviews];
    if subviews.is_null() {
        return;
    }
    let count: usize = msg_send![subviews, count];
    for i in 0..count {
        let child: *mut Object = msg_send![subviews, objectAtIndex: i];
        if child.is_null() {
            continue;
        }
        let child_frame: NsRect = msg_send![child, frame];
        let new_origin = NsPoint {
            x: child_frame.origin.x,
            y: parent_frame.size.height - child_frame.size.height,
        };
        let _: () = msg_send![child, setFrameOrigin: new_origin];
        let _: () =
            msg_send![child, setAutoresizingMask: NSVIEW_MIN_Y_MARGIN | NSVIEW_MAX_X_MARGIN];
    }
}

unsafe extern "C" fn gui_show<P: PluginExport>(_plugin: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn gui_hide<P: PluginExport>(_plugin: *const clap_plugin) -> bool {
    true
}

/// Reports horizontal/vertical/aspect-ratio constraints from the
/// editor. Returns `false` when the editor isn't resizable (Bitwig's
/// probe-call still gets a meaningful `false`, not a `None` slot).
unsafe extern "C" fn gui_get_resize_hints<P: PluginExport>(
    plugin: *const clap_plugin,
    hints: *mut clap_gui_resize_hints,
) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        let Some(editor) = data.editor.as_ref() else {
            return false;
        };
        if !editor.can_resize() {
            return false;
        }
        if hints.is_null() {
            return false;
        }
        let (aw, ah) = editor.aspect_ratio().unwrap_or((0, 0));
        *hints = clap_gui_resize_hints {
            can_resize_horizontally: true,
            can_resize_vertically: true,
            preserve_aspect_ratio: editor.aspect_ratio().is_some(),
            aspect_ratio_width: aw,
            aspect_ratio_height: ah,
        };
        true
    }
}

/// Stub: floating windows aren't supported (`is_api_supported` rejects
/// `is_floating=true`); `set_transient` is meaningless when the GUI is
/// always embedded. Present as `Some` for the same Bitwig probe-quirk
/// described on `gui_get_resize_hints`.
unsafe extern "C" fn gui_set_transient<P: PluginExport>(
    _plugin: *const clap_plugin,
    _window: *const clap_window,
) -> bool {
    false
}

/// Stub: the editor renders its own title bar; we don't need the host's
/// hint. Present as `Some` for the same Bitwig probe-quirk described
/// on `gui_get_resize_hints`.
unsafe extern "C" fn gui_suggest_title<P: PluginExport>(
    _plugin: *const clap_plugin,
    _title: *const c_char,
) {
}

/// Ask the host to resize its window to a logical size via
/// `clap_host_gui.request_resize`. Returns false when the host
/// doesn't expose the gui extension. Logical-to-physical scaling
/// matches `gui_get_size`.
unsafe fn request_host_resize(host: *const clap_host, host_scale: f64, lw: u32, lh: u32) -> bool {
    unsafe {
        if host.is_null() {
            return false;
        }
        let Some(get_ext) = (*host).get_extension else {
            return false;
        };
        let host_gui_ptr = get_ext(host, CLAP_EXT_GUI.as_ptr()).cast::<clap_host_gui>();
        if host_gui_ptr.is_null() {
            return false;
        }
        let Some(req) = (*host_gui_ptr).request_resize else {
            return false;
        };
        let (pw, ph) = scale_logical_to_physical(lw, lh, host_scale);
        req(host, pw, ph)
    }
}

/// Host commits a new size. When the editor opts into resizing,
/// delegate to `Editor::set_size`; otherwise accept only the
/// editor's current fixed size (Bitwig probe-quirk - see
/// `gui_get_resize_hints` rationale).
unsafe extern "C" fn gui_set_size<P: PluginExport>(
    plugin: *const clap_plugin,
    width: u32,
    height: u32,
) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        let Some(editor) = data.editor.as_mut() else {
            return false;
        };
        if editor.can_resize() {
            // Host passes physical points; truce works in logical.
            // Divide by the host-applied scale before handing to the
            // editor so `set_size` receives the same units `size()`
            // returns. Fit the largest on-ratio box *inside* the request:
            // a pure function of the request (no `current` dependence), so
            // it can't oscillate when a host re-asserts a pinned axis every
            // frame (Reaper caps the editor height at its FX-window content
            // height and reverts any grow past it).
            let req = scale_physical_to_logical(width, height, data.host_scale);
            let (lw, lh) = fit_logical_size(req.0, req.1, editor.as_ref());
            let accepted = editor.set_size(lw, lh);
            // The fit is `<=` the request, so this only ever asks the host to
            // shrink its window onto the on-ratio size (which hosts honour) -
            // never to grow past a pinned axis. Idempotent, so the host's
            // echoed `set_size` agrees and the exchange settles.
            let adopted = editor.size();
            if accepted && adopted != req {
                request_host_resize(data.host, data.host_scale, adopted.0, adopted.1);
            }
            accepted
        } else {
            let mut current_w: u32 = 0;
            let mut current_h: u32 = 0;
            if !gui_get_size::<P>(plugin, &raw mut current_w, &raw mut current_h) {
                return false;
            }
            width == current_w && height == current_h
        }
    }
}

/// Host asks "what's the nearest size you can render at?". CLAP
/// defines this as "clamp to acceptable", not "reject if not exact".
/// Resizable editors clamp to `min_size` / `max_size` and apply the
/// `aspect_ratio` constraint; fixed-size editors snap to current.
unsafe extern "C" fn gui_adjust_size<P: PluginExport>(
    plugin: *const clap_plugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        let Some(editor) = data.editor.as_ref() else {
            return false;
        };
        if editor.can_resize() {
            let req = scale_physical_to_logical(*width, *height, data.host_scale);
            let (lw, lh) = fit_logical_size(req.0, req.1, editor.as_ref());
            // Convert clamped logical back to physical for the host.
            let (pw, ph) = scale_logical_to_physical(lw, lh, data.host_scale);
            *width = pw;
            *height = ph;
            true
        } else {
            let mut current_w: u32 = 0;
            let mut current_h: u32 = 0;
            if !gui_get_size::<P>(plugin, &raw mut current_w, &raw mut current_h) {
                return false;
            }
            *width = current_w;
            *height = current_h;
            true
        }
    }
}

/// Convert physical points (what the host passes in resize APIs)
/// to logical points (what `Editor` works in). CLAP host scale is
/// 1.0 when the host hasn't called `set_scale`.
fn scale_physical_to_logical(pw: u32, ph: u32, host_scale: f64) -> (u32, u32) {
    if host_scale <= 0.0 || (host_scale - 1.0).abs() < f64::EPSILON {
        return (pw, ph);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lw = (f64::from(pw) / host_scale).round() as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lh = (f64::from(ph) / host_scale).round() as u32;
    (lw.max(1), lh.max(1))
}

/// Inverse of `scale_physical_to_logical`.
fn scale_logical_to_physical(lw: u32, lh: u32, host_scale: f64) -> (u32, u32) {
    if host_scale <= 0.0 || (host_scale - 1.0).abs() < f64::EPSILON {
        return (lw, lh);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pw = (f64::from(lw) * host_scale).round() as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ph = (f64::from(lh) * host_scale).round() as u32;
    (pw.max(1), ph.max(1))
}

fn make_gui_extension<P: PluginExport>() -> clap_plugin_gui {
    clap_plugin_gui {
        is_api_supported: Some(gui_is_api_supported::<P>),
        get_preferred_api: Some(gui_get_preferred_api::<P>),
        create: Some(gui_create::<P>),
        destroy: Some(gui_destroy::<P>),
        set_scale: Some(gui_set_scale::<P>),
        get_size: Some(gui_get_size::<P>),
        can_resize: Some(gui_can_resize::<P>),
        get_resize_hints: Some(gui_get_resize_hints::<P>),
        adjust_size: Some(gui_adjust_size::<P>),
        set_size: Some(gui_set_size::<P>),
        set_parent: Some(gui_set_parent::<P>),
        set_transient: Some(gui_set_transient::<P>),
        suggest_title: Some(gui_suggest_title::<P>),
        show: Some(gui_show::<P>),
        hide: Some(gui_hide::<P>),
    }
}

// ---------------------------------------------------------------------------
// get_extension
// ---------------------------------------------------------------------------

/// Holds the static extension structs. One per monomorphization, which is fine
/// because we only have one plugin type per shared library.
struct Extensions<P: PluginExport> {
    params: clap_plugin_params,
    state: clap_plugin_state,
    preset_load: clap_plugin_preset_load,
    audio_ports: clap_plugin_audio_ports,
    audio_ports_config: clap_plugin_audio_ports_config,
    note_ports: clap_plugin_note_ports,
    gui: clap_plugin_gui,
    latency: clap_plugin_latency,
    tail: clap_plugin_tail,
    render: clap_plugin_render,
    _phantom: PhantomData<P>,
}

unsafe extern "C" fn latency_get<P: PluginExport>(plugin: *const clap_plugin) -> u32 {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        data.latency_cache.load(Ordering::Relaxed)
    }
}

unsafe extern "C" fn tail_get<P: PluginExport>(plugin: *const clap_plugin) -> u32 {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        data.tail_cache.load(Ordering::Relaxed)
    }
}

/// `clap.render`: we have no hard realtime requirement - the plugin can
/// render offline. Returning `false` lets hosts drive an offline bounce.
unsafe extern "C" fn render_has_hard_realtime<P: PluginExport>(
    _plugin: *const clap_plugin,
) -> bool {
    false
}

/// `clap.render::set`: the host announces realtime vs offline. Store the
/// mode; `activate` re-preps with it (hosts deactivate before an offline
/// bounce), and each `process` block reads it for the per-block mode.
unsafe extern "C" fn render_set<P: PluginExport>(
    plugin: *const clap_plugin,
    mode: clap_plugin_render_mode,
) -> bool {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        let pm = if mode == CLAP_RENDER_OFFLINE {
            ProcessMode::Offline
        } else {
            ProcessMode::Realtime
        };
        data.render_mode.store(pm.as_u8(), Ordering::Relaxed);
        true
    }
}

impl<P: PluginExport> Extensions<P> {
    fn new() -> Self {
        Self {
            params: make_params_extension::<P>(),
            state: make_state_extension::<P>(),
            preset_load: make_preset_load_extension::<P>(),
            audio_ports: make_audio_ports_extension::<P>(),
            audio_ports_config: make_audio_ports_config_extension::<P>(),
            note_ports: make_note_ports_extension::<P>(),
            gui: make_gui_extension::<P>(),
            latency: clap_plugin_latency {
                get: Some(latency_get::<P>),
            },
            tail: clap_plugin_tail {
                get: Some(tail_get::<P>),
            },
            render: clap_plugin_render {
                has_hard_realtime_requirement: Some(render_has_hard_realtime::<P>),
                set: Some(render_set::<P>),
            },
            _phantom: PhantomData,
        }
    }

    /// Get or initialize the singleton extensions struct.
    ///
    /// Backed by a function-local `OnceLock` keyed off a leaked
    /// `Box<Self>`. The `OnceLock` itself stores the pointer as
    /// `usize` because Rust forbids generic statics - a literal
    /// `OnceLock<Extensions<P>>` static can't reference the outer
    /// generic parameter, so we erase to `usize` and re-attach the
    /// type on read. `OnceLock::get_or_init` runs the constructor at
    /// most once across all threads, so no losing `Box` ever gets
    /// built-and-thrown-away on a race the way a hand-rolled
    /// `AtomicPtr<u8>` + `compare_exchange` would.
    ///
    /// CLAP libraries only ship one plugin type per shared object, so
    /// there's exactly one monomorphization and one `OnceLock` per
    /// binary in practice.
    fn get() -> &'static Self {
        static PTR: OnceLock<usize> = OnceLock::new();
        let raw = *PTR.get_or_init(|| Box::into_raw(Box::new(Self::new())) as usize);
        // SAFETY: `raw` was produced by `Box::into_raw(Box::new(Self::new()))`
        // inside `get_or_init`, runs at most once, and is never freed; the
        // type matches because only one monomorphization of `Extensions<P>`
        // exists per binary.
        unsafe { &*(raw as *const Self) }
    }
}

unsafe extern "C" fn clap_plugin_get_extension<P: PluginExport>(
    _plugin: *const clap_plugin,
    id: *const c_char,
) -> *const c_void {
    unsafe {
        if id.is_null() {
            return ptr::null();
        }
        let ext_id = CStr::from_ptr(id);

        let extensions = Extensions::<P>::get();

        if ext_id == CLAP_EXT_PARAMS {
            return (&raw const extensions.params).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_STATE {
            return (&raw const extensions.state).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_PRESET_LOAD || ext_id == CLAP_EXT_PRESET_LOAD_COMPAT {
            return (&raw const extensions.preset_load).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_AUDIO_PORTS {
            return (&raw const extensions.audio_ports).cast::<c_void>();
        }
        // Only advertise config switching when there's more than one
        // layout to switch between; a single-layout plugin is fully
        // described by `audio_ports` alone.
        if ext_id == CLAP_EXT_AUDIO_PORTS_CONFIG && P::bus_layouts().len() > 1 {
            return (&raw const extensions.audio_ports_config).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_NOTE_PORTS {
            return (&raw const extensions.note_ports).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_GUI {
            return (&raw const extensions.gui).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_LATENCY {
            return (&raw const extensions.latency).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_TAIL {
            return (&raw const extensions.tail).cast::<c_void>();
        }
        if ext_id == CLAP_EXT_RENDER {
            return (&raw const extensions.render).cast::<c_void>();
        }
        ptr::null()
    }
}

// ---------------------------------------------------------------------------
// Factory: create_plugin
// ---------------------------------------------------------------------------

/// Create a `clap_plugin` instance for the given plugin type.
///
/// # Safety
/// Called by the host through the factory. The descriptor must remain valid
/// for the lifetime of the returned plugin.
pub unsafe fn create_plugin_instance<P: PluginExport>(
    descriptor: *const clap_plugin_descriptor,
    host: *const clap_host,
) -> *const clap_plugin {
    let instance = P::create();
    let info = P::info();
    let plugin_id_hash = state::hash_plugin_id(info.clap_id);
    let param_infos = instance.params().param_infos();
    let params_arc = instance.params_arc();
    let meter_store = instance.meter_store();
    let snapshot = instance.snapshot_slot();
    let task_spawner = instance.task_spawner();
    let editor_builder = instance.editor_builder();
    let latency_cache = AtomicU32::new(instance.latency());
    let tail_cache = AtomicU32::new(instance.tail());

    // Pre-size the per-block channel-slice scratch from the worst-case
    // bus layout the plugin advertises. Without this, the first
    // `clap_plugin_process` call after activate hits the global
    // allocator on the audio thread for every channel push; this
    // amortizes the cost into instance creation, where it belongs.
    let layouts = P::bus_layouts();
    let max_in = layouts
        .iter()
        .map(|l| l.total_input_channels() as usize)
        .max()
        .unwrap_or(0);
    let max_out = layouts
        .iter()
        .map(|l| l.total_output_channels() as usize)
        .max()
        .unwrap_or(0);

    let data = Box::new(ClapPluginData::<P> {
        plugin: shared_plugin(instance),
        params_arc,
        meter_store,
        snapshot,
        task_spawner,
        editor_builder,
        latency_cache,
        tail_cache,
        selected_config: AtomicU32::new(0),
        event_list: EventList::with_capacity(EVENT_LIST_PREALLOC),
        sounding_notes: SoundingNotes::new(info.midi_input_ports),
        output_events: EventList::with_capacity(EVENT_LIST_PREALLOC),
        sub_event_scratch: EventList::with_capacity(EVENT_LIST_PREALLOC),
        param_infos,
        sample_rate: 44100.0,
        max_block_size: 1024,
        info,
        plugin_id_hash,
        editor: None,
        gui_created: false,
        host,
        host_params: ptr::null(),
        host_latency: ptr::null(),
        latency_dirty: AtomicBool::new(false),
        gui_changes: Arc::new(GuiChangeQueue::new(GUI_QUEUE_CAPACITY)),
        pending_state: Arc::new(StateLoadQueue::new(1)),
        active: AtomicBool::new(false),
        render_mode: AtomicU8::new(ProcessMode::Realtime.as_u8()),
        needs_rescan: Arc::new(AtomicBool::new(false)),
        transport_slot: TransportSlot::new(),
        host_scale: 1.0,
        host_scale_set_by_host: false,
        input_slices: Vec::with_capacity(max_in),
        output_slices: Vec::with_capacity(max_out),
        input_widen: Vec::with_capacity(max_in),
        output_narrow: Vec::with_capacity(max_out),
        host_out_ptrs: Vec::with_capacity(max_out),
    });

    let clap = Box::new(clap_plugin {
        desc: descriptor,
        plugin_data: Box::into_raw(data).cast::<c_void>(),
        init: Some(clap_plugin_init::<P>),
        destroy: Some(clap_plugin_destroy::<P>),
        activate: Some(clap_plugin_activate::<P>),
        deactivate: Some(clap_plugin_deactivate::<P>),
        start_processing: Some(clap_plugin_start_processing::<P>),
        stop_processing: Some(clap_plugin_stop_processing::<P>),
        reset: Some(clap_plugin_reset::<P>),
        process: Some(clap_plugin_process::<P>),
        get_extension: Some(clap_plugin_get_extension::<P>),
        on_main_thread: Some(clap_plugin_on_main_thread::<P>),
    });

    Box::into_raw(clap).cast_const()
}

// ---------------------------------------------------------------------------
// export_clap! macro
// ---------------------------------------------------------------------------

/// Export a CLAP plugin entry point.
///
/// Usage:
/// ```ignore
/// export_clap!(MyPlugin);
/// ```
///
/// Where `MyPlugin` implements `PluginExport`.
#[macro_export]
macro_rules! export_clap {
    ($plugin_type:ty) => {
        mod _clap_entry {
            use super::*;
            use std::ffi::{CStr, c_char, c_void};
            use std::path::Path;
            use std::ptr;
            use std::sync::OnceLock;

            use ::clap_sys::entry::clap_plugin_entry;
            use ::clap_sys::factory::plugin_factory::{
                CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory,
            };
            use ::clap_sys::factory::preset_discovery::{
                CLAP_PRESET_DISCOVERY_FACTORY_ID, CLAP_PRESET_DISCOVERY_FACTORY_ID_COMPAT,
            };
            use ::clap_sys::host::clap_host;
            use ::clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
            use ::clap_sys::version::CLAP_VERSION;

            use ::truce_clap::__macro_deps::truce_core::plugin::PluginRuntime;
            use ::truce_clap::DescriptorHolder;

            static DESCRIPTOR: OnceLock<DescriptorHolder> = OnceLock::new();

            fn get_descriptor() -> &'static DescriptorHolder {
                DESCRIPTOR.get_or_init(|| {
                    let info = <$plugin_type as PluginRuntime>::info();
                    DescriptorHolder::new(&info)
                })
            }

            static FACTORY: clap_plugin_factory = clap_plugin_factory {
                get_plugin_count: Some(factory_get_plugin_count),
                get_plugin_descriptor: Some(factory_get_plugin_descriptor),
                create_plugin: Some(factory_create_plugin),
            };

            unsafe extern "C" fn factory_get_plugin_count(
                _factory: *const clap_plugin_factory,
            ) -> u32 {
                1
            }

            unsafe extern "C" fn factory_get_plugin_descriptor(
                _factory: *const clap_plugin_factory,
                index: u32,
            ) -> *const clap_plugin_descriptor {
                if index == 0 {
                    &get_descriptor().descriptor as *const clap_plugin_descriptor
                } else {
                    ptr::null()
                }
            }

            unsafe extern "C" fn factory_create_plugin(
                _factory: *const clap_plugin_factory,
                host: *const clap_host,
                plugin_id: *const c_char,
            ) -> *const clap_plugin {
                if plugin_id.is_null() {
                    return ptr::null();
                }
                let requested_id = CStr::from_ptr(plugin_id);
                let info = <$plugin_type as PluginRuntime>::info();
                let our_id = match std::ffi::CString::new(info.clap_id) {
                    Ok(s) => s,
                    Err(_) => return ptr::null(),
                };
                if requested_id != our_id.as_c_str() {
                    return ptr::null();
                }
                let descriptor = &get_descriptor().descriptor as *const clap_plugin_descriptor;
                ::truce_clap::create_plugin_instance::<$plugin_type>(descriptor, host)
            }

            unsafe extern "C" fn entry_init(plugin_path: *const c_char) -> bool {
                // Force descriptor initialization.
                let _ = get_descriptor();
                // Remember where the host loaded us from - the preset
                // discovery factory derives the factory-preset
                // location from it.
                ::truce_clap::presets::set_plugin_path(plugin_path);
                true
            }

            unsafe extern "C" fn entry_deinit() {}

            unsafe extern "C" fn entry_get_factory(factory_id: *const c_char) -> *const c_void {
                if factory_id.is_null() {
                    return ptr::null();
                }
                let id = CStr::from_ptr(factory_id);
                if id == CLAP_PLUGIN_FACTORY_ID {
                    return &FACTORY as *const clap_plugin_factory as *const c_void;
                }
                if id == CLAP_PRESET_DISCOVERY_FACTORY_ID
                    || id == CLAP_PRESET_DISCOVERY_FACTORY_ID_COMPAT
                {
                    return ::truce_clap::presets::discovery_factory::<$plugin_type>()
                        as *const c_void;
                }
                ptr::null()
            }

            #[unsafe(no_mangle)]
            #[allow(non_upper_case_globals)]
            pub static clap_entry: clap_plugin_entry = clap_plugin_entry {
                clap_version: CLAP_VERSION,
                init: Some(entry_init),
                deinit: Some(entry_deinit),
                get_factory: Some(entry_get_factory),
            };
        }
    };
}

#[cfg(test)]
mod transport_tests {
    use super::{
        CLAP_SECTIME_FACTOR, CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, build_transport_info,
        clap_event_transport,
    };

    #[test]
    fn position_samples_derived_from_seconds_timeline() {
        // SAFETY: clap_event_transport is a repr(C) POD; all-zero is a
        // valid value (no flags set, all positions 0).
        let mut t: clap_event_transport = unsafe { std::mem::zeroed() };
        t.flags = CLAP_TRANSPORT_HAS_SECONDS_TIMELINE;
        // 1.5 seconds, in CLAP's power-of-two fixed point (no float cast).
        t.song_pos_seconds = CLAP_SECTIME_FACTOR + CLAP_SECTIME_FACTOR / 2;
        let info = build_transport_info(&t, 48_000.0);
        assert_eq!(info.position_samples, 72_000); // 1.5 s * 48 kHz
        assert!((info.position_seconds - 1.5).abs() < 1e-9);
    }

    #[test]
    fn position_samples_zero_without_seconds_timeline() {
        // SAFETY: see above - zeroed POD, no seconds-timeline flag.
        let t: clap_event_transport = unsafe { std::mem::zeroed() };
        assert_eq!(build_transport_info(&t, 48_000.0).position_samples, 0);
    }
}

#[cfg(test)]
mod note_expression_tests {
    use super::{
        CLAP_NOTE_EXPRESSION_TUNING, CLAP_NOTE_EXPRESSION_VOLUME, clap_event_note_expression,
        clap_note_expression_of, note_expression_body,
    };
    use truce_core::events::EventBody;

    fn expression(id: i32, value: f64) -> clap_event_note_expression {
        // SAFETY: clap_event_note_expression is a repr(C) POD; all-zero
        // is valid (channel 0, key 0).
        let mut ne: clap_event_note_expression = unsafe { std::mem::zeroed() };
        ne.expression_id = id;
        ne.value = value;
        ne
    }

    #[test]
    fn volume_crosses_in_the_gain_domain() {
        // CLAP `VOLUME` is plain linear gain `0..=4`; wire full-scale
        // must land on gain 4 (+12 dB), unity on the quarter point.
        let (id, _, _, value) = clap_note_expression_of(&EventBody::PerNoteCC {
            group: 0,
            channel: 0,
            note: 60,
            cc: 7,
            value: u32::MAX,
            registered: true,
        })
        .expect("volume maps");
        assert_eq!(id, CLAP_NOTE_EXPRESSION_VOLUME);
        assert!((value - 4.0).abs() < 1e-9);

        // Host unity gain decodes to the wire's quarter point.
        let body = note_expression_body(&expression(CLAP_NOTE_EXPRESSION_VOLUME, 1.0), 0, 60)
            .expect("volume decodes");
        let EventBody::PerNoteCC { cc: 7, value, .. } = body else {
            panic!("expected volume PerNoteCC, got {body:?}");
        };
        assert!((f64::from(value) / f64::from(u32::MAX) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn assignable_per_note_cc_is_not_an_expression() {
        // Only registered per-note indices carry the predefined
        // expression semantics; an assignable index 7 is not volume.
        assert!(
            clap_note_expression_of(&EventBody::PerNoteCC {
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
    fn tuning_full_scale_is_48_semitones() {
        let (_, _, _, semis) = clap_note_expression_of(&EventBody::PerNotePitchBend {
            group: 0,
            channel: 0,
            note: 60,
            value: u32::MAX,
        })
        .expect("tuning maps");
        assert!((semis - 48.0).abs() < 1e-6);

        // A host bend beyond the wire's range saturates.
        let body = note_expression_body(&expression(CLAP_NOTE_EXPRESSION_TUNING, 120.0), 0, 60)
            .expect("tuning decodes");
        assert!(matches!(
            body,
            EventBody::PerNotePitchBend {
                value: u32::MAX,
                ..
            }
        ));
    }
}

#[cfg(test)]
mod sounding_notes_tests {
    use super::{NoteAxis, SoundingNotes};

    #[test]
    fn wildcard_axis_parses_all_concrete_and_junk() {
        assert_eq!(NoteAxis::parse(-1, 16), NoteAxis::All); // wildcard
        assert_eq!(NoteAxis::parse(3, 16), NoteAxis::One(3)); // concrete
        assert_eq!(NoteAxis::parse(16, 16), NoteAxis::Invalid); // out of domain
        assert_eq!(NoteAxis::parse(-2, 128), NoteAxis::Invalid); // hostile
    }

    #[test]
    fn drain_matches_only_the_requested_axes() {
        let mut s = SoundingNotes::new(2);
        s.set(0, 0, 60);
        s.set(0, 3, 60);
        s.set(0, 3, 64);
        s.set(1, 3, 60); // other port: never touched below

        // Key wildcard, concrete channel: both channel-3 notes drain.
        let mut hits = Vec::new();
        s.drain_matching(0, Some(3), None, |ch, k| hits.push((ch, k)));
        assert_eq!(hits, [(3, 60), (3, 64)]);

        // Drained notes are cleared - a second wildcard finds nothing.
        hits.clear();
        s.drain_matching(0, Some(3), None, |ch, k| hits.push((ch, k)));
        assert!(hits.is_empty());

        // Channel wildcard, concrete key.
        s.set(0, 5, 60);
        hits.clear();
        s.drain_matching(0, None, Some(60), |ch, k| hits.push((ch, k)));
        assert_eq!(hits, [(0, 60), (5, 60)]);

        // Both wildcard: everything left on the port.
        s.set(0, 9, 1);
        hits.clear();
        s.drain_matching(0, None, None, |ch, k| hits.push((ch, k)));
        assert_eq!(hits, [(9, 1)]);

        // The other port's note survived it all.
        hits.clear();
        s.drain_matching(1, None, None, |ch, k| hits.push((ch, k)));
        assert_eq!(hits, [(3, 60)]);
    }

    #[test]
    fn concrete_off_clears_the_bit() {
        let mut s = SoundingNotes::new(1);
        s.set(0, 0, 60);
        s.clear(0, 0, 60);
        let mut hits = Vec::new();
        s.drain_matching(0, None, None, |ch, k| hits.push((ch, k)));
        assert!(hits.is_empty());
    }

    #[test]
    fn for_each_leaves_notes_sounding() {
        // Expression fan-out addresses voices without releasing them:
        // the same wildcard visit must keep finding the note.
        let mut s = SoundingNotes::new(1);
        s.set(0, 2, 60);
        s.set(0, 2, 64);
        for _ in 0..2 {
            let mut hits = Vec::new();
            s.for_each_matching(0, Some(2), None, |ch, k| hits.push((ch, k)));
            assert_eq!(hits, [(2, 60), (2, 64)]);
        }
    }
}

#[cfg(test)]
mod note_address_tests {
    use super::{clap_event_note, clap_note_address};

    fn note(channel: i16, key: i16) -> clap_event_note {
        // SAFETY: clap_event_note is a repr(C) POD; all-zero is valid.
        let mut ne: clap_event_note = unsafe { std::mem::zeroed() };
        ne.channel = channel;
        ne.key = key;
        ne
    }

    #[test]
    fn concrete_address_resolves() {
        assert_eq!(clap_note_address(&note(15, 127)), Some((15, 127)));
        assert_eq!(clap_note_address(&note(0, 0)), Some((0, 0)));
    }

    #[test]
    fn wildcard_and_out_of_range_are_dropped() {
        // `-1` is CLAP's "match all" wildcard; anything past the MIDI
        // domain is a hostile/buggy host. Neither may reach a plugin
        // that indexes tables by note/channel.
        assert_eq!(clap_note_address(&note(-1, 60)), None);
        assert_eq!(clap_note_address(&note(0, -1)), None);
        assert_eq!(clap_note_address(&note(16, 60)), None);
        assert_eq!(clap_note_address(&note(0, 128)), None);
    }
}
