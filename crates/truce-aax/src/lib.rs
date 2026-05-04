//! AAX format wrapper for truce.
//!
//! Exports C ABI functions that the pre-built AAX template binary
//! loads via dlopen. No AAX SDK dependency — the Rust side only
//! knows about the C bridge types defined in `truce_aax_bridge.h`.

// The `pub unsafe fn _*` block below is a single FFI surface whose
// shared safety contract is documented in the block-header comment
// preceding the functions. Per-function `# Safety` docs would be
// uniformly repetitive without adding information.
#![allow(clippy::missing_safety_doc)]

use std::ffi::{CString, c_void};
use std::os::raw::c_char;
use std::slice;
use std::sync::{Arc, OnceLock};

use truce_core::editor::{ClosureBridge, Editor, PluginContext, RawWindowHandle, SendPtr};
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_core::process::ProcessContext;
use truce_core::state;
use truce_params::{ParamFlags, Params};

// ---------------------------------------------------------------------------
// C ABI types (must match truce_aax_bridge.h)
// ---------------------------------------------------------------------------

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
    pub is_instrument: i32,
    pub category: u32,
    pub has_editor: i32,
    /// Param ID flagged as `IS_BYPASS`, or `u32::MAX` for "no bypass
    /// param". The AAX C++ template registers this param under the
    /// well-known `cDefaultMasterBypassID` so Pro Tools' master-bypass
    /// UI tracks the param value.
    pub bypass_param_id: u32,
}

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
pub const AAX_CAT_NONE: u32 = 0x00000000;
pub const AAX_CAT_EQ: u32 = 0x00000001;
pub const AAX_CAT_DYNAMICS: u32 = 0x00000002;
pub const AAX_CAT_PITCH_SHIFT: u32 = 0x00000004;
pub const AAX_CAT_REVERB: u32 = 0x00000008;
pub const AAX_CAT_DELAY: u32 = 0x00000010;
pub const AAX_CAT_MODULATION: u32 = 0x00000020;
pub const AAX_CAT_HARMONIC: u32 = 0x00000040;
pub const AAX_CAT_NOISE_REDUCTION: u32 = 0x00000080;
pub const AAX_CAT_DITHER: u32 = 0x00000100;
pub const AAX_CAT_SOUND_FIELD: u32 = 0x00000200;
pub const AAX_CAT_SW_GENERATORS: u32 = 0x00000800;
pub const AAX_CAT_EFFECT: u32 = 0x00002000;

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
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct TruceAaxMidiEvent {
    pub delta_frames: u32,
    pub status: u8,
    pub data1: u8,
    pub data2: u8,
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

struct AaxInstance<P: PluginExport> {
    plugin: P,
    event_list: EventList,
    output_events: EventList,
    plugin_id_hash: u64,
    sample_rate: f64,
    /// Max block size declared by AAX in `EffectInit` (delivered
    /// through `_reset`'s `max_frames`).
    max_block_size: usize,
    /// Reused per-block scratch for `RawBufferScratch::build`. Lives
    /// on the instance so the audio thread doesn't heap-allocate.
    scratch: truce_core::buffer::RawBufferScratch,
    editor: Option<Box<dyn Editor>>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    transport_slot: Arc<truce_core::TransportSlot>,
    /// Cached serialized state plus the `state_revision` value it was
    /// captured at. Pro Tools calls `GetChunkSize` + `GetChunk` as a
    /// pair, and for undo-checkpointing may call the pair repeatedly
    /// without any intervening state change. Caching avoids re-running
    /// `collect_values` + `serialize_state` on every call. The blob
    /// is `Arc`-wrapped so cache hits hand back a refcount bump
    /// instead of copying multi-KB Vec contents per call.
    state_cache: std::sync::Mutex<Option<(u64, std::sync::Arc<Vec<u8>>)>>,
    /// Monotonically-incrementing counter bumped by `_set_param` (audio
    /// thread) and `_load_state` (main thread). `_save_state` snapshots
    /// it before reading params and re-checks after serialization; if
    /// the counter advanced during the read the result isn't cached
    /// (it would be an inconsistent snapshot of the audio state). The
    /// previous `AtomicBool`-based dirty flag had a race where
    /// `swap(false)` could clear a bit that the audio thread had just
    /// re-set, leaving the cache one update behind.
    state_revision: std::sync::atomic::AtomicU64,
}

// ---------------------------------------------------------------------------
// Static descriptor + param info (populated once at register time)
// ---------------------------------------------------------------------------

struct StaticInfo {
    descriptor: TruceAaxDescriptor,
    params: Vec<StaticParamInfo>,
}

// Safety: the raw pointers in descriptors point to leaked CStrings
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

/// Install-time override for the host-facing plugin name shown in
/// Pro Tools' plug-in menus. Populated by `cargo truce install` via
/// the `aax_name` field in `truce.toml`.
const AAX_NAME_OVERRIDE: Option<&'static str> = option_env!("TRUCE_AAX_NAME_OVERRIDE");

fn resolved_plugin_name(info: &truce_core::info::PluginInfo) -> &'static str {
    truce_core::info::resolve_name_override(AAX_NAME_OVERRIDE, info.name)
}

pub fn register_aax<P: PluginExport>() {
    INFO.get_or_init(|| {
        let info = P::info();
        let layout = truce_core::wrapper::first_bus_layout::<P>();

        // Leak the name/vendor CStrings via `into_raw()` — they live for
        // the process lifetime and are owned by the static `INFO`.
        let name = CString::new(resolved_plugin_name(&info))
            .unwrap_or_default()
            .into_raw();
        let vendor = CString::new(info.vendor).unwrap_or_default().into_raw();

        let is_instrument = info.au_type == *b"aumu";
        let category = if info.category == PluginCategory::Instrument {
            AAX_CAT_SW_GENERATORS
        } else {
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
                _ => AAX_CAT_EQ, // default — EQ is always visible
            }
        };

        // AAX requires every plugin to have audio I/O, even pure
        // MIDI effects (NoteEffect) and output-only instruments.
        // Other wrappers (AU v2/v3, CLAP, VST3, LV2) accept
        // audio-less plugins natively — AAX is the outlier.
        // Synthesize dummy channels here so plugin authors can
        // declare truthful `bus_layouts: [BusLayout::new()]` for
        // MIDI effects without AAX-specific workarounds polluting
        // the plugin code.
        let (aax_inputs, aax_outputs) = match (
            layout.total_input_channels(),
            layout.total_output_channels(),
        ) {
            (0, 0) => (2, 2),              // pure MIDI effect → stereo passthrough
            (0, out) => (out.max(2), out), // output-only instrument → match output
            (in_, out) => (in_, out),
        };

        let descriptor = TruceAaxDescriptor {
            name,
            vendor,
            version: 1,
            num_inputs: aax_inputs,
            num_outputs: aax_outputs,
            num_params: 0, // filled below
            manufacturer_id: fourcc(&info.au_manufacturer),
            product_id: fourcc(&info.fourcc),
            // plugin_id must differ from product_id — XOR with a salt
            plugin_id: fourcc(&info.fourcc) ^ 0x01010101,
            is_instrument: i32::from(is_instrument),
            category,
            has_editor: 0,             // filled below
            bypass_param_id: u32::MAX, // filled below
        };

        // Static metadata path: derive emits a `LazyLock`-cached
        // `Vec<ParamInfo>`, and `has_editor_static` is a const-style
        // predicate plugins can override. Together they let the AAX
        // `Describe` block — which runs from C++ static init on some
        // hosts — skip plugin construction entirely. Plugins without
        // overrides fall back to the runtime path inside the
        // `PluginExport` defaults, matching the historical behavior.
        let param_infos = P::param_infos_static();
        let bypass_param_id = param_infos
            .iter()
            .find(|pi| pi.flags.contains(ParamFlags::IS_BYPASS))
            .map_or(u32::MAX, |pi| pi.id);
        let mut params = Vec::with_capacity(param_infos.len());
        for pi in &param_infos {
            let cs = truce_core::wrapper::ParamCStrings::from_info(pi);
            let info = TruceAaxParamInfo {
                id: pi.id,
                name: cs.name.as_ptr(),
                min: pi.range.min(),
                max: pi.range.max(),
                default_value: pi.default_plain,
                step_count: pi.range.step_count().map_or(0, std::num::NonZero::get),
                unit: cs.unit.as_ptr(),
            };
            params.push(StaticParamInfo {
                info,
                _name: cs.name,
                _unit: cs.unit,
            });
        }

        let has_editor = P::has_editor_static();
        let mut desc = descriptor;
        desc.num_params = truce_core::cast::len_u32(params.len());
        desc.has_editor = i32::from(has_editor);
        desc.bypass_param_id = bypass_param_id;

        StaticInfo {
            descriptor: desc,
            params,
        }
    });
}

fn fourcc(bytes: &[u8; 4]) -> i32 {
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

            // Force registration on library load
            #[used]
            #[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
            #[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
            #[cfg_attr(target_os = "windows", unsafe(link_section = ".CRT$XCU"))]
            static INIT: extern "C" fn() = {
                extern "C" fn init() {
                    ::truce_aax::register_aax::<$plugin_type>();
                }
                init
            };

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_get_descriptor(
                out: *mut ::truce_aax::TruceAaxDescriptor,
            ) {
                ::truce_aax::_get_descriptor(out);
            }
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn truce_aax_get_param_info(
                index: u32,
                out: *mut ::truce_aax::TruceAaxParamInfo,
            ) {
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
// pattern but is *paired* with `_destroy` reconstituting the Box —
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
// `&*` vs `&mut *` on the `ctx` cast below: the choice tracks what each
// callback actually mutates on the `AaxInstance`. Read-only or
// interior-mutability-only paths (`_get_param`, `_set_param` — which
// goes through atomics in `Params`, `_format_param`, `_save_state`)
// take `&*`; paths that write `inst.event_list` / `inst.sample_rate` /
// `inst.editor` take `&mut *`. The sequential-per-instance guarantee
// from the AAX SDK means a single mutable reference is always exclusive
// when we take one. Mirrors the pattern in `truce-au` and `truce-vst3`.
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
    let mut plugin = P::create();
    plugin.init();
    let info = P::info();
    let instance = Box::new(AaxInstance::<P> {
        plugin,
        event_list: EventList::new(),
        output_events: EventList::new(),
        plugin_id_hash: state::shared_plugin_state_hash(&info),
        sample_rate: 44100.0,
        max_block_size: 0,
        scratch: truce_core::buffer::RawBufferScratch::default(),
        editor: None,
        transport_slot: truce_core::TransportSlot::new(),
        state_cache: std::sync::Mutex::new(None),
        // Start at 1 so the first cached entry (revision 0) never
        // matches and we always serialize on the first save_state call.
        state_revision: std::sync::atomic::AtomicU64::new(1),
    });
    Box::into_raw(instance).cast::<std::ffi::c_void>()
}

pub unsafe fn _destroy<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    if !ctx.is_null() {
        unsafe { drop(Box::from_raw(ctx.cast::<AaxInstance<P>>())) };
    }
}

pub unsafe fn _reset<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_rate: f64,
    max_frames: u32,
) {
    let inst = unsafe { &mut *ctx.cast::<AaxInstance<P>>() };
    inst.sample_rate = sample_rate;
    inst.max_block_size = max_frames as usize;
    inst.plugin.reset(sample_rate, max_frames as usize);
    inst.plugin.params().set_sample_rate(sample_rate);
    inst.plugin.params().snap_smoothers();
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
    let inst = unsafe { &mut *ctx.cast::<AaxInstance<P>>() };
    let num_frames = num_frames as usize;

    // Convert MIDI
    inst.event_list.clear();
    if !events.is_null() && num_events > 0 {
        let ev_slice = unsafe { slice::from_raw_parts(events, num_events as usize) };
        for ev in ev_slice {
            let status = ev.status & 0xF0;
            let channel = ev.status & 0x0F;
            let body = match status {
                0x90 if ev.data2 > 0 => Some(EventBody::NoteOn {
                    channel,
                    note: ev.data1,
                    velocity: f32::from(ev.data2) / 127.0,
                }),
                0x90 => Some(EventBody::NoteOff {
                    channel,
                    note: ev.data1,
                    velocity: 0.0,
                }),
                0x80 => Some(EventBody::NoteOff {
                    channel,
                    note: ev.data1,
                    velocity: f32::from(ev.data2) / 127.0,
                }),
                0xA0 => Some(EventBody::Aftertouch {
                    channel,
                    note: ev.data1,
                    pressure: f32::from(ev.data2) / 127.0,
                }),
                0xB0 => Some(EventBody::ControlChange {
                    channel,
                    cc: ev.data1,
                    value: f32::from(ev.data2) / 127.0,
                }),
                0xE0 => {
                    let raw = (u16::from(ev.data2) << 7) | u16::from(ev.data1);
                    Some(EventBody::PitchBend {
                        channel,
                        value: (f32::from(raw) - 8192.0) / 8192.0,
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

    // Build AudioBuffer from raw pointers, reusing the per-instance scratch.
    debug_assert!(
        num_frames <= inst.max_block_size,
        "host violated AAX contract: render() got {num_frames} frames \
         but EffectInit declared max {}",
        inst.max_block_size
    );
    unsafe {
        let mut buffer = inst
            .scratch
            .build(inputs, outputs, num_in, num_out, num_frames as u32);
        let transport = if !transport_ptr.is_null() && (*transport_ptr).valid != 0 {
            let t = &*transport_ptr;
            TransportInfo {
                playing: t.playing != 0,
                recording: t.recording != 0,
                tempo: t.tempo,
                time_sig_num: t.time_sig_num.clamp(0, i32::from(u8::MAX)) as u8,
                time_sig_den: t.time_sig_den.clamp(0, i32::from(u8::MAX)) as u8,
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
            .process(&mut buffer, &inst.event_list, &mut context);
    }
}

/// Map a truce `Event` body to a 3-byte AAX-shaped MIDI packet. Returns
/// `None` for event types Pro Tools doesn't accept from plug-ins
/// (MIDI 2.0, `ParamChange`, Transport, etc.). The AAX SDK's
/// `AAX_IMIDINode::PostMIDIPacket` doc enumerates the supported set:
/// `NoteOn` / `NoteOff`, Pitch bend, Polyphonic key pressure, Program
/// change, Channel pressure, Bank-select-CC#0. Mirrors
/// `truce_vst2::try_encode_vst2_midi` so the two formats stay in sync.
fn try_encode_aax_midi(event: &Event) -> Option<TruceAaxMidiEvent> {
    let (status, data1, data2) = match &event.body {
        EventBody::NoteOn {
            channel,
            note,
            velocity,
        } => (
            0x90 | (channel & 0x0F),
            *note,
            truce_core::cast::midi_7bit(*velocity),
        ),
        EventBody::NoteOff {
            channel,
            note,
            velocity,
        } => (
            0x80 | (channel & 0x0F),
            *note,
            truce_core::cast::midi_7bit(*velocity),
        ),
        EventBody::Aftertouch {
            channel,
            note,
            pressure,
        } => (
            0xA0 | (channel & 0x0F),
            *note,
            truce_core::cast::midi_7bit(*pressure),
        ),
        EventBody::ControlChange { channel, cc, value } => (
            0xB0 | (channel & 0x0F),
            *cc,
            truce_core::cast::midi_7bit(*value),
        ),
        EventBody::ChannelPressure { channel, pressure } => (
            0xD0 | (channel & 0x0F),
            truce_core::cast::midi_7bit(*pressure),
            0,
        ),
        EventBody::PitchBend { channel, value } => {
            let n = truce_core::cast::midi_14bit_pb(*value);
            (
                0xE0 | (channel & 0x0F),
                (n & 0x7F) as u8,
                ((n >> 7) & 0x7F) as u8,
            )
        }
        EventBody::ProgramChange { channel, program } => (0xC0 | (channel & 0x0F), *program, 0),
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
    inst.output_events
        .iter()
        .filter(|e| try_encode_aax_midi(e).is_some())
        .count() as u32
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
    if let Some(packet) = inst
        .output_events
        .iter()
        .filter_map(try_encode_aax_midi)
        .nth(index as usize)
    {
        unsafe { *out = packet };
    }
}

pub unsafe fn _get_param<P: PluginExport>(ctx: *mut std::ffi::c_void, id: u32) -> f64 {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    inst.plugin.params().get_plain(id).unwrap_or(0.0)
}

pub unsafe fn _set_param<P: PluginExport>(ctx: *mut std::ffi::c_void, id: u32, value: f64) {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    inst.plugin.params().set_plain(id, value);
    // Bump the revision counter so the next `_save_state` notices the
    // change. `Release` synchronizes with the `Acquire` load in
    // `_save_state` — anyone seeing the bumped revision also sees the
    // param store.
    inst.state_revision
        .fetch_add(1, std::sync::atomic::Ordering::Release);
}

pub unsafe fn _format_param<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
    out: *mut c_char,
    out_len: u32,
) {
    // `out_len == 0` would underflow on `out_len as usize - 1` and
    // let `copy_nonoverlapping` write past the host-supplied buffer.
    if out_len == 0 || out.is_null() {
        return;
    }
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    if let Some(text) = inst.plugin.params().format_value(id, value) {
        let bytes = text.as_bytes();
        let len = bytes.len().min((out_len as usize) - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out, len);
            *out.add(len) = 0;
        }
    }
}

pub unsafe fn _save_state<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out_data: *mut *mut u8,
) -> u32 {
    let inst = unsafe { &*ctx.cast::<AaxInstance<P>>() };
    // Hot-path optimization for Pro Tools undo/snapshot flows,
    // which call the `GetChunkSize` + `GetChunk` pair repeatedly.
    // We use a seqlock-style protocol against the audio thread:
    //
    //   1. Snapshot `state_revision` *before* reading params.
    //   2. If the cache exists and was captured at this revision,
    //      hand back a clone — no audio update has happened since.
    //   3. Otherwise serialize the current param snapshot.
    //   4. Re-read `state_revision` *after* serialization. If it
    //      didn't advance, the serialized blob is consistent with
    //      `revision_before` and we cache it. If it did advance, an
    //      audio-thread `_set_param` ran during our read and the
    //      blob may not represent any single moment in time —
    //      return it (best-effort) but don't cache, so the next
    //      call re-serializes.
    //
    // The previous `AtomicBool::swap(false)` design had a window
    // where the audio thread could re-set the flag between the
    // swap and the read, then have its update overwritten when we
    // wrote the cache; this counter scheme detects that case.
    let revision_before = inst
        .state_revision
        .load(std::sync::atomic::Ordering::Acquire);

    let serialize_now = |inst: &AaxInstance<P>| -> Vec<u8> {
        let (ids, values) = inst.plugin.params().collect_values();
        let extra = inst.plugin.save_state();
        state::serialize_state(inst.plugin_id_hash, &ids, &values, extra.as_deref())
    };

    let blob: std::sync::Arc<Vec<u8>> = {
        // Recover from poisoning rather than bypassing the cache for
        // the rest of the plugin's lifetime. A panic anywhere on the
        // main thread (the only `_save_state` caller in Pro Tools)
        // would otherwise silently disable the seqlock-style cache
        // — the next save would re-serialize, the next after that
        // would too, and the audit-noted hot-path optimization would
        // be effectively gone. The cache content is just an
        // `Option<(u64, Arc<Vec<u8>>)>`, with no invariants a panic
        // could break, so `into_inner()` is sound.
        let mut guard = inst.state_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match guard.as_ref() {
            // `Arc::clone` on a hit — refcount bump, no Vec copy.
            Some((rev, blob)) if *rev == revision_before => std::sync::Arc::clone(blob),
            _ => {
                let fresh = std::sync::Arc::new(serialize_now(inst));
                let revision_after = inst
                    .state_revision
                    .load(std::sync::atomic::Ordering::Acquire);
                if revision_after == revision_before {
                    // No audio update during serialization — safe to cache.
                    *guard = Some((revision_before, std::sync::Arc::clone(&fresh)));
                }
                // else: audio updated mid-read; return the blob but
                // skip caching so the next call re-serializes.
                fresh
            }
        }
    };
    unsafe { finalize_blob(&blob, out_data) }
}

/// Hand a serialized state blob to the C caller as a raw pointer +
/// length. The blob is copied into a fresh boxed slice the C side will
/// later free with `_free_state` — taking `&[u8]` rather than `Vec<u8>`
/// lets callers hand us either a freshly-built `Vec` or a borrow into
/// an `Arc<Vec<u8>>` without an intermediate clone.
///
/// **Note on the `to_vec`:** the `Arc<Vec<u8>>` cache route still
/// pays a copy here because `_free_state` reconstitutes ownership via
/// `Vec::from_raw_parts`, which requires the Rust global allocator
/// **and** uniquely-owned bytes (no other Arc clones outstanding).
/// Pro Tools holds the buffer until it calls `_free_state`, but the
/// in-memory cache also keeps an `Arc` clone — there are at least 2
/// references at the moment of hand-off, so we can't `Arc::try_unwrap`.
/// A ref-counted hand-off (a small bridge type the C side would
/// decrement on free) would eliminate the copy entirely; today's
/// shape trades the extra `to_vec` allocation for keeping the C
/// boundary simple.
unsafe fn finalize_blob(blob: &[u8], out_data: *mut *mut u8) -> u32 {
    let len = truce_core::cast::len_u32(blob.len());
    let mut boxed = blob.to_vec().into_boxed_slice();
    let ptr = boxed.as_mut_ptr();
    std::mem::forget(boxed);
    unsafe { *out_data = ptr };
    len
}

pub unsafe fn _load_state<P: PluginExport>(ctx: *mut std::ffi::c_void, data: *const u8, len: u32) {
    let inst = unsafe { &mut *ctx.cast::<AaxInstance<P>>() };
    // `slice::from_raw_parts(null, n)` for `n > 0` is UB. Treat
    // `(null, *)` and `(_, 0)` the same as "host gave us nothing".
    if data.is_null() || len == 0 {
        return;
    }
    let blob = unsafe { slice::from_raw_parts(data, len as usize) };
    if let Some(deserialized) = state::deserialize_state(blob, inst.plugin_id_hash) {
        inst.plugin.params().restore_values(&deserialized.params);
        inst.plugin.params().snap_smoothers();
        if let Some(extra) = &deserialized.extra {
            inst.plugin.load_state(extra);
        }
        // State changed wholesale — bump the revision counter so the
        // next `_save_state` re-captures the restored values.
        inst.state_revision
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        // Drop the stale `Arc<Vec<u8>>` cached against the previous
        // revision now, instead of holding it until the next
        // `_save_state` call replaces it. Cosmetic: the revision-key
        // mismatch already forces a re-serialize on the next save,
        // but we'd otherwise pin a multi-KB blob across the gap.
        if let Ok(mut guard) = inst.state_cache.lock() {
            *guard = None;
        }
        if let Some(ref mut editor) = inst.editor {
            editor.state_changed();
        }
    }
}

// ---------------------------------------------------------------------------
// GUI bridge functions
// ---------------------------------------------------------------------------

pub unsafe fn _editor_create<P: PluginExport>(ctx: *mut c_void, out: *mut TruceAaxEditorInfo) {
    unsafe {
        let inst = &mut *ctx.cast::<AaxInstance<P>>();
        inst.editor = inst.plugin.editor();
        let info = match &inst.editor {
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
    }
}

pub unsafe fn _editor_open<P: PluginExport>(
    ctx: *mut c_void,
    parent_view: *mut c_void,
    platform: i32,
    callbacks: *const TruceAaxGuiCallbacks,
) {
    unsafe {
        // Defensive null checks — the AAX template is in-tree so the
        // contract is between matched halves, but every other format
        // wrapper guards parent + callback pointers (CLAP `:1455`,
        // VST3 `cb_gui_open`). Mismatched ABI between a stale shim
        // build and a fresh Rust build would otherwise fault inside
        // `&*callbacks`.
        if ctx.is_null() || callbacks.is_null() || parent_view.is_null() {
            return;
        }
        let inst = &mut *ctx.cast::<AaxInstance<P>>();
        let editor = match inst.editor.as_mut() {
            Some(e) => e,
            None => return,
        };

        let cb = &*callbacks;
        // Wrap raw pointers in SendPtr for Send+Sync
        let aax_ctx = SendPtr::new(cb.aax_ctx);
        let touch_fn = cb.touch_param;
        let set_fn = cb.set_param;
        let release_fn = cb.release_param;
        let resize_fn = cb.request_resize;
        let params = inst.plugin.params_arc();
        let plugin_ptr = SendPtr::new(&raw const inst.plugin);
        let params_for_set = params.clone();
        let params_for_get = params.clone();
        let params_for_plain = params.clone();
        let params_for_fmt = params.clone();
        let params_for_ctx = params.clone();
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
                get_meter: Box::new(move |id| {
                    let plugin = plugin_ptr.get();
                    plugin.get_meter(id)
                }),
                get_state: Box::new(move || {
                    let plugin = plugin_ptr.get();
                    plugin.save_state().unwrap_or_default()
                }),
                set_state: Box::new(move |data| {
                    let plugin = &mut *plugin_ptr.as_ptr().cast_mut();
                    plugin.load_state(&data);
                }),
                transport: Box::new(move || transport_slot.read()),
            },
            params_for_ctx,
        );

        let handle = match platform {
            1 => RawWindowHandle::AppKit(parent_view),
            3 => RawWindowHandle::Win32(parent_view),
            _ => return,
        };

        editor.open(handle, context);
    }
}

pub unsafe fn _editor_close<P: PluginExport>(ctx: *mut c_void) {
    unsafe {
        let inst = &mut *ctx.cast::<AaxInstance<P>>();
        if let Some(ref mut editor) = inst.editor {
            editor.close();
        }
    }
}

pub unsafe fn _editor_idle<P: PluginExport>(ctx: *mut c_void) {
    unsafe {
        let inst = &mut *ctx.cast::<AaxInstance<P>>();
        if let Some(ref mut editor) = inst.editor {
            editor.idle();
        }
    }
}

pub unsafe fn _editor_get_size<P: PluginExport>(ctx: *mut c_void, w: *mut u32, h: *mut u32) -> i32 {
    unsafe {
        let inst = &*ctx.cast::<AaxInstance<P>>();
        match &inst.editor {
            Some(editor) => {
                // Logical size. The patched baseview CGLayer path handles
                // HiDPI internally — same contract as CLAP / VST3 / AU.
                let (ew, eh) = editor.size();
                *w = ew;
                *h = eh;
                1
            }
            None => 0,
        }
    }
}

/// Free a state blob handed out by [`_save_state`].
///
/// **Contract:** `data` must point to memory allocated via the Rust
/// global allocator with `cap == len`. `_save_state` honors this by
/// going through `Vec::into_boxed_slice` (which trims capacity to len)
/// then `mem::forget`. Don't change either side to use `libc::malloc`
/// / `Vec::into_raw_parts` / a different cap-tracking strategy
/// without updating the other — `Vec::from_raw_parts` requires the
/// allocator and `cap` to match exactly. AAX never calls
/// `_free_state` with a non-Rust pointer today; the comment exists to
/// flag that drift if VST3's `libc_malloc` shape ever migrates here.
pub unsafe fn _free_state(data: *mut u8, len: u32) {
    if !data.is_null() && len > 0 {
        unsafe { drop(Vec::from_raw_parts(data, len as usize, len as usize)) };
    }
}

// Plugin → host MIDI is wired through `truce_aax_output_event_count`
// / `truce_aax_output_event_at` (defined as `_output_event_*` above).
// The C++ template's `RenderAudio` reads them after `truce_aax_process`
// and posts each packet via `AAX_IMIDINode::PostMIDIPacket` on the
// `LocalOutput` node it registered in its hand-built component
// descriptor — replacing the old `AAX_CMonolithicParameters::StaticDescribe`
// path, which only knew how to register `LocalInput` / `Global` /
// `Transport` nodes. See `cargo-truce/templates/aax/TruceAAX_Describe.cpp`
// for the descriptor build.
