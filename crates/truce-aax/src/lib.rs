//! AAX format wrapper for truce.
//!
//! Exports C ABI functions that the pre-built AAX template binary
//! loads via dlopen. No AAX SDK dependency - the Rust side only
//! knows about the C bridge types defined in `truce_aax_bridge.h`.

// The `pub unsafe fn _*` block below is a single FFI surface whose
// shared safety contract is documented in the block-header comment
// preceding the functions. Per-function `# Safety` docs would be
// uniformly repetitive without adding information.
#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, CString, c_void};
use std::mem;
use std::os::raw::c_char;
use std::slice;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use truce_core::TransportSlot;
use truce_core::buffer::RawBufferScratch;
use truce_core::bus::BusLayout;
use truce_core::cast::{len_u32, sample_pos_i64};
use truce_core::chunked_process::{ChunkedProcess, process_chunked};
use truce_core::config::{AudioConfig, ProcessMode};
use truce_core::editor::EditorBuilder;
use truce_core::editor::{ClosureBridge, Editor, PluginContext, RawWindowHandle, SendPtr};
use truce_core::events::{EVENT_LIST_PREALLOC, Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::{PluginCategory, PluginInfo, resolve_name_override};
use truce_core::meters::MeterStore;
use truce_core::midi::{decode_short_message, downconvert_to_midi1, pitch_bend_to_bytes};
use truce_core::plugin::PluginRuntime;
use truce_core::rt::{RtSection, audit};
use truce_core::snapshot::SnapshotSlot;
use truce_core::state;
use truce_core::tasks::AnyTaskSpawner;
use truce_core::wrapper::{
    ParamCStrings, PluginCell, SharedPlugin, copy_c_str, enter_plugin, first_bus_layout,
    log_midi_ports_clamped, log_missing_bus_layout, max_io_channels, run_audio_block,
    run_extern_callback_with, run_register, save_extra, shared_plugin,
};
use truce_params::{ParamFlags, ParamInfo, ParamRange, Params};

// ---------------------------------------------------------------------------
// C ABI types (must match truce_aax_bridge.h)
// ---------------------------------------------------------------------------

// The C ABI contract — header text and version constant — lives
// in the sibling `truce-aax-bridge` crate so `cargo-truce` can
// consume it without pulling in our runtime stack (`truce-core`,
// `truce-params`, `crossbeam-queue`). Re-exported here for source
// compatibility with anything that imported
// `truce_aax::TRUCE_AAX_ABI_VERSION` / `truce_aax::BRIDGE_HEADER`.
pub use truce_aax_bridge::{BRIDGE_HEADER, TRUCE_AAX_ABI_VERSION};

/// Wire values for [`TruceAaxParamInfo::range_type`]. The C++ shim
/// switches on these to pick the matching `AAX_ITaperDelegate` for
/// each registered parameter - without this, AAX defaults to a
/// linear normalize/denormalize and round-trips a log-ranged knob
/// through `RenderAudio` into a different plain value than the
/// editor wrote (knob fights the user mid-drag).
pub const TRUCE_AAX_RANGE_LINEAR: u8 = 0;
pub const TRUCE_AAX_RANGE_LOG: u8 = 1;
pub const TRUCE_AAX_RANGE_DISCRETE: u8 = 2;
/// Opaque taper: the C++ shim uses a custom `AAX_ITaperDelegate` that
/// routes normalize/denormalize back through `truce_aax_normalize` /
/// `truce_aax_denormalize` (this param's `ParamRange`), so AAX's
/// coefficient<->plain mapping matches truce's curve exactly - the only
/// faithful option for skew shapes AAX has no native taper for.
pub const TRUCE_AAX_RANGE_CUSTOM: u8 = 3;

/// The AAX taper hint for a range. Skewed shapes have no native AAX
/// taper, so they use `CUSTOM` - a linear taper (the old fallback) would
/// map the knob linearly while truce's editor maps it through the skew,
/// making the knob fight the user and recording wrong automation.
///
/// `Reversed` also uses `CUSTOM`: truce normalizes it as
/// `1 - inner.normalize`, which no native AAX taper mirrors, so routing
/// through truce's own normalize/denormalize is the only faithful mapping.
/// Classifying it by the inner shape would hand AAX an un-reversed taper -
/// the same knob-fights-editor / wrong-automation bug the skew fix closed.
/// Discreteness is preserved independently: the C++ shim's
/// `SetType`/`SetNumberOfSteps` key off the step count, not the taper, so a
/// `Reversed(Discrete/Enum)` stays a stepped param with a reversed taper.
fn aax_range_type(range: &ParamRange) -> u8 {
    match range {
        ParamRange::Linear { .. } => TRUCE_AAX_RANGE_LINEAR,
        // Skew shapes have no native AAX taper; a reversed range's
        // `1 - inner.normalize` has none either. Both route through the
        // custom taper (truce's own normalize/denormalize).
        ParamRange::Skewed { .. }
        | ParamRange::SymmetricalSkewed { .. }
        | ParamRange::Reversed(_) => TRUCE_AAX_RANGE_CUSTOM,
        ParamRange::Logarithmic { .. } => TRUCE_AAX_RANGE_LOG,
        ParamRange::Discrete { .. } | ParamRange::Enum { .. } => TRUCE_AAX_RANGE_DISCRETE,
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct TruceAaxDescriptor {
    pub name: *const c_char,
    pub vendor: *const c_char,
    pub version: u32,
    pub num_inputs: u32,
    pub num_outputs: u32,
    pub num_params: u32,
    pub manufacturer_id: i32,
    pub product_id: i32,
    pub plugin_id: i32,
    /// `1` if the plugin accepts MIDI input. Gates the `LocalInput`
    /// MIDI node registration in `Describe` and the MIDI-event
    /// collection in `RenderAudio`.
    pub wants_input_midi: i32,
    /// `1` if the plugin emits MIDI to the host. Gates the
    /// `LocalOutput` MIDI node registration in `Describe`.
    pub emits_midi: i32,
    pub category: u32,
    pub has_editor: i32,
    /// Param ID flagged as `IS_BYPASS`, or `u32::MAX` for "no bypass
    /// param". The AAX C++ template registers this param under the
    /// well-known `cDefaultMasterBypassID` so Pro Tools' master-bypass
    /// UI tracks the param value.
    pub bypass_param_id: u32,
    /// Chunk fourccs a pre-truce build stored its state under
    /// (`aax_chunk_ids` in `truce.toml`'s `[plugin.legacy_state]`).
    /// The template declares them alongside truce's own chunk so
    /// Pro Tools hands old sessions' chunks to `SetChunk`, which
    /// routes them to `truce_aax_load_state_foreign` and the
    /// plugin's `migrate_state`. Fixed capacity keeps the descriptor
    /// free of pointer lifetimes; only the first
    /// `num_legacy_chunk_ids` entries are valid.
    pub num_legacy_chunk_ids: u32,
    pub legacy_chunk_ids: [u32; MAX_LEGACY_CHUNK_IDS],
    /// Main-bus (in, out) channel counts of each `bus_layouts()` entry;
    /// the describe template registers one AAX stem-format config each.
    /// `null` / `num_layouts == 0` keeps the legacy mono/stereo describe.
    pub layout_in_channels: *const i16,
    pub layout_out_channels: *const i16,
    pub num_layouts: u32,
    /// Total sidechain (non-main) input width from the first layout. `> 0`
    /// makes the describe template register an AAX side-chain port; the
    /// render appends that (mono) channel after the main inputs.
    pub sidechain_in_channels: u32,
}

/// Capacity of [`TruceAaxDescriptor::legacy_chunk_ids`]; mirrors
/// `TRUCE_AAX_MAX_LEGACY_CHUNKS` in the bridge header.
pub const MAX_LEGACY_CHUNK_IDS: usize = 8;

#[repr(C)]
pub struct TruceAaxEditorInfo {
    pub has_editor: i32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
pub struct TruceAaxGuiCallbacks {
    pub aax_ctx: *mut c_void,
    pub touch_param: unsafe extern "C" fn(*mut c_void, u32),
    pub set_param: unsafe extern "C" fn(*mut c_void, u32, f64),
    pub release_param: unsafe extern "C" fn(*mut c_void, u32),
    pub request_resize: unsafe extern "C" fn(*mut c_void, u32, u32) -> i32,
}

// AAX plugin categories (matches AAX_Enums.h)
pub const AAX_CAT_EQ: u32 = 0x0000_0001;
pub const AAX_CAT_DYNAMICS: u32 = 0x0000_0002;
pub const AAX_CAT_PITCH_SHIFT: u32 = 0x0000_0004;
pub const AAX_CAT_REVERB: u32 = 0x0000_0008;
pub const AAX_CAT_DELAY: u32 = 0x0000_0010;
pub const AAX_CAT_MODULATION: u32 = 0x0000_0020;
pub const AAX_CAT_HARMONIC: u32 = 0x0000_0040;
pub const AAX_CAT_NOISE_REDUCTION: u32 = 0x0000_0080;
pub const AAX_CAT_DITHER: u32 = 0x0000_0100;
pub const AAX_CAT_SOUND_FIELD: u32 = 0x0000_0200;
pub const AAX_CAT_SW_GENERATORS: u32 = 0x0000_0800;
pub const AAX_CAT_EFFECT: u32 = 0x0000_2000;
pub const AAX_CAT_MIDI_EFFECT: u32 = 0x0001_0000;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct TruceAaxParamInfo {
    pub id: u32,
    pub name: *const c_char,
    pub min: f64,
    pub max: f64,
    pub default_value: f64,
    pub step_count: u32,
    pub unit: *const c_char,
    /// One of `TRUCE_AAX_RANGE_LINEAR` / `_LOG` / `_DISCRETE`. Drives
    /// taper-delegate selection in the C++ shim so AAX's normalized
    /// ↔ plain mapping matches what `ParamRange` does on the Rust
    /// side; mismatched tapers send the editor's writes back as a
    /// different plain value on the next render block.
    pub range_type: u8,
    /// Trailing pad keeps the struct's natural alignment stable
    /// across the C ABI; explicit pad makes the layout obvious to
    /// the matching C struct in `truce_aax_bridge.h`.
    #[allow(clippy::pub_underscore_fields)]
    pub _pad: [u8; 7],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct TruceAaxMidiEvent {
    pub delta_frames: u32,
    pub status: u8,
    pub data1: u8,
    pub data2: u8,
    // Trailing 1-byte pad keeping the struct's 8-byte alignment to
    // match `TruceAaxMidiEvent` in `truce_aax_bridge.h`.
    #[allow(clippy::pub_underscore_fields)]
    pub _pad: u8,
}

/// Transport snapshot filled by the AAX template's `RenderAudio` from
/// `AAX_ITransport` and passed to the Rust process callback.
///
/// Layout must match `TruceAaxTransportSnapshot` in `truce_aax_bridge.h`.
#[repr(C)]
pub struct TruceAaxTransportSnapshot {
    pub valid: i32,
    pub playing: i32,
    pub recording: i32,
    pub loop_active: i32,
    pub time_sig_num: i32,
    pub time_sig_den: i32,
    pub tempo: f64,
    pub position_samples: f64,
    pub position_beats: f64,
    pub bar_start_beats: f64,
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
}

// ---------------------------------------------------------------------------
// Instance wrapper
// ---------------------------------------------------------------------------

/// Bounded handoff slot for state loads. Capacity 1: presets don't
/// arrive faster than the audio thread completes a block, and on
/// overflow we want most-recent-wins (`force_push`) so a rapid
/// double-recall doesn't get the audio thread to apply a stale state
/// after the host already moved on.
type StateLoadQueue = crossbeam_queue::ArrayQueue<state::DeserializedState>;

struct AaxInstance<P: PluginExport> {
    /// The plugin in the wrapper-standard ownership cell: the audio
    /// thread owns it per block, host lifecycle callbacks own it while
    /// processing is stopped, and the two never overlap. `_save_state`
    /// and the editor's `get_state` read the lock-free snapshot instead,
    /// so they never touch it. See `truce_core::wrapper::SharedPlugin`.
    plugin: SharedPlugin<P>,
    /// Stable handle to the params Arc, set once at instance creation.
    /// Host-thread callbacks (`_get_param`, `_set_param`,
    /// `_format_value`, `_save_state`'s param walk) read params
    /// through this handle so a param query never touches the
    /// plugin. Params are atomic-backed and `Sync`.
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
    /// Atomic snapshots of the plugin's most recent `latency()` /
    /// `tail()`. Updated by the audio thread (or `_reset`).
    latency_cache: AtomicU32,
    tail_cache: AtomicU32,
    /// Current render mode as a [`ProcessMode`] discriminant. Pro Tools
    /// posts `EnteringOfflineMode` / `ExitingOfflineMode` notifications
    /// on offline bounce; the template forwards them via
    /// `_set_render_mode` (main thread) and `_reset` / `_process` read
    /// it (audio thread). Defaults to `Realtime` (0).
    render_mode: AtomicU8,
    /// Cached param-info table for the chunker's split predicate.
    param_infos: Vec<ParamInfo>,
    /// `min_subblock_samples` from `truce.toml`'s `[automation]`.
    min_subblock_samples: u32,
    plugin_id_hash: u64,
    /// Audio + lifecycle-owned per-block scratch. Behind a `PluginCell` so
    /// every callback reaches it through a shared `&AaxInstance` - never a
    /// whole-struct `&mut *ctx`, which would alias a concurrent host-thread
    /// `&*ctx` (param reads, GUI) and is UB under the aliasing model.
    /// `_render` on the audio thread and the lifecycle callbacks never
    /// overlap, so the cell is never held twice at once.
    audio: PluginCell<AaxAudio<P>>,
    /// Main/UI-thread-owned editor state, behind a `PluginCell` for the same
    /// reason. Its owners - the GUI callbacks and `_load_state`'s
    /// editor-notify - all run on the host main thread; `_render` never
    /// touches it.
    gui: PluginCell<AaxGui>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    transport_slot: Arc<TransportSlot>,
    /// Cached serialized state plus the `state_revision` value it was
    /// captured at. Pro Tools calls `GetChunkSize` + `GetChunk` as a
    /// pair, and for undo-checkpointing may call the pair repeatedly
    /// without any intervening state change. Caching avoids re-running
    /// `collect_values` + `serialize_state` on every call. The blob
    /// is `Arc`-wrapped so cache hits hand back a refcount bump
    /// instead of copying multi-KB Vec contents per call.
    state_cache: Mutex<Option<(u64, Arc<[u8]>)>>,
    /// Monotonically-incrementing counter bumped by `_set_param` (audio
    /// thread) and `_load_state` (main thread). `_save_state` snapshots
    /// it before reading params and re-checks after serialization; if
    /// the counter advanced during the read the result isn't cached
    /// (it would be an inconsistent snapshot of the audio state). A
    /// boolean dirty flag instead of a counter would let
    /// `swap(false)` clear a bit that the audio thread had just
    /// re-set, leaving the cache one update behind, so the counter
    /// is required for correctness.
    state_revision: AtomicU64,
    /// Bounded SPSC handoff for state loads. Host (`_load_state`)
    /// and editor (`set_state` callback) deserialize on their thread
    /// and push the result; the audio thread pops at the top of
    /// `_process` and calls [`state::apply_state`] under
    /// its exclusive `&mut plugin`.
    pending_state: Arc<StateLoadQueue>,
}

/// Audio + lifecycle-owned per-block scratch (see [`AaxInstance::audio`]).
struct AaxAudio<P: PluginExport> {
    event_list: EventList,
    /// Set when `_push_sysex_input` has queued `SysEx` for the current
    /// `_render` block. `RenderAudio` reassembles `SysEx` from the MIDI
    /// packet stream and pushes it before calling into Rust, so the
    /// render must not blindly clear `event_list` or it wipes the
    /// queued `SysEx`. The first push of a block clears + sets this;
    /// the render consumes it instead of re-clearing.
    sysex_inputs_pending: bool,
    output_events: EventList,
    /// Per-sub-block scratch for `chunked_process::process_chunked`.
    sub_event_scratch: EventList,
    sample_rate: f64,
    /// Max block size declared by AAX in `EffectInit` (delivered
    /// through `_reset`'s `max_frames`). A generous default keeps
    /// the contract assert in `_process` from tripping for hosts
    /// that send process before declaring a max.
    max_block_size: usize,
    /// `true` once `_reset` has run. `_process` early-returns and
    /// zeros outputs while false so DSP doesn't run with un-snapped
    /// smoothers / unset sample rate.
    prepared: bool,
    /// Reused per-block scratch for `RawBufferScratch::build`. Lives
    /// on the instance so the audio thread doesn't heap-allocate.
    ///
    /// Parameterised by `P::Sample`; widens/narrows host-`f32`
    /// buffers around `plugin.process()` for plugins on `prelude64`.
    scratch: RawBufferScratch<<P as PluginRuntime>::Sample>,
}

/// Main/UI-thread-owned editor state (see [`AaxInstance::gui`]).
struct AaxGui {
    editor: Option<Box<dyn Editor>>,
}

// ---------------------------------------------------------------------------
// Static descriptor + param info (populated once at register time)
// ---------------------------------------------------------------------------

struct StaticInfo {
    descriptor: TruceAaxDescriptor,
    params: Vec<StaticParamInfo>,
}

// SAFETY: the raw pointers in descriptors point to leaked CStrings
// that live for the process lifetime. They are read-only after init.
unsafe impl Send for StaticInfo {}
unsafe impl Sync for StaticInfo {}

struct StaticParamInfo {
    info: TruceAaxParamInfo,
    _name: CString,
    _unit: CString,
}

static INFO: OnceLock<StaticInfo> = OnceLock::new();

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Plugin display-name shown in Pro Tools' plug-in menus. Reads
/// `truce.toml`'s `aax_name` (baked into `PluginInfo` by
/// `truce::plugin_info!`), falling back to `PluginInfo::name`.
fn resolved_plugin_name(info: &PluginInfo) -> &'static str {
    resolve_name_override(info.aax_name, info.name)
}

/// Idempotent registration trigger called from the C ABI entry
/// points (`truce_aax_get_descriptor` / `truce_aax_get_param_info`).
///
/// Registration is deliberately *lazy* - it runs on the first host
/// query rather than from a library-load static initializer. On
/// Windows the AAX C++ shim `LoadLibraryA`s this Rust DLL from inside
/// `GetEffectDescriptions`, which Pro Tools calls during plug-in
/// scanning. A `.CRT$XCU` static initializer would therefore run
/// `register_aax` *under the Windows loader lock*, and that path
/// constructs the plugin (via `has_editor_static`'s default) and can
/// touch user32 (`GetDpiForSystem` in editor setup) - heavy work and
/// a Win32 call under the loader lock, which hangs the scan. Deferring
/// to the first `get_descriptor` call (made after `LoadLibraryA`
/// returns and the loader lock is released) keeps the loader-lock
/// window empty, mirroring how VST3/CLAP register lazily on their
/// first host entry point.
#[inline]
pub fn ensure_registered<P: PluginExport>() {
    if INFO.get().is_none() {
        register_aax::<P>();
    }
}

pub fn register_aax<P: PluginExport>() {
    // A panic crossing the C ABI boundary aborts the host process -
    // wrap the body so a plugin-author misconfiguration logs cleanly
    // and leaves INFO unset (host sees no plugin).
    run_register::<P>("AAX", || {
        let Some(layout) = first_bus_layout::<P>() else {
            log_missing_bus_layout::<P>("AAX");
            return;
        };
        register_aax_inner::<P>(&layout);
    });
}

fn register_aax_inner<P: PluginExport>(layout: &BusLayout) {
    INFO.get_or_init(|| {
        let info = P::info();

        // Leak the name/vendor CStrings via `into_raw()` - they live for
        // the process lifetime and are owned by the static `INFO`.
        let name = CString::new(resolved_plugin_name(&info))
            .unwrap_or_default()
            .into_raw();
        let vendor = CString::new(info.vendor).unwrap_or_default().into_raw();

        let is_note_effect = info.category == PluginCategory::NoteEffect;
        // Note effects need MIDI input *and* a category that lands them
        // in Pro Tools' MIDI plug-ins menu - without
        // `AAX_ePlugInCategory_MIDIEffect` they show up under audio
        // effects, and inserting one before an instrument routes only
        // the wrapper's stereo passthrough (no notes reach the synth).
        let category = if info.category == PluginCategory::Instrument {
            AAX_CAT_SW_GENERATORS
        } else if is_note_effect {
            AAX_CAT_MIDI_EFFECT
        } else {
            // Explicit `Some("EQ")` arm keeps the supported-strings table
            // complete next to Dynamics/Reverb/etc.; the wildcard default
            // also returns EQ (always-visible category) for unknown
            // strings. Both arms intentionally share the value.
            #[allow(clippy::match_same_arms)]
            match info.aax_category {
                Some("EQ") => AAX_CAT_EQ,
                Some("Dynamics") => AAX_CAT_DYNAMICS,
                Some("PitchShift") => AAX_CAT_PITCH_SHIFT,
                Some("Reverb") => AAX_CAT_REVERB,
                Some("Delay") => AAX_CAT_DELAY,
                Some("Modulation") => AAX_CAT_MODULATION,
                Some("Harmonic") => AAX_CAT_HARMONIC,
                Some("NoiseReduction") => AAX_CAT_NOISE_REDUCTION,
                Some("Dither") => AAX_CAT_DITHER,
                Some("SoundField") => AAX_CAT_SOUND_FIELD,
                Some("Effect") => AAX_CAT_EFFECT,
                _ => AAX_CAT_EQ, // default - EQ is always visible
            }
        };

        // AAX requires every plugin to have audio I/O, even pure
        // MIDI effects (NoteEffect) and output-only instruments.
        // Other wrappers (AU v2/v3, CLAP, VST3, LV2) accept
        // audio-less plugins natively - AAX is the outlier.
        // Synthesize dummy channels here so plugin authors can
        // declare truthful `bus_layouts: [BusLayout::new()]` for
        // MIDI effects without AAX-specific workarounds polluting
        // the plugin code.
        // AAX registers the main input stem; a sidechain is a separate
        // mono side-chain port, so the stem width is the main bus alone,
        // not the summed total. Non-sidechain plugins have main == total.
        let main_in_of = |l: &BusLayout| l.inputs.first().map_or(0, |b| b.channels.channel_count());
        let sidechain_in: u32 = layout
            .inputs
            .iter()
            .skip(1)
            .map(|b| b.channels.channel_count())
            .sum();
        let (aax_inputs, aax_outputs) = match (main_in_of(layout), layout.total_output_channels()) {
            (0, 0) => (2, 2),              // pure MIDI effect → stereo passthrough
            (0, out) => (out.max(2), out), // output-only instrument → match output
            (in_, out) => (in_, out),
        };

        // AAX carries a single MIDI stream per direction; clamp a
        // multi-port declaration to one and warn.
        log_midi_ports_clamped("AAX", "input", info.midi_input_ports);
        log_midi_ports_clamped("AAX", "output", info.midi_output_ports);

        // Legacy chunk fourccs, capped to the descriptor's fixed
        // capacity. `>4`-byte or short ids are skipped rather than
        // mangled (a wrong fourcc would silently never match).
        let mut legacy_chunk_ids = [0u32; MAX_LEGACY_CHUNK_IDS];
        let mut num_legacy_chunk_ids = 0u32;
        for id in info.legacy_aax_chunk_ids {
            if num_legacy_chunk_ids as usize >= MAX_LEGACY_CHUNK_IDS {
                eprintln!(
                    "[truce AAX] more than {MAX_LEGACY_CHUNK_IDS} legacy_state aax_chunk_ids; \
                     ignoring the rest"
                );
                break;
            }
            let Ok(bytes) = <[u8; 4]>::try_from(id.as_bytes()) else {
                eprintln!(
                    "[truce AAX] legacy_state chunk id {id:?} is not a 4-byte fourcc; skipped"
                );
                continue;
            };
            // Bit-for-bit reinterpretation: `fourcc` packs four bytes
            // into the AAX SDK's signed fourcc type; the descriptor
            // slot stores the same 32 bits unsigned.
            #[allow(clippy::cast_sign_loss)]
            {
                legacy_chunk_ids[num_legacy_chunk_ids as usize] = fourcc(bytes) as u32;
            }
            num_legacy_chunk_ids += 1;
        }

        // Supported (in, out) channel configs from `bus_layouts()`, one
        // AAX stem-format component per layout. Only when there's more
        // than one layout; a single-layout plugin keeps `num_layouts == 0`
        // and the legacy mono/stereo describe (which also covers the
        // audio-less synthesis in `default_io_channels`). The leaked
        // arrays live for the process, like the descriptor.
        let layouts = P::bus_layouts();
        let (layout_in_channels, layout_out_channels, num_layouts) = if layouts.len() > 1 {
            let ch = |c: u32| i16::try_from(c).unwrap_or(0);
            let ins: Vec<i16> = layouts.iter().map(|l| ch(main_in_of(l))).collect();
            let outs: Vec<i16> = layouts
                .iter()
                .map(|l| ch(l.total_output_channels()))
                .collect();
            let n = len_u32(ins.len());
            (
                Box::leak(ins.into_boxed_slice()).as_ptr(),
                Box::leak(outs.into_boxed_slice()).as_ptr(),
                n,
            )
        } else {
            (std::ptr::null(), std::ptr::null(), 0)
        };

        let descriptor = TruceAaxDescriptor {
            name,
            vendor,
            version: 1,
            num_inputs: aax_inputs,
            num_outputs: aax_outputs,
            num_params: 0, // filled below
            manufacturer_id: fourcc(info.au_manufacturer),
            product_id: fourcc(info.fourcc),
            // plugin_id must differ from product_id - XOR with a salt
            plugin_id: fourcc(info.fourcc) ^ 0x0101_0101,
            wants_input_midi: i32::from(info.accepts_midi_in),
            emits_midi: i32::from(info.emits_midi),
            category,
            has_editor: 0,             // filled below
            bypass_param_id: u32::MAX, // filled below
            num_legacy_chunk_ids,
            legacy_chunk_ids,
            layout_in_channels,
            layout_out_channels,
            num_layouts,
            sidechain_in_channels: sidechain_in,
        };

        // Static metadata path: derive emits a `LazyLock`-cached
        // `Vec<ParamInfo>`, and `has_editor_static` is a const-style
        // predicate plugins can override. Together they let the AAX
        // `Describe` block - which runs from C++ static init on some
        // hosts - skip plugin construction entirely. Plugins without
        // overrides fall back to the runtime path inside the
        // `PluginExport` defaults, matching the historical behavior.
        let param_infos = P::param_infos_static();
        let bypass_param_id = param_infos
            .iter()
            .find(|pi| pi.flags.contains(ParamFlags::IS_BYPASS))
            .map_or(u32::MAX, |pi| pi.id);
        let mut params = Vec::with_capacity(param_infos.len());
        for pi in &param_infos {
            let cs = ParamCStrings::from_info(pi);
            // Enum maps to the same shape as Discrete in AAX - both
            // get the linear taper with `SetNumberOfSteps`, which is
            // how AAX represents stepped automatable controls.
            let range_type = aax_range_type(&pi.range);
            let info = TruceAaxParamInfo {
                id: pi.id,
                name: cs.name.as_ptr(),
                min: pi.range.min(),
                max: pi.range.max(),
                default_value: pi.default_plain,
                step_count: pi.range.step_count().map_or(0, std::num::NonZero::get),
                unit: cs.unit.as_ptr(),
                range_type,
                _pad: [0; 7],
            };
            params.push(StaticParamInfo {
                info,
                _name: cs.name,
                _unit: cs.unit,
            });
        }

        let has_editor = P::has_editor_static();
        let mut desc = descriptor;
        desc.num_params = len_u32(params.len());
        desc.has_editor = i32::from(has_editor);
        desc.bypass_param_id = bypass_param_id;

        StaticInfo {
            descriptor: desc,
            params,
        }
    });
}

fn fourcc(bytes: [u8; 4]) -> i32 {
    (i32::from(bytes[0]) << 24)
        | (i32::from(bytes[1]) << 16)
        | (i32::from(bytes[2]) << 8)
        | i32::from(bytes[3])
}

// ---------------------------------------------------------------------------
// Export macro
// ---------------------------------------------------------------------------

/// Generates the C ABI entry points that the AAX template dlopen()s.
#[macro_export]
macro_rules! export_aax {
    ($plugin_type:ty) => {
        #[allow(non_snake_case)]
        mod _aax_entry {
            use super::*;

            // Registration is lazy: the first `get_descriptor` /
            // `get_param_info` query triggers it. This deliberately
            // avoids a library-load static initializer
            // (`.CRT$XCU` / `.init_array` / `__mod_init_func`) - on
            // Windows that would run plugin construction under the
            // loader lock during Pro Tools' scan and hang. See
            // `truce_aax::ensure_registered`.
            #[unsafe(no_mangle)]
            pub extern "C" fn truce_aax_abi_version() -> u32 {
                ::truce_aax::TRUCE_AAX_ABI_VERSION
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_get_descriptor(
                out: *mut ::truce_aax::TruceAaxDescriptor,
            ) {
                ::truce_aax::ensure_registered::<$plugin_type>();
                ::truce_aax::_get_descriptor(out);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_get_param_info(
                index: u32,
                out: *mut ::truce_aax::TruceAaxParamInfo,
            ) {
                ::truce_aax::ensure_registered::<$plugin_type>();
                ::truce_aax::_get_param_info(index, out);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_create() -> *mut ::std::ffi::c_void {
                ::truce_aax::_create::<$plugin_type>()
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_destroy(ctx: *mut ::std::ffi::c_void) {
                ::truce_aax::_destroy::<$plugin_type>(ctx);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_reset(
                ctx: *mut ::std::ffi::c_void,
                sample_rate: f64,
                max_frames: u32,
            ) {
                ::truce_aax::_reset::<$plugin_type>(ctx, sample_rate, max_frames);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_set_render_mode(
                ctx: *mut ::std::ffi::c_void,
                mode: u32,
            ) {
                ::truce_aax::_set_render_mode::<$plugin_type>(ctx, mode);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_latency(ctx: *mut ::std::ffi::c_void) -> u32 {
                ::truce_aax::_latency::<$plugin_type>(ctx)
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_process(
                ctx: *mut ::std::ffi::c_void,
                inputs: *const *const f32,
                outputs: *mut *mut f32,
                num_in: u32,
                num_out: u32,
                num_frames: u32,
                events: *const ::truce_aax::TruceAaxMidiEvent,
                num_events: u32,
                transport: *const ::truce_aax::TruceAaxTransportSnapshot,
            ) {
                ::truce_aax::_process::<$plugin_type>(
                    ctx, inputs, outputs, num_in, num_out, num_frames, events, num_events,
                    transport,
                );
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_output_event_count(
                ctx: *mut ::std::ffi::c_void,
            ) -> u32 {
                ::truce_aax::_output_event_count::<$plugin_type>(ctx)
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_output_event_at(
                ctx: *mut ::std::ffi::c_void,
                index: u32,
                out: *mut ::truce_aax::TruceAaxMidiEvent,
            ) {
                ::truce_aax::_output_event_at::<$plugin_type>(ctx, index, out);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_push_sysex_input(
                ctx: *mut ::std::ffi::c_void,
                delta_frames: u32,
                bytes: *const u8,
                len: u32,
            ) {
                ::truce_aax::_push_sysex_input::<$plugin_type>(ctx, delta_frames, bytes, len);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_output_sysex_count(
                ctx: *mut ::std::ffi::c_void,
            ) -> u32 {
                ::truce_aax::_output_sysex_count::<$plugin_type>(ctx)
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_output_sysex_at(
                ctx: *mut ::std::ffi::c_void,
                index: u32,
                out_delta_frames: *mut u32,
                out_bytes: *mut *const u8,
                out_len: *mut u32,
            ) {
                ::truce_aax::_output_sysex_at::<$plugin_type>(
                    ctx,
                    index,
                    out_delta_frames,
                    out_bytes,
                    out_len,
                );
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_get_param(
                ctx: *mut ::std::ffi::c_void,
                id: u32,
            ) -> f64 {
                ::truce_aax::_get_param::<$plugin_type>(ctx, id)
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_set_param(
                ctx: *mut ::std::ffi::c_void,
                id: u32,
                value: f64,
            ) {
                ::truce_aax::_set_param::<$plugin_type>(ctx, id, value);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_format_param(
                ctx: *mut ::std::ffi::c_void,
                id: u32,
                value: f64,
                out: *mut ::std::os::raw::c_char,
                out_len: u32,
            ) {
                ::truce_aax::_format_param::<$plugin_type>(ctx, id, value, out, out_len);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_parse_param(
                ctx: *mut ::std::ffi::c_void,
                id: u32,
                text: *const ::std::os::raw::c_char,
                out_plain: *mut f64,
            ) -> i32 {
                ::truce_aax::_parse_param::<$plugin_type>(ctx, id, text, out_plain)
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_normalize(
                ctx: *mut ::std::ffi::c_void,
                id: u32,
                plain: f64,
            ) -> f64 {
                ::truce_aax::_normalize::<$plugin_type>(ctx, id, plain)
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_denormalize(
                ctx: *mut ::std::ffi::c_void,
                id: u32,
                normalized: f64,
            ) -> f64 {
                ::truce_aax::_denormalize::<$plugin_type>(ctx, id, normalized)
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_save_state(
                ctx: *mut ::std::ffi::c_void,
                out_data: *mut *mut u8,
            ) -> u32 {
                ::truce_aax::_save_state::<$plugin_type>(ctx, out_data)
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_load_state(
                ctx: *mut ::std::ffi::c_void,
                data: *const u8,
                len: u32,
            ) {
                ::truce_aax::_load_state::<$plugin_type>(ctx, data, len);
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_load_state_foreign(
                ctx: *mut ::std::ffi::c_void,
                chunk_id: u32,
                data: *const u8,
                len: u32,
            ) -> i32 {
                ::truce_aax::_load_state_foreign::<$plugin_type>(ctx, chunk_id, data, len)
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_free_state(data: *mut u8, len: u32) {
                ::truce_aax::_free_state(data, len);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_editor_create(
                ctx: *mut ::std::ffi::c_void,
                out: *mut ::truce_aax::TruceAaxEditorInfo,
            ) {
                ::truce_aax::_editor_create::<$plugin_type>(ctx, out);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_editor_open(
                ctx: *mut ::std::ffi::c_void,
                parent_view: *mut ::std::ffi::c_void,
                platform: i32,
                callbacks: *const ::truce_aax::TruceAaxGuiCallbacks,
            ) {
                ::truce_aax::_editor_open::<$plugin_type>(ctx, parent_view, platform, callbacks);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_editor_close(ctx: *mut ::std::ffi::c_void) {
                ::truce_aax::_editor_close::<$plugin_type>(ctx);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_editor_idle(ctx: *mut ::std::ffi::c_void) {
                ::truce_aax::_editor_idle::<$plugin_type>(ctx);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_editor_get_size(
                ctx: *mut ::std::ffi::c_void,
                w: *mut u32,
                h: *mut u32,
            ) -> i32 {
                ::truce_aax::_editor_get_size::<$plugin_type>(ctx, w, h)
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Intentional leaks
//
// `CString::into_raw()` on plugin name + vendor (in `register_aax`)
// and the `std::mem::forget(boxed)` of the static `params: Vec<...>`
// hand `*const c_char` / `*const TruceAaxParamInfo` slices into the
// `TruceAaxDescriptor` that Pro Tools (via the AAX template's dlopen
// of this dylib) caches for the process lifetime. Pro Tools re-reads
// these pointers on demand (parameter editor, automation panel,
// preset save) with no callback signalling "you may free this now".
// Freeing is therefore unsound.
//
// The leak is bounded: O(plugin_count × (param_count + a few strings))
// per process, allocated once at `register_aax`. No leak per audio
// callback, per render, per editor open. The AAX dylib gets unloaded
// when Pro Tools exits, which reclaims the allocation.
//
// `Box::into_raw(boxed_instance)` in `_create` follows the same
// pattern but is *paired* with `_destroy` reconstituting the Box -
// so it isn't a leak, just a C-lifetime handoff.
//
// ---------------------------------------------------------------------------
// Implementation functions (called by the macro-generated exports)
//
// SAFETY for all pub unsafe fn below:
// - `ctx` is a *mut c_void created by Box::into_raw(Box::new(AaxInstance))
//   in _create(). Valid until _destroy() is called (exactly once per
//   create, guaranteed by the AAX SDK lifecycle).
// - The AAX template calls these functions via dlopen'd function
//   pointers. The template guarantees sequential access per instance
//   (RenderAudio is the only callback on the audio thread; all others
//   are on the main thread and serialized by Pro Tools).
// - Audio buffer pointers (inputs/outputs) are provided by Pro Tools
//   via AAX_SInstrumentRenderInfo and are valid for the declared
//   channel count × buffer size.
// - State pointers (out_data in save_state, data in load_state) are
//   managed by the AAX chunk system. The template handles allocation.
//
// Every callback below forms a shared `&*ctx` - never a whole-struct
// `&mut *ctx`. `_render` on the audio thread runs concurrently with
// host-thread callbacks (param reads, GUI, state), so a `&mut` to the
// whole instance would alias their `&*` and is UB under the aliasing
// model. Per-block scratch (`event_list`, `sample_rate`, ...) lives in
// the `audio` cell and editor state in the `gui` cell; both are reached
// through the shared `&*ctx`, and the AAX contract keeps their owners
// from overlapping. Params mutate through their own atomics.
// ---------------------------------------------------------------------------

pub unsafe fn _get_descriptor(out: *mut TruceAaxDescriptor) {
    if let Some(info) = INFO.get() {
        unsafe { *out = info.descriptor };
    }
}

pub unsafe fn _get_param_info(index: u32, out: *mut TruceAaxParamInfo) {
    if let Some(info) = INFO.get()
        && let Some(p) = info.params.get(index as usize)
    {
        unsafe { *out = p.info };
    }
}

#[must_use]
pub unsafe fn _create<P: PluginExport>() -> *mut std::ffi::c_void {
    // Author `create` / `init` run here; the extern "C" macro wrapper is
    // bare, so a panic would abort Pro Tools. A null return tells the host
    // construction failed.
    run_extern_callback_with::<P, *mut std::ffi::c_void>(
        "aax",
        "create",
        std::ptr::null_mut(),
        || {
            let mut plugin = P::create();
            plugin.init();
            let info = P::info();
            let param_infos = plugin.params().param_infos();
            let params_arc = plugin.params_arc();
            let meter_store = plugin.meter_store();
            let snapshot = plugin.snapshot_slot();
            let task_spawner = plugin.task_spawner();
            let editor_builder = plugin.editor_builder();
            let latency_cache = AtomicU32::new(plugin.latency());
            let tail_cache = AtomicU32::new(plugin.tail());
            let instance = Box::new(AaxInstance::<P> {
                plugin: shared_plugin(plugin),
                params_arc,
                snapshot,
                task_spawner,
                editor_builder,
                meter_store,
                latency_cache,
                tail_cache,
                render_mode: AtomicU8::new(ProcessMode::Realtime.as_u8()),
                param_infos,
                min_subblock_samples: info.automation.min_subblock_samples,
                plugin_id_hash: state::shared_plugin_state_hash(&info),
                audio: PluginCell::new(AaxAudio {
                    event_list: EventList::with_capacity(EVENT_LIST_PREALLOC),
                    sysex_inputs_pending: false,
                    output_events: EventList::with_capacity(EVENT_LIST_PREALLOC),
                    sub_event_scratch: EventList::with_capacity(EVENT_LIST_PREALLOC),
                    sample_rate: 44100.0,
                    max_block_size: 8192,
                    prepared: false,
                    scratch: RawBufferScratch::default(),
                }),
                gui: PluginCell::new(AaxGui { editor: None }),
                transport_slot: TransportSlot::new(),
                state_cache: Mutex::new(None),
                // Start at 1 so the first cached entry (revision 0) never
                // matches and we always serialize on the first save_state call.
                state_revision: AtomicU64::new(1),
                pending_state: Arc::new(StateLoadQueue::new(1)),
            });
            Box::into_raw(instance).cast::<std::ffi::c_void>()
        },
    )
}

pub unsafe fn _destroy<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    // Dropping the instance cascades into the editor's `Drop` (wgpu surface /
    // NSView / runloop-timer teardown). A panic there would abort Pro Tools -
    // the host where a GUI-teardown crash is most painful. Hosts destroy
    // instances routinely mid-session (removing a plugin, closing a session),
    // not just at quit, so swallow it; the instance is going away regardless.
    run_extern_callback_with::<P, ()>("aax", "destroy", (), || unsafe {
        if !ctx.is_null() {
            drop(Box::from_raw(ctx.cast::<AaxInstance<P>>()));
        }
    });
}

pub unsafe fn _reset<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_rate: f64,
    max_frames: u32,
) {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    let mut audio = inst.audio.enter();
    // Clamp host-supplied max_frames to a sane minimum.
    let max_frames = (max_frames as usize).max(1024);
    audio.sample_rate = sample_rate;
    audio.max_block_size = max_frames;
    // Size scratch to the widest declared layout: a multi-layout plugin
    // gets one AAX component per stem, and this instance may be any of
    // them, so pre-allocating for the max keeps `_process` off the audio
    // thread's allocator when the stem is wider than the first layout.
    let (num_in, num_out) = max_io_channels::<P>().unwrap_or((2, 2));
    audio
        .scratch
        .ensure_capacity(num_in as usize, num_out as usize, max_frames);
    // Offline-bounce state, set from the host's `EnteringOfflineMode` /
    // `ExitingOfflineMode` notifications (forwarded by the template).
    let mode = ProcessMode::from_u8(inst.render_mode.load(Ordering::Relaxed));
    {
        let mut plugin = enter_plugin(&inst.plugin);
        plugin.reset(&AudioConfig::new(sample_rate, max_frames).with_process_mode(mode));
        inst.latency_cache
            .store(plugin.latency(), Ordering::Relaxed);
        inst.tail_cache.store(plugin.tail(), Ordering::Relaxed);
    }
    audio.prepared = true;
}

/// Host entered / left offline-bounce mode. `mode` is a [`ProcessMode`]
/// discriminant; stash it so `_reset` (buffer / quality prep) and every
/// `_process` block pick it up. Unknown values fold to `Realtime`.
///
/// # Safety
/// `ctx` must be a live `AaxInstance<P>` pointer from [`_create`].
pub unsafe fn _set_render_mode<P: PluginExport>(ctx: *mut std::ffi::c_void, mode: u32) {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    let disc = u8::try_from(mode).unwrap_or(0);
    inst.render_mode
        .store(ProcessMode::from_u8(disc).as_u8(), Ordering::Relaxed);
}

/// Current plugin latency in samples, for host delay compensation.
/// Reads the audio-thread-updated cache, so the template's idle-thread
/// `TimerWakeup` poll never touches the plugin. The template
/// pushes changes to Pro Tools via `AAX_IController::SetSignalLatency`.
///
/// # Safety
/// `ctx` must be a live `AaxInstance<P>` pointer from [`_create`].
pub unsafe fn _latency<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    inst.latency_cache.load(Ordering::Relaxed)
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn _process<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    inputs: *const *const f32,
    outputs: *mut *mut f32,
    num_in: u32,
    num_out: u32,
    num_frames: u32,
    events: *const TruceAaxMidiEvent,
    num_events: u32,
    transport_ptr: *const TruceAaxTransportSnapshot,
) {
    let nf = num_frames as usize;
    let ok = run_audio_block::<P>("AAX", || {
        // Shared `&AaxInstance` (never a whole-struct `&mut`) - the audio
        // scratch is reached through its ownership cell, so a concurrent
        // host-thread `&*ctx` (param reads, GUI) can't alias us.
        let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
        let mut audio = inst.audio.enter();
        let num_frames = nf;

        // Host called render before EffectInit primed sample rate /
        // smoothers. Zero outputs and bail so DSP doesn't run with
        // uninitialized state.
        if !audio.prepared {
            for ch in 0..num_out as usize {
                let ptr = unsafe { *outputs.add(ch) };
                if !ptr.is_null() {
                    unsafe { std::ptr::write_bytes(ptr, 0, num_frames) };
                }
            }
            audio.event_list.clear();
            audio.sysex_inputs_pending = false;
            return;
        }

        // Take ownership of the plugin for the whole block: an
        // uncontended `Acquire`, never a wait, since the host contract
        // keeps `process` from overlapping a lifecycle callback and host
        // saves read the snapshot instead of the plugin. Enter through a
        // local Arc clone so the guard doesn't pin a borrow of `inst`.
        let plugin_arc = Arc::clone(&inst.plugin);
        let mut plugin = enter_plugin(&plugin_arc);

        // Apply any pending state-load before per-block work so the
        // plugin sees consistent params and extra state for the entire
        // block. See `pending_state` field comment for the queue-overflow
        // policy. Bumps `state_revision` so the next `_save_state` call
        // re-captures the restored values rather than handing back the
        // stale cache.
        if let Some(state) = inst.pending_state.pop() {
            state::apply_state(&mut *plugin, &state);
            inst.state_revision.fetch_add(1, Ordering::Release);
        }

        // Paranoid allocation check (the `rt-paranoid` feature): guard the
        // wrapper's per-block glue - event conversion, transport, process,
        // output encode, snapshot publish - as well as the plugin. Placed
        // after the state-load apply above, since `load_state` legitimately
        // allocates. No-op and zero-sized when the feature is off.
        let _rt = RtSection::enter();

        // One reborrow of the scratch so the disjoint fields below (event
        // list, sub-block scratch, output events, build scratch) can be
        // borrowed simultaneously - the guard's `Deref` can't split-borrow.
        let scr = &mut *audio;

        // Convert MIDI. `RenderAudio` reassembles SysEx from the MIDI
        // packet stream and pushes it via `_push_sysex_input` before
        // calling in here, so preserve any queued SysEx instead of
        // clearing it; otherwise clear the previous block's events
        // before appending short MIDI.
        if scr.sysex_inputs_pending {
            scr.sysex_inputs_pending = false;
        } else {
            scr.event_list.clear();
        }
        if !events.is_null() && num_events > 0 {
            let ev_slice = unsafe { slice::from_raw_parts(events, num_events as usize) };
            for ev in ev_slice {
                if let Some(body) = decode_short_message(ev.status, ev.data1, ev.data2) {
                    scr.event_list.push(Event {
                        sample_offset: ev.delta_frames,
                        port: 0,
                        body,
                    });
                }
            }
        }
        scr.event_list.ensure_sorted_by_offset();

        // Build AudioBuffer from raw pointers, reusing the per-instance scratch.
        debug_assert!(
            num_frames <= scr.max_block_size,
            "host violated AAX contract: render() got {num_frames} frames \
         but EffectInit declared max {}",
            scr.max_block_size
        );
        unsafe {
            let mut buffer = scr.scratch.build(
                inputs,
                outputs,
                num_in,
                num_out,
                len_u32(num_frames),
                P::supports_in_place(),
            );
            let transport = if !transport_ptr.is_null() && (*transport_ptr).valid != 0 {
                let t = &*transport_ptr;
                TransportInfo {
                    playing: t.playing != 0,
                    recording: t.recording != 0,
                    tempo: t.tempo,
                    // The two `as u8` casts are post-clamped to `0..=255`.
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    time_sig_num: t.time_sig_num.clamp(0, i32::from(u8::MAX)) as u8,
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    time_sig_den: t.time_sig_den.clamp(0, i32::from(u8::MAX)) as u8,
                    position_samples: sample_pos_i64(t.position_samples),
                    // Derived from samples for a consistent cross-format
                    // value (CLAP fills it directly), matching VST3/VST2/AU/
                    // LV2. Guard the pre-reset zero sample rate.
                    position_seconds: if scr.sample_rate > 0.0 {
                        t.position_samples / scr.sample_rate
                    } else {
                        0.0
                    },
                    position_beats: t.position_beats,
                    bar_start_beats: t.bar_start_beats,
                    loop_active: t.loop_active != 0,
                    loop_start_beats: t.loop_start_beats,
                    loop_end_beats: t.loop_end_beats,
                }
            } else {
                TransportInfo::default()
            };
            scr.output_events.clear();
            inst.transport_slot.write(&transport);

            let mut transport_snap = transport;
            let chunk_args = ChunkedProcess {
                events: &scr.event_list,
                sub_event_scratch: &mut scr.sub_event_scratch,
                transport: &mut transport_snap,
                sample_rate: scr.sample_rate,
                // Offline-bounce state from the host notifications; read
                // per block so a mid-session toggle applies immediately.
                process_mode: ProcessMode::from_u8(inst.render_mode.load(Ordering::Relaxed)),
                output_events: &mut scr.output_events,
                params_fn: None,
                meters_fn: None,
                param_infos: &inst.param_infos,
                min_subblock_samples: inst.min_subblock_samples,
            };
            process_chunked(
                &mut *plugin,
                inst.params_arc.as_ref() as &dyn Params,
                &mut buffer,
                chunk_args,
            );
            let _ = buffer;
            // Narrow rendered f64 output back to host f32 when needed.
            // No-op for `f32` plugins.
            scr.scratch
                .finish_widening(outputs, num_out, len_u32(num_frames));

            // Refresh latency / tail caches so the host's main-thread
            // queries don't have to touch the plugin.
            inst.latency_cache
                .store(plugin.latency(), Ordering::Relaxed);
            inst.tail_cache.store(plugin.tail(), Ordering::Relaxed);
        }
    });
    if !ok {
        unsafe {
            for ch in 0..num_out as usize {
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
/// shared `_process` body (null events / transport, small stereo
/// buffers), returning the steady-state audio-thread allocation count
/// (0 = clean). Vacuously 0 unless the `rt-paranoid` feature installs the
/// checking allocator. Not public API.
#[doc(hidden)]
#[must_use]
// The wrapper's process entry points are `_`-prefixed by AAX convention.
#[allow(clippy::used_underscore_items)]
pub fn rt_paranoid_smoke<P: PluginExport>() -> u32 {
    const FRAMES: u32 = 512;
    const CH: u32 = 2;
    let frames = FRAMES as usize;
    // SAFETY: constructs, drives, and destroys its own instance; all
    // pointers below outlive each `_process` call, buffers sized to
    // `FRAMES`, and the event / transport pointers are null (which
    // `_process` tolerates).
    unsafe {
        let ctx = _create::<P>();
        _reset::<P>(ctx, 48_000.0, FRAMES);

        let in_left = vec![0.5f32; frames];
        let in_right = vec![0.5f32; frames];
        let mut out_left = vec![0f32; frames];
        let mut out_right = vec![0f32; frames];
        let in_ptrs: [*const f32; 2] = [in_left.as_ptr(), in_right.as_ptr()];
        let mut out_ptrs: [*mut f32; 2] = [out_left.as_mut_ptr(), out_right.as_mut_ptr()];

        let mut count = 0;
        for _ in 0..3 {
            let ((), n) = audit(|| {
                _process::<P>(
                    ctx,
                    in_ptrs.as_ptr(),
                    out_ptrs.as_mut_ptr(),
                    CH,
                    CH,
                    FRAMES,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                );
            });
            count = n;
        }

        assert!(
            out_left.iter().any(|s| s.abs() > 0.0),
            "aax smoke: process did not run (output stayed zero)"
        );
        _destroy::<P>(ctx);
        count
    }
}

/// Map a truce `Event` body to a 3-byte AAX-shaped MIDI packet. Returns
/// `None` for event types Pro Tools doesn't accept through the
/// fixed-width MIDI channel-voice path (MIDI 2.0, `ParamChange`,
/// Transport, `SysEx`, etc.). The AAX SDK's
/// `AAX_IMIDINode::PostMIDIPacket` doc enumerates the supported set:
/// `NoteOn` / `NoteOff`, Pitch bend, Polyphonic key pressure, Program
/// change, Channel pressure, Bank-select-CC#0.
///
/// `SysEx` is **not** dropped here; it goes through a separate
/// multi-packet path the C++ template assembles / fragments
/// around `0xF0` ... `0xF7` framing
/// (see `_push_sysex_input`, `_output_sysex_count`,
/// `_output_sysex_at`). `try_encode_aax_midi` returning `None` for
/// `SysEx` is correct: the channel-voice slot can't carry it.
fn try_encode_aax_midi(event: &Event) -> Option<TruceAaxMidiEvent> {
    // AAX is MIDI 1.0 only; down-convert any 2.0 output first.
    let body = downconvert_to_midi1(&event.body).unwrap_or(event.body);
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
        EventBody::Aftertouch {
            channel,
            note,
            pressure,
            ..
        } => (0xA0 | (channel & 0x0F), *note, *pressure),
        EventBody::ControlChange {
            channel, cc, value, ..
        } => (0xB0 | (channel & 0x0F), *cc, *value),
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
    Some(TruceAaxMidiEvent {
        delta_frames: event.sample_offset,
        status,
        data1,
        data2,
        _pad: 0,
    })
}

/// Number of plugin-emitted MIDI events the C++ template can drain
/// from this block. The C++ side calls this immediately after
/// `truce_aax_process` and follows with `_at` for each index. The
/// per-call filter mirrors the iterator path in `_at` so the count and
/// the indexable view agree.
pub unsafe fn _output_event_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    let audio = inst.audio.enter();
    let n = audio
        .output_events
        .iter()
        .filter(|e| try_encode_aax_midi(e).is_some())
        .count();
    len_u32(n)
}

/// Read the i-th encodable output MIDI event into `out`. Indices are
/// stable within a single block (the queue isn't modified between
/// `process()` and the `_count` / `_at` drain).
pub unsafe fn _output_event_at<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out: *mut TruceAaxMidiEvent,
) {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    let audio = inst.audio.enter();
    if let Some(packet) = audio
        .output_events
        .iter()
        .filter_map(try_encode_aax_midi)
        .nth(index as usize)
    {
        unsafe { *out = packet };
    }
}

/// `SysEx` input - the AAX C++ template reassembles long messages
/// across consecutive `AAX_CMidiPacket` slots (per the SDK's
/// `0xF0` start / `0xF7` end framing) and calls this once per
/// complete logical message with the inner bytes. We copy into
/// the plug-in's `EventList` `SysEx` pool synchronously; pool-full
/// failures drop the message rather than corrupt-split it.
pub unsafe fn _push_sysex_input<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    delta_frames: u32,
    bytes: *const u8,
    len: u32,
) {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    if bytes.is_null() || len == 0 {
        return;
    }
    let mut audio = inst.audio.enter();
    let scr = &mut *audio;
    // First SysEx of the block clears the previous block's events and
    // flags `_render` to keep what we queue here rather than clearing
    // again.
    if !scr.sysex_inputs_pending {
        scr.event_list.clear();
        scr.sysex_inputs_pending = true;
    }
    let slice = unsafe { std::slice::from_raw_parts(bytes, len as usize) };
    let _ = scr.event_list.push_sysex(delta_frames, slice);
}

pub unsafe fn _output_sysex_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    let audio = inst.audio.enter();
    len_u32(
        audio
            .output_events
            .iter()
            .filter(|e| matches!(e.body, EventBody::SysEx { .. }))
            .count(),
    )
}

pub unsafe fn _output_sysex_at<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out_delta_frames: *mut u32,
    out_bytes: *mut *const u8,
    out_len: *mut u32,
) {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    let audio = inst.audio.enter();
    if let Some(event) = audio
        .output_events
        .iter()
        .filter(|e| matches!(e.body, EventBody::SysEx { .. }))
        .nth(index as usize)
    {
        let bytes = audio.output_events.sysex_bytes(&event.body);
        unsafe {
            *out_delta_frames = event.sample_offset;
            *out_bytes = bytes.as_ptr();
            *out_len = len_u32(bytes.len());
        }
    }
}

pub unsafe fn _get_param<P: PluginExport>(ctx: *mut std::ffi::c_void, id: u32) -> f64 {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    inst.params_arc.get_plain(id).unwrap_or(0.0)
}

pub unsafe fn _set_param<P: PluginExport>(ctx: *mut std::ffi::c_void, id: u32, value: f64) {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    inst.params_arc.set_plain(id, value);
    // Bump the revision counter so the next `_save_state` notices the
    // change. `Release` synchronizes with the `Acquire` load in
    // `_save_state` - anyone seeing the bumped revision also sees the
    // param store.
    inst.state_revision.fetch_add(1, Ordering::Release);
}

pub unsafe fn _format_param<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
    out: *mut c_char,
    out_len: u32,
) {
    // Author `format_value` can panic; firewall it (nothing written).
    run_extern_callback_with::<P, ()>("aax", "format_value", (), || unsafe {
        // `out_len == 0` would underflow on `out_len as usize - 1` and
        // let `copy_nonoverlapping` write past the host-supplied buffer.
        if out_len == 0 || out.is_null() {
            return;
        }
        let inst = &*ctx.cast::<AaxInstance<P>>();
        if let Some(text) = inst.params_arc.format_value(id, value) {
            let _ = copy_c_str(out, out_len as usize, &text);
        }
    });
}

/// Parse host text-entry (UTF-8) into a plain param value. Returns `1`
/// and writes `out_plain` on success, `0` when the text isn't parseable
/// (the shim's display delegate then leaves the value unchanged).
///
/// # Safety
/// `ctx` must be a live `AaxInstance<P>`; `text` a NUL-terminated C
/// string; `out_plain` a writable `f64`.
pub unsafe fn _parse_param<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    text: *const c_char,
    out_plain: *mut f64,
) -> i32 {
    // Author `parse_value` can panic; firewall it (0 = "not parsed").
    run_extern_callback_with::<P, i32>("aax", "parse_value", 0, || unsafe {
        if text.is_null() || out_plain.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<AaxInstance<P>>();
        let Ok(text) = CStr::from_ptr(text).to_str() else {
            return 0;
        };
        match inst.params_arc.parse_value(id, text) {
            Some(v) => {
                *out_plain = v;
                1
            }
            None => 0,
        }
    })
}

/// Normalize a plain value to `[0,1]` through the param's `ParamRange`
/// (the `CUSTOM` taper's `RealToNormalized`). Falls back to the plain
/// value for an unknown id. UI-rate, so the linear scan is fine.
///
/// # Safety
/// `ctx` must be a live `AaxInstance<P>`.
#[must_use]
pub unsafe fn _normalize<P: PluginExport>(ctx: *mut std::ffi::c_void, id: u32, plain: f64) -> f64 {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    inst.param_infos
        .iter()
        .find(|pi| pi.id == id)
        .map_or(plain, |pi| pi.range.normalize(plain))
}

/// Denormalize `[0,1]` to a plain value through the param's `ParamRange`
/// (the `CUSTOM` taper's `NormalizedToReal`). Falls back to the input
/// for an unknown id.
///
/// # Safety
/// `ctx` must be a live `AaxInstance<P>`.
#[must_use]
pub unsafe fn _denormalize<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    normalized: f64,
) -> f64 {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    inst.param_infos
        .iter()
        .find(|pi| pi.id == id)
        .map_or(normalized, |pi| pi.range.denormalize(normalized))
}

pub unsafe fn _save_state<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out_data: *mut *mut u8,
) -> u32 {
    // Pre-zero the out pointer so a panic anywhere in the body below
    // (caught via `run_extern_callback_with`) leaves the host seeing
    // an empty blob rather than a stale buffer pointer. The fallback
    // `0` returned on panic matches the `*out_data = null` state.
    unsafe {
        *out_data = std::ptr::null_mut();
    }
    run_extern_callback_with::<P, u32>("aax", "save_state", 0, || unsafe {
        save_state_body::<P>(ctx, out_data)
    })
}

unsafe fn save_state_body<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out_data: *mut *mut u8,
) -> u32 {
    // Allocator pin: AAX uses the Rust global allocator on both the
    // save (`finalize_blob` boxes a `Vec`) and free (`_free_state`
    // reconstitutes a `Vec` via `Vec::from_raw_parts`) paths. Mixing
    // the two sides through different allocators is UB.

    /// Cap on retries when the audio thread keeps bumping the
    /// revision mid-walk. A handful of attempts covers the common
    /// "user wiggling automation while Pro Tools snapshots" case;
    /// past that we hand back the most recent (possibly torn) blob
    /// rather than spinning indefinitely.
    const SNAPSHOT_RETRIES: u32 = 3;

    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    // Hot-path optimization for Pro Tools undo/snapshot flows,
    // which call the `GetChunkSize` + `GetChunk` pair repeatedly.
    // We use a seqlock-style protocol against the audio thread:
    //
    //   1. Snapshot `state_revision` *before* reading params.
    //   2. If the cache exists and was captured at this revision,
    //      hand back a clone - no audio update has happened since.
    //   3. Otherwise serialize the current param snapshot.
    //   4. Re-read `state_revision` *after* serialization. If it
    //      didn't advance, the serialized blob is consistent with
    //      `revision_before` and we cache it. If it did advance, an
    //      audio-thread `_set_param` ran during our read and the
    //      blob may not represent any single moment in time;
    //      return it (best-effort) but don't cache, so the next
    //      call re-serializes.
    let serialize_now = |inst: &AaxInstance<P>| -> Vec<u8> {
        let (ids, values) = inst.params_arc.collect_values();
        // Read the custom state from the lock-free snapshot the audio
        // thread publishes each block. Never touches the plugin, so it
        // can't stall a block in flight.
        let extra = save_extra(&inst.snapshot);
        let persist = inst.params_arc.serialize_persist();
        state::serialize_state(inst.plugin_id_hash, &ids, &values, &extra, &persist)
    };

    let blob: Arc<[u8]> = {
        // Recover from poisoning rather than bypassing the cache for
        // the rest of the plugin's lifetime. A panic anywhere on the
        // main thread (the only `_save_state` caller in Pro Tools)
        // would otherwise silently disable the seqlock-style cache
        // - the next save would re-serialize, the next after that
        // would too, and the hot-path optimization would be
        // effectively gone. The cache content is just an
        // `Option<(u64, Arc<[u8]>)>`, with no invariants a panic
        // could break, so `into_inner()` is sound.
        let mut guard = inst
            .state_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let revision_before = inst.state_revision.load(Ordering::Acquire);
        if let Some((rev, blob)) = guard.as_ref()
            && *rev == revision_before
        {
            Arc::clone(blob)
        } else {
            // Retry the param walk if the audio thread bumps the
            // revision mid-serialize. `collect_values` walks parameter
            // atomics individually with no whole-tree atomic snapshot,
            // so a `_set_param` that lands between two reads produces
            // a blob mixing pre- and post-update values for adjacent
            // params. Re-serialize until we get a consistent revision
            // (or exhaust the budget - see `SNAPSHOT_RETRIES`).
            let mut rev_start = revision_before;
            let mut fresh: Arc<[u8]> = Arc::from(serialize_now(inst));
            let mut consistent = false;
            for _ in 0..SNAPSHOT_RETRIES {
                let rev_end = inst.state_revision.load(Ordering::Acquire);
                if rev_end == rev_start {
                    consistent = true;
                    break;
                }
                rev_start = rev_end;
                fresh = Arc::from(serialize_now(inst));
            }
            if consistent {
                *guard = Some((rev_start, Arc::clone(&fresh)));
            }
            fresh
        }
    };
    unsafe { finalize_blob(&blob, out_data) }
}

/// Hand a serialized state blob to the C caller as a raw pointer +
/// length. The blob is copied into a fresh boxed slice the C side will
/// later free with `_free_state` - taking `&[u8]` rather than `Vec<u8>`
/// lets callers hand us either a freshly-built `Vec` or a borrow into
/// an `Arc<[u8]>` without an intermediate clone.
///
/// **Note on the `to_vec`:** the `Arc<[u8]>` cache route still
/// pays a copy here because `_free_state` reconstitutes ownership via
/// `Vec::from_raw_parts`, which requires the Rust global allocator
/// **and** uniquely-owned bytes (no other Arc clones outstanding).
/// Pro Tools holds the buffer until it calls `_free_state`, but the
/// in-memory cache also keeps an `Arc` clone - there are at least 2
/// references at the moment of hand-off, so we can't `Arc::try_unwrap`.
/// A ref-counted hand-off (a small bridge type the C side would
/// decrement on free) would eliminate the copy entirely; today's
/// shape trades the extra `to_vec` allocation for keeping the C
/// boundary simple.
unsafe fn finalize_blob(blob: &[u8], out_data: *mut *mut u8) -> u32 {
    let len = len_u32(blob.len());
    let mut boxed = blob.to_vec().into_boxed_slice();
    let ptr = boxed.as_mut_ptr();
    mem::forget(boxed);
    unsafe { *out_data = ptr };
    len
}

pub unsafe fn _load_state<P: PluginExport>(ctx: *mut std::ffi::c_void, data: *const u8, len: u32) {
    run_extern_callback_with::<P, ()>("aax", "load_state", (), || unsafe {
        let inst = &*ctx.cast::<AaxInstance<P>>();
        // `slice::from_raw_parts(null, n)` for `n > 0` is UB. Treat
        // `(null, *)` and `(_, 0)` the same as "host gave us nothing".
        if data.is_null() || len == 0 {
            return;
        }
        let blob = slice::from_raw_parts(data, len as usize);
        // Not this plugin's envelope? Offer the bytes to the plugin's
        // `migrate_state` hook (legacy sessions from a pre-truce
        // build); `None` fails the load.
        if let Some(deserialized) =
            state::parse_or_migrate::<P>(blob, inst.plugin_id_hash, state::PluginFormat::Aax, None)
        {
            // Apply params synchronously on the host thread (atomic-safe)
            // so host queries that read parameter values right after the
            // state load see the restored values without first running a
            // process block.
            state::apply_params(&*inst.params_arc, &deserialized);
            // Hand the deserialized state to the audio thread for
            // application. `force_push` overwrites any older pending blob
            // - see the `pending_state` field comment for why
            // newest-wins is the right policy. The audio thread's drain
            // bumps `state_revision`, so the cache invalidation is
            // covered there; we still drop the cached `Arc<[u8]>` here
            // so the multi-KB blob isn't pinned across the gap before
            // the next `_save_state` would replace it.
            let _ = inst.pending_state.force_push(deserialized);
            if let Ok(mut guard) = inst.state_cache.lock() {
                *guard = None;
            }
            if let Some(ref mut editor) = inst.gui.enter().editor {
                editor.state_changed();
            }
        }
    });
}

/// Bytes Pro Tools restored under a legacy chunk id (declared via
/// `[plugin.legacy_state]` `aax_chunk_ids`): offer them to the
/// plugin's `migrate_state` hook and ride the normal restore
/// pipeline on acceptance. Returns 1 when the plugin translated the
/// bytes, 0 when it didn't recognize them.
///
/// # Safety
/// Same `ctx` contract as `_load_state`; `data` must address `len`
/// readable bytes for the duration of the call.
pub unsafe fn _load_state_foreign<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    chunk_id: u32,
    data: *const u8,
    len: u32,
) -> i32 {
    run_extern_callback_with::<P, i32>("aax", "migrate_state", 0, || unsafe {
        let inst = &*ctx.cast::<AaxInstance<P>>();
        if data.is_null() || len == 0 {
            return 0;
        }
        let bytes = slice::from_raw_parts(data, len as usize);
        // Present the chunk id as its readable fourcc so
        // `migrate_state` implementations match on the same string
        // they declared in truce.toml.
        let id_bytes = chunk_id.to_be_bytes();
        let key = std::str::from_utf8(&id_bytes).unwrap_or("");
        let Some(migrated) = P::migrate_state(&state::ForeignState::Raw {
            format: state::PluginFormat::Aax,
            source_key: Some(key),
            bytes,
        }) else {
            return 0;
        };
        let deserialized: state::DeserializedState = migrated.into();
        state::apply_params(&*inst.params_arc, &deserialized);
        let _ = inst.pending_state.force_push(deserialized);
        if let Ok(mut guard) = inst.state_cache.lock() {
            *guard = None;
        }
        if let Some(ref mut editor) = inst.gui.enter().editor {
            editor.state_changed();
        }
        1
    })
}

// ---------------------------------------------------------------------------
// GUI bridge functions
// ---------------------------------------------------------------------------

pub unsafe fn _editor_create<P: PluginExport>(ctx: *mut c_void, out: *mut TruceAaxEditorInfo) {
    // The editor builder + `Editor::size` are author code; firewall them so a
    // panic can't unwind across the C ABI (leaving `*out` untouched, which
    // the shim zero-inits to "no editor").
    run_extern_callback_with::<P, ()>("aax", "editor_create", (), || unsafe {
        let inst = &*ctx.cast::<AaxInstance<P>>();
        let mut gui = inst.gui.enter();
        // Built from the lock-free param store the wrapper already
        // holds outside the plugin, so opening the GUI never
        // stalls the audio thread.
        gui.editor = (inst.editor_builder)(inst.params_arc.clone());
        let info = match &gui.editor {
            Some(editor) => {
                // Report logical size; the patched baseview CGLayer path
                // applies the host scale factor internally when it
                // configures the wgpu surface (same contract as CLAP /
                // VST3 / AU on macOS).
                let (w, h) = editor.size();
                TruceAaxEditorInfo {
                    has_editor: 1,
                    width: w,
                    height: h,
                }
            }
            None => TruceAaxEditorInfo {
                has_editor: 0,
                width: 0,
                height: 0,
            },
        };
        *out = info;
    });
}

pub unsafe fn _editor_open<P: PluginExport>(
    ctx: *mut c_void,
    parent_view: *mut c_void,
    platform: i32,
    callbacks: *const TruceAaxGuiCallbacks,
) {
    // `editor.open` runs author GUI-construction code that can panic;
    // firewall it so the panic can't unwind across the C ABI.
    run_extern_callback_with::<P, ()>("aax", "editor_open", (), || unsafe {
        // Defensive null checks - the AAX template is in-tree so the
        // contract is between matched halves, but every other format
        // wrapper guards parent + callback pointers (CLAP `:1455`,
        // VST3 `cb_gui_open`). Mismatched ABI between a stale shim
        // build and a fresh Rust build would otherwise fault inside
        // `&*callbacks`.
        if ctx.is_null() || callbacks.is_null() || parent_view.is_null() {
            return;
        }
        let inst = &*ctx.cast::<AaxInstance<P>>();
        let mut gui = inst.gui.enter();
        let Some(editor) = gui.editor.as_mut() else {
            return;
        };

        let cb = &*callbacks;
        // Wrap raw pointers in SendPtr for Send+Sync
        let aax_ctx = SendPtr::new(cb.aax_ctx);
        let touch_fn = cb.touch_param;
        let set_fn = cb.set_param;
        let release_fn = cb.release_param;
        let resize_fn = cb.request_resize;
        let params = Arc::clone(&inst.params_arc);
        let meter_store = Arc::clone(&inst.meter_store);
        let snapshot = Arc::clone(&inst.snapshot);
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
                    touch_fn(aax_ctx.as_ptr().cast_mut(), id);
                }),
                set_param: Box::new(move |id, value| {
                    let normalized = params_for_set.set_normalized_returning_normalized(id, value);
                    set_fn(aax_ctx.as_ptr().cast_mut(), id, normalized);
                }),
                end_edit: Box::new(move |id| {
                    release_fn(aax_ctx.as_ptr().cast_mut(), id);
                }),
                request_resize: Box::new(move |w, h| {
                    resize_fn(aax_ctx.as_ptr().cast_mut(), w, h) != 0
                }),
                get_param: Box::new(move |id| params_for_get.get_normalized(id).unwrap_or(0.0)),
                get_param_plain: Box::new(move |id| params_for_plain.get_plain(id).unwrap_or(0.0)),
                format_param: Box::new(move |id| {
                    let val = params_for_fmt.get_plain(id).unwrap_or(0.0);
                    params_for_fmt
                        .format_value(id, val)
                        .unwrap_or_else(|| format!("{val:.1}"))
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

        let handle = match platform {
            1 => RawWindowHandle::AppKit(parent_view),
            3 => RawWindowHandle::Win32(parent_view),
            _ => return,
        };

        editor.open(handle, context);
    });
}

pub unsafe fn _editor_close<P: PluginExport>(ctx: *mut c_void) {
    // `editor.close` runs author teardown code that can panic; firewall it.
    run_extern_callback_with::<P, ()>("aax", "editor_close", (), || unsafe {
        let inst = &*ctx.cast::<AaxInstance<P>>();
        if let Some(ref mut editor) = inst.gui.enter().editor {
            editor.close();
        }
    });
}

pub unsafe fn _editor_idle<P: PluginExport>(ctx: *mut c_void) {
    // `editor.idle` runs author code that can panic; firewall it.
    run_extern_callback_with::<P, ()>("aax", "editor_idle", (), || unsafe {
        let inst = &*ctx.cast::<AaxInstance<P>>();
        if let Some(ref mut editor) = inst.gui.enter().editor {
            editor.idle();
        }
    });
}

pub unsafe fn _editor_get_size<P: PluginExport>(ctx: *mut c_void, w: *mut u32, h: *mut u32) -> i32 {
    // `Editor::size` is author code; firewall it (0 = "size unavailable").
    run_extern_callback_with::<P, i32>("aax", "editor_get_size", 0, || unsafe {
        let inst = &*ctx.cast::<AaxInstance<P>>();
        match &inst.gui.enter().editor {
            Some(editor) => {
                // Logical size. The patched baseview CGLayer path handles
                // HiDPI internally - same contract as CLAP / VST3 / AU.
                let (ew, eh) = editor.size();
                *w = ew;
                *h = eh;
                1
            }
            None => 0,
        }
    })
}

/// Free a state blob handed out by [`_save_state`].
///
/// **Contract:** `data` must point to memory allocated via the Rust
/// global allocator with `cap == len`. `_save_state` honors this by
/// going through `Vec::into_boxed_slice` (which trims capacity to len)
/// then `mem::forget`. `Vec::from_raw_parts` requires the allocator
/// and `cap` to match exactly, so any change to the allocation
/// strategy on the save side must update this free side in lock-step.
pub unsafe fn _free_state(data: *mut u8, len: u32) {
    if !data.is_null() && len > 0 {
        // `finalize_blob` produced this pointer via
        // `Vec.to_vec().into_boxed_slice()` and `mem::forget`. A boxed
        // slice has `cap == len` by construction, so reconstructing
        // through `Vec::from_raw_parts(ptr, len, len)` on the same
        // global allocator is the symmetric free. Reconstructing a
        // `Box<[u8]>` instead would also work, but the existing
        // `Vec::from_raw_parts` shape matches what every other
        // wrapper crate uses.
        #[allow(clippy::same_length_and_capacity)]
        unsafe {
            drop(Vec::from_raw_parts(data, len as usize, len as usize));
        }
    }
}

// Plugin → host MIDI is wired through `truce_aax_output_event_count`
// / `truce_aax_output_event_at` (defined as `_output_event_*` above).
// The C++ template's `RenderAudio` reads them after `truce_aax_process`
// and posts each packet via `AAX_IMIDINode::PostMIDIPacket` on the
// `LocalOutput` node it registered in its hand-built component
// descriptor. `AAX_CMonolithicParameters::StaticDescribe` only knows
// how to register `LocalInput` / `Global` / `Transport` nodes, so the
// hand-built descriptor is what makes plugin → host MIDI possible.
// See `cargo-truce/templates/aax/TruceAAX_Describe.cpp` for the
// descriptor build.

// (The Rust-vs-`.h` ABI drift assertion lives in
// `truce-aax-bridge`, the crate that owns both the header text
// and the Rust constant. No need to duplicate it here.)

#[cfg(test)]
mod range_type_tests {
    use super::{
        TRUCE_AAX_RANGE_CUSTOM, TRUCE_AAX_RANGE_DISCRETE, TRUCE_AAX_RANGE_LINEAR,
        TRUCE_AAX_RANGE_LOG, aax_range_type,
    };
    use truce_params::ParamRange;

    static LINEAR: ParamRange = ParamRange::Linear { min: 0.0, max: 1.0 };
    static DISCRETE: ParamRange = ParamRange::Discrete { min: 0, max: 3 };
    static ENUM: ParamRange = ParamRange::Enum { count: 4 };

    /// A reversed range must route through the CUSTOM taper (truce's own
    /// `1 - inner.normalize`), for every inner shape - classifying it by
    /// the inner shape would hand AAX an un-reversed taper and the knob
    /// would fight the editor.
    #[test]
    fn reversed_always_uses_custom() {
        assert_eq!(
            aax_range_type(&ParamRange::Reversed(&LINEAR)),
            TRUCE_AAX_RANGE_CUSTOM
        );
        assert_eq!(
            aax_range_type(&ParamRange::Reversed(&DISCRETE)),
            TRUCE_AAX_RANGE_CUSTOM
        );
        assert_eq!(
            aax_range_type(&ParamRange::Reversed(&ENUM)),
            TRUCE_AAX_RANGE_CUSTOM
        );
    }

    /// Non-reversed shapes keep their native classification.
    #[test]
    fn non_reversed_classification_unchanged() {
        assert_eq!(
            aax_range_type(&ParamRange::Linear { min: 0.0, max: 1.0 }),
            TRUCE_AAX_RANGE_LINEAR
        );
        assert_eq!(
            aax_range_type(&ParamRange::Logarithmic {
                min: 20.0,
                max: 20_000.0
            }),
            TRUCE_AAX_RANGE_LOG
        );
        assert_eq!(
            aax_range_type(&ParamRange::Skewed {
                min: 0.0,
                max: 1.0,
                factor: 2.0
            }),
            TRUCE_AAX_RANGE_CUSTOM
        );
        assert_eq!(
            aax_range_type(&ParamRange::Discrete { min: 0, max: 3 }),
            TRUCE_AAX_RANGE_DISCRETE
        );
        assert_eq!(
            aax_range_type(&ParamRange::Enum { count: 4 }),
            TRUCE_AAX_RANGE_DISCRETE
        );
    }
}
