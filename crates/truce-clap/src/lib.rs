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
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_IS_LIVE, CLAP_EVENT_MIDI, CLAP_EVENT_MIDI_SYSEX,
    CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_GESTURE_BEGIN,
    CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_MOD, CLAP_EVENT_PARAM_VALUE,
    CLAP_EVENT_TRANSPORT, CLAP_TRANSPORT_HAS_BEATS_TIMELINE, CLAP_TRANSPORT_HAS_SECONDS_TIMELINE,
    CLAP_TRANSPORT_HAS_TEMPO, CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_LOOP_ACTIVE,
    CLAP_TRANSPORT_IS_PLAYING, CLAP_TRANSPORT_IS_RECORDING, clap_event_header, clap_event_midi,
    clap_event_midi_sysex, clap_event_note, clap_event_param_gesture, clap_event_param_value,
    clap_event_transport, clap_input_events, clap_output_events,
};
use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, CLAP_PORT_MONO, CLAP_PORT_STEREO,
    clap_audio_port_info, clap_plugin_audio_ports,
};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
use clap_sys::ext::note_ports::{
    CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP, clap_note_port_info, clap_plugin_note_ports,
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

use truce_core::Float;
use truce_core::buffer::AudioBuffer;
use truce_core::bus::ChannelConfig;
use truce_core::cast::{len_u32, size_of_u32};
use truce_core::chunked_process::{ChunkedProcess, process_chunked};
use truce_core::editor::{ClosureBridge, Editor, PluginContext, RawWindowHandle, SendPtr};
use truce_core::events::{EVENT_LIST_PREALLOC, Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::{PluginCategory, PluginInfo};
use truce_core::midi::{decode_short_message, denorm_7bit, pitch_bend_to_bytes};
use truce_core::plugin::PluginRuntime;
use truce_core::process::ProcessStatus;
use truce_core::state;
use truce_core::wrapper::run_audio_block_with;
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
    /// The user's plugin instance.
    plugin: P,
    /// Stable handle to the params Arc, set once at instance creation.
    /// Host-thread callbacks (`params_get_value`, `params_value_to_text`,
    /// `params_text_to_value`, `state_save`) read params through this
    /// handle so they never form a `&data.plugin` reference - the audio
    /// thread's `&mut data.plugin` would otherwise let LLVM deduce
    /// noalias on the plugin field and reorder loads past the audio
    /// thread's stores. Params are atomic-backed and `Sync`.
    params_arc: Arc<P::Params>,
    /// Atomic snapshots of the plugin's most recent `latency()` /
    /// `tail()`. Updated by the audio thread (or `init`/`reset`) so
    /// `latency_get` / `tail_get` read the value without touching
    /// `data.plugin`.
    latency_cache: AtomicU32,
    tail_cache: AtomicU32,
    /// Re-usable event list for converting CLAP events each process call.
    event_list: EventList,
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
    /// Flag: GUI changed params, need rescan on main thread.
    needs_rescan: Arc<AtomicBool>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    transport_slot: Arc<truce_core::TransportSlot>,
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
    /// Per-channel widening scratch. Empty when `P::Sample == f32`
    /// (slices point straight into host memory). When `P::Sample ==
    /// f64`, each channel's f32 host input is widened into the
    /// matching slot here and the slice in `input_slices` points
    /// there.
    input_widen: Vec<Vec<<P as PluginRuntime>::Sample>>,
    /// Per-channel narrowing scratch. Same shape: only used when
    /// `P::Sample == f64`, in which case the plugin writes here and
    /// the wrapper copies + casts back to the host's f32 output
    /// pointers after `process()` returns.
    output_narrow: Vec<Vec<<P as PluginRuntime>::Sample>>,
    /// Cached pointers to host output channels, captured at slice
    /// build time so the post-`process` narrow loop can copy back
    /// without re-walking the CLAP bus structures.
    host_out_ptrs: Vec<*mut f32>,
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
    truce_core::info::resolve_name_override(info.clap_name, info.name)
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
        data.plugin.init();
        data.param_infos = data.plugin.params().param_infos();
        // Query host params extension for request_flush support
        if !data.host.is_null()
            && let Some(get_ext) = (*data.host).get_extension
        {
            let ext = get_ext(data.host, CLAP_EXT_PARAMS.as_ptr());
            if !ext.is_null() {
                data.host_params = ext.cast::<clap_host_params>();
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
        data.plugin.reset(sample_rate, max_block);
        data.plugin.params().set_sample_rate(sample_rate);
        data.plugin.params().snap_smoothers();

        // Pre-grow the widening / narrowing scratch on the f64 path.
        // Without this, the first audio block after `activate` hits
        // the global allocator inside `clap_plugin_process` to grow
        // the outer Vec and each channel's inner Vec - a real RT
        // hazard on the first block post-reload. The outer-Vec
        // capacity is already reserved in `create_plugin`; what we
        // do here is push the inner per-channel `Vec<P::Sample>`s up
        // to `max_block_size` frames so the per-block `.clear() +
        // .reserve()` path is no-op-on-the-allocator.
        let same_precision = std::any::TypeId::of::<P::Sample>() == std::any::TypeId::of::<f32>();
        if !same_precision {
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
                data.host_out_ptrs.push(std::ptr::null_mut());
            }
        }

        true
    }
}

unsafe extern "C" fn clap_plugin_deactivate<P: PluginExport>(_plugin: *const clap_plugin) {
    // Nothing to do.
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
        data.plugin.reset(data.sample_rate, data.max_block_size);
        data.plugin.params().snap_smoothers();
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
    }
}

// ---------------------------------------------------------------------------
// Event conversion: CLAP input events -> EventList
// ---------------------------------------------------------------------------

/// Build a `TransportInfo` from a CLAP transport event/struct.
///
/// Same flag-driven decoding is needed in two places - the
/// `CLAP_EVENT_TRANSPORT` arm of `convert_input_events` (which sees a
/// `clap_event_transport` arriving as an input event mid-block) and
/// the per-process `clap_process::transport` field. Hosts deliver
/// transport state through whichever channel they prefer; the bit
/// layout is identical, so the decode is too.
//
// CLAP transport positions arrive as `i64` fixed-point counts that
// must be divided into `f64` seconds/beats; the `i64 as f64`
// narrowing is bounded in practice by song-length (well below 2^52).
#[allow(clippy::cast_precision_loss)]
fn build_transport_info(t: &clap_event_transport) -> TransportInfo {
    let flags = t.flags;
    let beats_timeline = flags & CLAP_TRANSPORT_HAS_BEATS_TIMELINE != 0;
    let has_time_sig = flags & CLAP_TRANSPORT_HAS_TIME_SIGNATURE != 0;
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
        // CLAP doesn't expose sample-position in transport - tracked
        // by the plugin's own block cursor when needed.
        position_samples: 0,
        position_seconds: if flags & CLAP_TRANSPORT_HAS_SECONDS_TIMELINE != 0 {
            t.song_pos_seconds as f64 / CLAP_SECTIME_FACTOR as f64
        } else {
            0.0
        },
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

            match (*header).type_ {
                CLAP_EVENT_NOTE_ON => {
                    let note_event = &*header.cast::<clap_event_note>();
                    // CLAP delivers `channel`/`key` as `i16` but the
                    // valid MIDI domain is `0..=15` / `0..=127`.
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let (channel, note) = (note_event.channel as u8, note_event.key as u8);
                    // CLAP's f64 velocity is a normalized [0, 1]; truce
                    // exposes it as a wire-native 7-bit value to match
                    // every other format. Plugins that want CLAP's full
                    // float precision can handle `NoteOn2` from
                    // `CLAP_EVENT_MIDI2` (when the host emits that path).
                    data.event_list.push(Event {
                        sample_offset,
                        body: EventBody::NoteOn {
                            group: 0,
                            channel,
                            note,
                            velocity: denorm_7bit(f32::from_f64(note_event.velocity)),
                        },
                    });
                }
                CLAP_EVENT_NOTE_OFF => {
                    let note_event = &*header.cast::<clap_event_note>();
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let (channel, note) = (note_event.channel as u8, note_event.key as u8);
                    data.event_list.push(Event {
                        sample_offset,
                        body: EventBody::NoteOff {
                            group: 0,
                            channel,
                            note,
                            velocity: denorm_7bit(f32::from_f64(note_event.velocity)),
                        },
                    });
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
                    data.event_list.push(Event {
                        sample_offset,
                        body: EventBody::ParamChange {
                            id: param_event.param_id,
                            value: param_event.value,
                        },
                    });
                }
                CLAP_EVENT_PARAM_MOD => {
                    // Same rationale as PARAM_VALUE above: drop
                    // pre-state-load mod packets.
                    if state_loaded {
                        continue;
                    }
                    let mod_event = &*header.cast::<clap_event_param_value>();
                    data.event_list.push(Event {
                        sample_offset,
                        body: EventBody::ParamMod {
                            id: mod_event.param_id,
                            note_id: mod_event.note_id,
                            value: mod_event.value,
                        },
                    });
                }
                CLAP_EVENT_TRANSPORT => {
                    let transport = &*header.cast::<clap_event_transport>();
                    data.event_list.push(Event {
                        sample_offset,
                        body: EventBody::Transport(build_transport_info(transport)),
                    });
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
                        data.event_list.push(Event {
                            sample_offset,
                            body,
                        });
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
                _ => {
                    // Unsupported event type (system real-time,
                    // MIDI 2.0) - skip silently. MIDI 2.0 demux is
                    // gated behind a per-plug-in version opt-in
                    // that's not wired yet; until then the channel
                    // voice 1.0 + `SysEx` paths above are the only
                    // ones surfaced.
                }
            }
        }

        if sort {
            data.event_list.sort();
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

        // Apply any state-load that the host or editor handed us
        // since the last block. Runs before per-block work so the
        // plugin sees consistent params for the entire block. The
        // single-slot queue means a rapid double-recall lands the
        // newest blob and the older one is dropped - preferred to
        // the audio thread chasing stale state across blocks.
        let state_loaded = data.pending_state.pop().is_some_and(|state| {
            state::apply_state(&mut data.plugin, &state);
            true
        });

        // Convert CLAP input events to our EventList - sort by
        // sample offset so the plugin sees them in time order.
        // `state_loaded` causes ParamValue/ParamMod events to be
        // dropped because they predate the state-load intent.
        convert_input_events::<P>(data, proc.in_events, true, state_loaded);

        // Build transport info from the CLAP transport event (or default).
        let transport = if proc.transport.is_null() {
            TransportInfo::default()
        } else {
            build_transport_info(&*proc.transport)
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
        // every bus. A null bus (`buf.data32 == null`) emits empty
        // slices for each of its declared channels rather than being
        // skipped - skipping would shift downstream buses' channel
        // indices and silently re-route audio onto the wrong bus for
        // multi-bus plugins.
        //
        // CLAP audio is always `f32` on the wire. If `P::Sample` is
        // also `f32`, slices point straight at host memory (zero-
        // copy). If `P::Sample` is `f64`, each channel's host input
        // is widened into per-channel scratch in `input_widen`, and
        // the matching `output_narrow` slot is what the plugin
        // writes into - we copy + narrow back to the host's f32
        // output pointers after `process()` returns. Compares
        // `TypeId` at runtime; the same-precision path stays a
        // single pointer cast.
        let same_precision = std::any::TypeId::of::<P::Sample>() == std::any::TypeId::of::<f32>();

        data.input_slices.clear();
        // Reset each inner scratch buffer's length to 0 (preserves
        // its heap allocation), don't `.clear()` the outer
        // `Vec<Vec<_>>` - that would drop every inner Vec and force
        // the per-channel `Vec::with_capacity` push below to
        // re-allocate every block, defeating the activate-time
        // pre-grow.
        for buf in &mut data.input_widen {
            buf.clear();
        }
        // The outer Vec is pre-sized in `clap_plugin_activate`; the
        // while-loop below only runs as a fallback if the pre-grow
        // didn't cover the bus layout the host actually picked.
        let mut flat_in_idx = 0usize;
        for bus_idx in 0..proc.audio_inputs_count {
            let buf = &*proc.audio_inputs.add(bus_idx as usize);
            for ch in 0..buf.channel_count {
                let host_ptr: *const f32 = if buf.data32.is_null() {
                    std::ptr::null()
                } else {
                    *buf.data32.add(ch as usize)
                };
                let slice: &[P::Sample] = if host_ptr.is_null() {
                    &[]
                } else if same_precision {
                    // SAFETY: runtime check above proved P::Sample == f32.
                    let raw = host_ptr.cast::<P::Sample>();
                    std::slice::from_raw_parts(raw, num_frames)
                } else {
                    while data.input_widen.len() <= flat_in_idx {
                        data.input_widen.push(Vec::with_capacity(num_frames));
                    }
                    let scratch = &mut data.input_widen[flat_in_idx];
                    scratch.clear();
                    scratch.reserve(num_frames);
                    let host = std::slice::from_raw_parts(host_ptr, num_frames);
                    for &h in host {
                        scratch.push(P::Sample::from_f32(h));
                    }
                    // SAFETY: `scratch` lives in `data` which outlives this block.
                    std::slice::from_raw_parts(scratch.as_ptr(), num_frames)
                };
                data.input_slices
                    .push(transmute::<&[P::Sample], &'static [P::Sample]>(slice));
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
            for ch in 0..buf.channel_count {
                let host_ptr: *mut f32 = if buf.data32.is_null() {
                    std::ptr::null_mut()
                } else {
                    *buf.data32.add(ch as usize)
                };
                data.host_out_ptrs.push(host_ptr);
                let slice: &mut [P::Sample] = if host_ptr.is_null() {
                    &mut []
                } else if same_precision {
                    let raw = host_ptr.cast::<P::Sample>();
                    std::slice::from_raw_parts_mut(raw, num_frames)
                } else {
                    while data.output_narrow.len() <= flat_out_idx {
                        data.output_narrow.push(Vec::with_capacity(num_frames));
                    }
                    let scratch = &mut data.output_narrow[flat_out_idx];
                    scratch.clear();
                    scratch.resize(num_frames, P::Sample::default());
                    std::slice::from_raw_parts_mut(scratch.as_mut_ptr(), num_frames)
                };
                data.output_slices
                    .push(transmute::<&mut [P::Sample], &'static mut [P::Sample]>(
                        slice,
                    ));
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
            output_events: &mut data.output_events,
            params_fn: None,
            meters_fn: None,
            param_infos: &data.param_infos,
            min_subblock_samples: data.info.automation.min_subblock_samples,
        };
        let status = process_chunked(
            &mut data.plugin,
            data.params_arc.as_ref() as &dyn Params,
            &mut audio_buffer,
            chunk_args,
        );

        // Narrow + copy back to host f32 outputs if the plugin ran
        // in f64. No-op when `P::Sample == f32`: the plugin wrote
        // directly into host memory and `output_narrow` is empty.
        // `zip` over the two slices instead of indexing - if either
        // vector is shorter (it shouldn't be, but a future drift
        // would hit this), iteration stops at the min cleanly
        // rather than panicking on an out-of-bounds index.
        if !same_precision {
            debug_assert_eq!(
                data.host_out_ptrs.len(),
                data.output_narrow.len(),
                "CLAP narrow-back: host_out_ptrs / output_narrow drifted out of lockstep",
            );
            for (host_ptr, plugin) in data.host_out_ptrs.iter().zip(data.output_narrow.iter()) {
                if host_ptr.is_null() {
                    continue;
                }
                let host = std::slice::from_raw_parts_mut(*host_ptr, num_frames);
                for (h, &p) in host.iter_mut().zip(plugin.iter()) {
                    *h = p.to_f32();
                }
            }
        }

        // Refresh latency / tail caches so the host's main-thread
        // queries don't have to call into `data.plugin`.
        data.latency_cache
            .store(data.plugin.latency(), Ordering::Relaxed);
        data.tail_cache.store(data.plugin.tail(), Ordering::Relaxed);

        // Flush GUI-initiated param changes to host output events
        flush_gui_changes::<P>(data, proc.out_events);

        // Forward plugin output events (MIDI output from instruments/effects)
        if !proc.out_events.is_null() && !data.output_events.is_empty() {
            let Some(try_push) = (*proc.out_events).try_push else {
                return CLAP_PROCESS_CONTINUE;
            };
            for event in data.output_events.iter() {
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
                            port_index: 0,
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
                            port_index: 0,
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
                            port_index: 0,
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
                            port_index: 0,
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
                            port_index: 0,
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
                            port_index: 0,
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
                            port_index: 0,
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
                            port_index: 0,
                            buffer: bytes.as_ptr(),
                            size: len_u32(bytes.len()),
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    // MIDI 2.0, ParamMod, Transport, and per-note
                    // events: the plugin-output direction isn't
                    // routinely emitted by truce plugins; leave them
                    // as silent skips rather than building partial
                    // encoders. MIDI 2.0 emission stays parked
                    // until a per-plug-in version opt-in lands.
                    _ => {}
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
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        let (ids, values) = data.params_arc.collect_values();
        // `plugin.save_state()` reads through the plugin reference: a
        // user impl that mutates non-atomic state from `process` while
        // also reading it from `save_state` races here. The contract
        // is "save_state must be safe to call concurrently with
        // process"; impls that copy from atomic params are fine.
        let extra = data.plugin.save_state();
        let blob = state::serialize_state(data.plugin_id_hash, &ids, &values, &extra);

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
    }
}

unsafe extern "C" fn state_load<P: PluginExport>(
    plugin: *const clap_plugin,
    stream: *const clap_istream,
) -> bool {
    unsafe {
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

        let Some(deserialized) = state::deserialize_state(&blob, data.plugin_id_hash) else {
            return false;
        };

        // Apply params synchronously on the host thread (atomic-safe)
        // so host queries that read parameter values right after
        // `clap_plugin_state.load` see the restored values without
        // first running a process block - clap-validator reads back
        // immediately after a load round-trip.
        state::apply_params(&*data.params_arc, &deserialized);

        // Hand the deserialized state to the audio thread for
        // application. `force_push` overwrites any older pending blob
        // - see the `pending_state` field comment for why "newest
        // wins" is the right policy here.
        let _ = data.pending_state.force_push(deserialized);

        if let Some(ref mut editor) = data.editor {
            editor.state_changed();
        }

        true
    }
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

        let Some(deserialized) =
            truce_core::presets::load_preset_file(std::path::Path::new(path), data.plugin_id_hash)
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

unsafe extern "C" fn audio_ports_count<P: PluginExport>(
    _plugin: *const clap_plugin,
    is_input: bool,
) -> u32 {
    let layouts = P::bus_layouts();
    let Some(layout) = layouts.first() else {
        return 0;
    };
    if is_input {
        len_u32(layout.inputs.len())
    } else {
        len_u32(layout.outputs.len())
    }
}

unsafe extern "C" fn audio_ports_get<P: PluginExport>(
    _plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_audio_port_info,
) -> bool {
    unsafe {
        let layouts = P::bus_layouts();
        let Some(layout) = layouts.first() else {
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
// Extension: note_ports (only for instruments)
// ---------------------------------------------------------------------------

unsafe extern "C" fn note_ports_count<P: PluginExport>(
    _plugin: *const clap_plugin,
    _is_input: bool,
) -> u32 {
    // All plugins declare 1 input + 1 output note port.
    // Effects that don't use MIDI simply ignore the events.
    1
}

unsafe extern "C" fn note_ports_get<P: PluginExport>(
    _plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_note_port_info,
) -> bool {
    unsafe {
        if index != 0 {
            return false;
        }

        let out = &mut *info;
        out.id = u32::from(!is_input);
        out.supported_dialects = CLAP_NOTE_DIALECT_CLAP;
        out.preferred_dialect = CLAP_NOTE_DIALECT_CLAP;
        out.name = [0; CLAP_NAME_SIZE];
        copy_str_to_buf(
            &mut out.name,
            if is_input {
                "Note Input"
            } else {
                "Note Output"
            },
        );

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
        data.editor = data.plugin.editor();
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
            let editor_ptr: *mut dyn truce_core::editor::Editor = editor.as_mut();
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

        let params = data.plugin.params_arc();
        // SAFETY: `data.plugin` is the `Box::into_raw` plugin instance owned
        // by the host's plugin slot - outlives the editor. Params fields are
        // atomic; cross-thread reads from the GUI thread are sound. The host
        // pointers are valid for the plugin's lifetime; closures capturing
        // them run on the main thread only.
        let plugin_ptr = SendPtr::new(&raw const data.plugin);
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
                        // Host expects physical points; editor speaks
                        // logical. Same scale-up the wrappers use for
                        // `gui_get_size`.
                        let (pw, ph) = scale_logical_to_physical(lw, lh, host_scale_for_resize);
                        let host = host_ptr.as_ptr();
                        if host.is_null() {
                            return false;
                        }
                        let Some(get_ext) = (*host).get_extension else {
                            return false;
                        };
                        let host_gui_ptr =
                            get_ext(host, CLAP_EXT_GUI.as_ptr()).cast::<clap_host_gui>();
                        if host_gui_ptr.is_null() {
                            return false;
                        }
                        let Some(req) = (*host_gui_ptr).request_resize else {
                            return false;
                        };
                        req(host, pw, ph)
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
            // Divide by the host-applied scale before handing to
            // the editor so `set_size` receives the same units
            // `size()` returns. Also clamp against the editor's
            // min/max/aspect - some hosts (Reaper) skip the
            // pre-flight `gui_adjust_size` and call `gui_set_size`
            // with raw drag dimensions, which leaves the editor
            // surface below `min_size` and clips content.
            let req = scale_physical_to_logical(width, height, data.host_scale);
            let (lw, lh) = clamp_logical_size(req.0, req.1, editor.as_ref());
            editor.set_size(lw, lh)
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
            let (lw, lh) = clamp_logical_size(req.0, req.1, editor.as_ref());
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

/// Apply `min_size` / `max_size` and the optional `aspect_ratio`
/// to a requested logical size.
///
/// With an aspect ratio set, the constraint has to know *which axis the
/// user dragged*. Deriving the other axis from a single fixed one (say
/// height-from-width) silently swallows a drag on the opposite edge:
/// dragging the vertical edge leaves the width unchanged, so the same
/// height is re-derived and the window doesn't move — only horizontal and
/// corner drags keep the aspect. Comparing the request against the
/// editor's current size (`editor.size()`, the size being resized *from*)
/// tells us which axis actually moved; that axis drives the other, so any
/// edge preserves the aspect. `u64` arithmetic for the multiplication so a
/// hypothetical `(u32::MAX, 1)` aspect doesn't overflow before the clamp
/// lands.
fn clamp_logical_size(w: u32, h: u32, editor: &dyn truce_core::editor::Editor) -> (u32, u32) {
    let (min_w, min_h) = editor.min_size();
    let (max_w, max_h) = editor.max_size();
    let mut w = w.clamp(min_w.max(1), max_w);
    let mut h = h.clamp(min_h.max(1), max_h);
    if let Some((num, denom)) = editor.aspect_ratio()
        && num > 0
        && denom > 0
    {
        let num64 = u64::from(num);
        let denom64 = u64::from(denom);
        // The axis that moved furthest from the current size is the one the
        // user dragged; derive the other from it. A tie / no movement (e.g.
        // the host echoing a size back) falls to the width branch.
        let (cur_w, cur_h) = editor.size();
        if h.abs_diff(cur_h) > w.abs_diff(cur_w) {
            // Vertical edge dragged: width follows the height, then re-derive
            // height from the bounds-clamped width to stay on-ratio.
            let w_implied = (u64::from(h) * num64 / denom64).clamp(1, u64::from(u32::MAX));
            #[allow(clippy::cast_possible_truncation)]
            {
                w = (w_implied as u32).clamp(min_w.max(1), max_w);
            }
            let h_final = (u64::from(w) * denom64 / num64).clamp(1, u64::from(u32::MAX));
            #[allow(clippy::cast_possible_truncation)]
            {
                h = (h_final as u32).clamp(min_h.max(1), max_h);
            }
        } else {
            // Horizontal edge dragged (or a tie): height follows the width,
            // then re-derive width from the clamped height to stay on-ratio.
            let h_implied = (u64::from(w) * denom64 / num64).clamp(1, u64::from(u32::MAX));
            #[allow(clippy::cast_possible_truncation)]
            {
                h = (h_implied as u32).clamp(min_h.max(1), max_h);
            }
            let w_final = (u64::from(h) * num64 / denom64).clamp(1, u64::from(u32::MAX));
            #[allow(clippy::cast_possible_truncation)]
            {
                w = (w_final as u32).clamp(min_w.max(1), max_w);
            }
        }
    }
    (w, h)
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
    note_ports: clap_plugin_note_ports,
    gui: clap_plugin_gui,
    latency: clap_plugin_latency,
    tail: clap_plugin_tail,
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

impl<P: PluginExport> Extensions<P> {
    fn new() -> Self {
        Self {
            params: make_params_extension::<P>(),
            state: make_state_extension::<P>(),
            preset_load: make_preset_load_extension::<P>(),
            audio_ports: make_audio_ports_extension::<P>(),
            note_ports: make_note_ports_extension::<P>(),
            gui: make_gui_extension::<P>(),
            latency: clap_plugin_latency {
                get: Some(latency_get::<P>),
            },
            tail: clap_plugin_tail {
                get: Some(tail_get::<P>),
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
        plugin: instance,
        params_arc,
        latency_cache,
        tail_cache,
        event_list: EventList::with_capacity(EVENT_LIST_PREALLOC),
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
        gui_changes: Arc::new(GuiChangeQueue::new(GUI_QUEUE_CAPACITY)),
        pending_state: Arc::new(StateLoadQueue::new(1)),
        needs_rescan: Arc::new(AtomicBool::new(false)),
        transport_slot: truce_core::TransportSlot::new(),
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
mod tests {
    use super::*;
    use truce_core::editor::{Editor, PluginContext, RawWindowHandle};

    /// Minimal editor stub: only the bounds/aspect/size hooks
    /// `clamp_logical_size` reads carry meaning; the rest are unused.
    struct StubEditor {
        size: (u32, u32),
        min: (u32, u32),
        max: (u32, u32),
        aspect: Option<(u32, u32)>,
    }

    impl Editor for StubEditor {
        fn size(&self) -> (u32, u32) {
            self.size
        }
        fn open(&mut self, _parent: RawWindowHandle, _context: PluginContext) {}
        fn close(&mut self) {}
        fn min_size(&self) -> (u32, u32) {
            self.min
        }
        fn max_size(&self) -> (u32, u32) {
            self.max
        }
        fn aspect_ratio(&self) -> Option<(u32, u32)> {
            self.aspect
        }
    }

    fn stub(size: (u32, u32), aspect: Option<(u32, u32)>) -> StubEditor {
        StubEditor {
            size,
            min: (320, 240),
            max: (u32::MAX, u32::MAX),
            aspect,
        }
    }

    #[test]
    fn no_aspect_clamps_each_axis_to_bounds() {
        let e = stub((640, 480), None);
        assert_eq!(clamp_logical_size(800, 600, &e), (800, 600));
        assert_eq!(clamp_logical_size(100, 100, &e), (320, 240));
    }

    #[test]
    fn vertical_edge_drag_derives_width_from_height() {
        // Resizing from 640x480, the user dragged only the height to 600. The
        // old height-from-width rule swallowed this (returned 480); the
        // dragged-axis rule grows the width to keep 4:3.
        let e = stub((640, 480), Some((4, 3)));
        assert_eq!(clamp_logical_size(640, 600, &e), (800, 600));
    }

    #[test]
    fn horizontal_edge_drag_derives_height_from_width() {
        let e = stub((640, 480), Some((4, 3)));
        assert_eq!(clamp_logical_size(800, 480, &e), (800, 600));
    }

    #[test]
    fn corner_drag_follows_the_larger_delta() {
        // width +160 vs height +120: width dominates, height follows it.
        let e = stub((640, 480), Some((4, 3)));
        assert_eq!(clamp_logical_size(800, 600, &e), (800, 600));
    }

    #[test]
    fn result_stays_on_ratio_and_within_bounds() {
        let e = stub((640, 480), Some((16, 9)));
        let (w, h) = clamp_logical_size(640, 800, &e);
        assert!((i64::from(w) * 9 - i64::from(h) * 16).abs() <= 16);
        assert!(w >= 320 && h >= 240);
    }
}
