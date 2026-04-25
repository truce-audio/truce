//! AAX format wrapper for truce.
//!
//! Exports C ABI functions that the pre-built AAX template binary
//! loads via dlopen. No AAX SDK dependency — the Rust side only
//! knows about the C bridge types defined in truce_aax_bridge.h.

// The `pub unsafe fn _*` block below is a single FFI surface whose
// shared safety contract is documented in the block-header comment
// preceding the functions. Per-function `# Safety` docs would be
// uniformly repetitive without adding information.
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_void, CString};
use std::os::raw::c_char;
use std::slice;
use std::sync::{Arc, OnceLock};

use truce_core::editor::{Editor, EditorContext, RawWindowHandle, SendPtr};
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_core::process::ProcessContext;
use truce_core::state;
use truce_params::Params;

// ---------------------------------------------------------------------------
// C ABI types (must match truce_aax_bridge.h)
// ---------------------------------------------------------------------------

#[repr(C)]
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
pub struct TruceAaxMidiEvent {
    pub delta_frames: u32,
    pub status: u8,
    pub data1: u8,
    pub data2: u8,
    pub _pad: u8,
}

/// Transport snapshot filled by the AAX template's RenderAudio from
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
    editor: Option<Box<dyn Editor>>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    transport_slot: Arc<truce_core::TransportSlot>,
    /// Cached serialized state. Pro Tools calls `GetChunkSize` +
    /// `GetChunk` as a pair, and for undo-checkpointing may call the
    /// pair repeatedly without any intervening state change. Caching
    /// avoids re-running `collect_values` + `serialize_state` on every
    /// call. Invalidated by `_set_param` and `_load_state`.
    state_cache: std::sync::Mutex<Option<Vec<u8>>>,
    /// Set when a param write or explicit load invalidates the cache.
    /// `_save_state` checks this; when true it re-serializes and
    /// clears the flag, when false it clones from the cache.
    state_dirty: std::sync::atomic::AtomicBool,
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

        let name = CString::new(resolved_plugin_name(&info)).unwrap_or_default();
        let vendor = CString::new(info.vendor).unwrap_or_default();

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
            name: name.as_ptr(),
            vendor: vendor.as_ptr(),
            version: 1,
            num_inputs: aax_inputs,
            num_outputs: aax_outputs,
            num_params: 0, // filled below
            manufacturer_id: fourcc(&info.au_manufacturer),
            product_id: fourcc(&info.fourcc),
            // plugin_id must differ from product_id — XOR with a salt
            plugin_id: fourcc(&info.fourcc) ^ 0x01010101,
            is_instrument: is_instrument as i32,
            category,
            has_editor: 0, // filled below
        };

        // Build param info + check for editor
        let mut temp = P::create();
        let param_infos = temp.params().param_infos();
        let mut params = Vec::with_capacity(param_infos.len());
        for pi in &param_infos {
            let cs = truce_core::wrapper::ParamCStrings::from_info(pi);
            let info = TruceAaxParamInfo {
                id: pi.id,
                name: cs.name.as_ptr(),
                min: pi.range.min(),
                max: pi.range.max(),
                default_value: pi.default_plain,
                step_count: pi.range.step_count(),
                unit: cs.unit.as_ptr(),
            };
            params.push(StaticParamInfo {
                info,
                _name: cs.name,
                _unit: cs.unit,
            });
        }

        let has_editor = temp.editor().is_some();
        let mut desc = descriptor;
        desc.num_params = params.len() as u32;
        desc.has_editor = has_editor as i32;
        // Fix name/vendor pointers — need to leak the CStrings
        let info = P::info();
        let name_leaked = CString::new(resolved_plugin_name(&info)).unwrap_or_default();
        let vendor_leaked = CString::new(info.vendor).unwrap_or_default();
        desc.name = name_leaked.into_raw();
        desc.vendor = vendor_leaked.into_raw();

        StaticInfo {
            descriptor: desc,
            params,
        }
    });
}

fn fourcc(bytes: &[u8; 4]) -> i32 {
    ((bytes[0] as i32) << 24)
        | ((bytes[1] as i32) << 16)
        | ((bytes[2] as i32) << 8)
        | (bytes[3] as i32)
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
            #[cfg_attr(target_os = "linux", link_section = ".init_array")]
            #[cfg_attr(target_os = "macos", link_section = "__DATA,__mod_init_func")]
            #[cfg_attr(target_os = "windows", link_section = ".CRT$XCU")]
            static INIT: extern "C" fn() = {
                extern "C" fn init() {
                    ::truce_aax::register_aax::<$plugin_type>();
                }
                init
            };

            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_get_descriptor(
                out: *mut ::truce_aax::TruceAaxDescriptor,
            ) {
                ::truce_aax::_get_descriptor(out);
            }
            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_get_param_info(
                index: u32,
                out: *mut ::truce_aax::TruceAaxParamInfo,
            ) {
                ::truce_aax::_get_param_info(index, out);
            }
            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_create() -> *mut ::std::ffi::c_void {
                ::truce_aax::_create::<$plugin_type>()
            }
            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_destroy(ctx: *mut ::std::ffi::c_void) {
                ::truce_aax::_destroy::<$plugin_type>(ctx);
            }
            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_reset(
                ctx: *mut ::std::ffi::c_void,
                sample_rate: f64,
                max_frames: u32,
            ) {
                ::truce_aax::_reset::<$plugin_type>(ctx, sample_rate, max_frames);
            }
            #[no_mangle]
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
            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_get_param(
                ctx: *mut ::std::ffi::c_void,
                id: u32,
            ) -> f64 {
                ::truce_aax::_get_param::<$plugin_type>(ctx, id)
            }
            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_set_param(
                ctx: *mut ::std::ffi::c_void,
                id: u32,
                value: f64,
            ) {
                ::truce_aax::_set_param::<$plugin_type>(ctx, id, value);
            }
            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_format_param(
                ctx: *mut ::std::ffi::c_void,
                id: u32,
                value: f64,
                out: *mut ::std::os::raw::c_char,
                out_len: u32,
            ) {
                ::truce_aax::_format_param::<$plugin_type>(ctx, id, value, out, out_len);
            }
            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_save_state(
                ctx: *mut ::std::ffi::c_void,
                out_data: *mut *mut u8,
            ) -> u32 {
                ::truce_aax::_save_state::<$plugin_type>(ctx, out_data)
            }
            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_load_state(
                ctx: *mut ::std::ffi::c_void,
                data: *const u8,
                len: u32,
            ) {
                ::truce_aax::_load_state::<$plugin_type>(ctx, data, len);
            }
            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_free_state(data: *mut u8, len: u32) {
                ::truce_aax::_free_state(data, len);
            }
            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_editor_create(
                ctx: *mut ::std::ffi::c_void,
                out: *mut ::truce_aax::TruceAaxEditorInfo,
            ) {
                ::truce_aax::_editor_create::<$plugin_type>(ctx, out);
            }
            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_editor_open(
                ctx: *mut ::std::ffi::c_void,
                parent_view: *mut ::std::ffi::c_void,
                platform: i32,
                callbacks: *const ::truce_aax::TruceAaxGuiCallbacks,
            ) {
                ::truce_aax::_editor_open::<$plugin_type>(ctx, parent_view, platform, callbacks);
            }
            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_editor_close(ctx: *mut ::std::ffi::c_void) {
                ::truce_aax::_editor_close::<$plugin_type>(ctx);
            }
            #[no_mangle]
            pub unsafe extern "C" fn truce_aax_editor_idle(ctx: *mut ::std::ffi::c_void) {
                ::truce_aax::_editor_idle::<$plugin_type>(ctx);
            }
            #[no_mangle]
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
// ---------------------------------------------------------------------------

pub unsafe fn _get_descriptor(out: *mut TruceAaxDescriptor) {
    if let Some(info) = INFO.get() {
        unsafe { *out = std::ptr::read(&info.descriptor) };
    }
}

pub unsafe fn _get_param_info(index: u32, out: *mut TruceAaxParamInfo) {
    if let Some(info) = INFO.get() {
        if let Some(p) = info.params.get(index as usize) {
            unsafe { *out = std::ptr::read(&p.info) };
        }
    }
}

pub unsafe fn _create<P: PluginExport>() -> *mut std::ffi::c_void {
    let mut plugin = P::create();
    plugin.init();
    let info = P::info();
    let instance = Box::new(AaxInstance::<P> {
        plugin,
        event_list: EventList::new(),
        output_events: EventList::new(),
        plugin_id_hash: state::hash_plugin_id(info.clap_id),
        sample_rate: 44100.0,
        editor: None,
        transport_slot: truce_core::TransportSlot::new(),
        state_cache: std::sync::Mutex::new(None),
        state_dirty: std::sync::atomic::AtomicBool::new(true),
    });
    Box::into_raw(instance) as *mut std::ffi::c_void
}

pub unsafe fn _destroy<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    if !ctx.is_null() {
        unsafe { drop(Box::from_raw(ctx as *mut AaxInstance<P>)) };
    }
}

pub unsafe fn _reset<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_rate: f64,
    max_frames: u32,
) {
    let inst = unsafe { &mut *(ctx as *mut AaxInstance<P>) };
    inst.sample_rate = sample_rate;
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
    let inst = unsafe { &mut *(ctx as *mut AaxInstance<P>) };
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
    unsafe {
        let mut scratch = truce_core::buffer::RawBufferScratch::default();
        let mut buffer = scratch.build(inputs, outputs, num_in, num_out, num_frames as u32);
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
            .process(&mut buffer, &inst.event_list, &mut context);
    }
}

pub unsafe fn _get_param<P: PluginExport>(ctx: *mut std::ffi::c_void, id: u32) -> f64 {
    let inst = unsafe { &*(ctx as *mut AaxInstance<P>) };
    inst.plugin.params().get_plain(id).unwrap_or(0.0)
}

pub unsafe fn _set_param<P: PluginExport>(ctx: *mut std::ffi::c_void, id: u32, value: f64) {
    let inst = unsafe { &*(ctx as *mut AaxInstance<P>) };
    inst.plugin.params().set_plain(id, value);
    // A param moved — the cached state blob is now stale.
    inst.state_dirty
        .store(true, std::sync::atomic::Ordering::Release);
}

pub unsafe fn _format_param<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
    out: *mut c_char,
    out_len: u32,
) {
    let inst = unsafe { &*(ctx as *mut AaxInstance<P>) };
    if let Some(text) = inst.plugin.params().format_value(id, value) {
        let bytes = text.as_bytes();
        let len = bytes.len().min(out_len as usize - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, out, len);
            *out.add(len) = 0;
        }
    }
}

pub unsafe fn _save_state<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out_data: *mut *mut u8,
) -> u32 {
    let inst = unsafe { &*(ctx as *mut AaxInstance<P>) };
    // Hot-path optimization for Pro Tools undo/snapshot flows, which
    // call the `GetChunkSize` + `GetChunk` pair repeatedly. On a
    // clean cache we hand back a clone of the last serialized blob;
    // otherwise we re-serialize and cache for the next call.
    let dirty = inst
        .state_dirty
        .swap(false, std::sync::atomic::Ordering::AcqRel);
    let blob = {
        let mut guard = match inst.state_cache.lock() {
            Ok(g) => g,
            // Poisoned (shouldn't happen; save_state is single-threaded
            // in practice). Bypass the cache rather than panicking
            // inside the AAX callback.
            Err(_) => {
                let (ids, values) = inst.plugin.params().collect_values();
                let extra = inst.plugin.save_state();
                let fresh =
                    state::serialize_state(inst.plugin_id_hash, &ids, &values, extra.as_deref());
                return finalize_blob(fresh, out_data);
            }
        };
        if dirty || guard.is_none() {
            let (ids, values) = inst.plugin.params().collect_values();
            let extra = inst.plugin.save_state();
            let fresh =
                state::serialize_state(inst.plugin_id_hash, &ids, &values, extra.as_deref());
            *guard = Some(fresh.clone());
            fresh
        } else {
            // SAFETY: we just checked is_some().
            guard.as_ref().unwrap().clone()
        }
    };
    finalize_blob(blob, out_data)
}

/// Hand a serialized state blob to the C caller as a raw pointer +
/// length. Caller later calls `_free_state` to drop the Box.
unsafe fn finalize_blob(blob: Vec<u8>, out_data: *mut *mut u8) -> u32 {
    let len = blob.len() as u32;
    let mut boxed = blob.into_boxed_slice();
    let ptr = boxed.as_mut_ptr();
    std::mem::forget(boxed);
    unsafe { *out_data = ptr };
    len
}

pub unsafe fn _load_state<P: PluginExport>(ctx: *mut std::ffi::c_void, data: *const u8, len: u32) {
    let inst = unsafe { &mut *(ctx as *mut AaxInstance<P>) };
    let blob = unsafe { slice::from_raw_parts(data, len as usize) };
    if let Some(deserialized) = state::deserialize_state(blob, inst.plugin_id_hash) {
        inst.plugin.params().restore_values(&deserialized.params);
        if let Some(extra) = &deserialized.extra {
            inst.plugin.load_state(extra);
        }
        // State changed wholesale — invalidate the serialization cache
        // so the next `_save_state` re-captures the restored values.
        inst.state_dirty
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(ref mut editor) = inst.editor {
            editor.state_changed();
        }
    }
}

// ---------------------------------------------------------------------------
// GUI bridge functions
// ---------------------------------------------------------------------------

pub unsafe fn _editor_create<P: PluginExport>(ctx: *mut c_void, out: *mut TruceAaxEditorInfo) {
    let inst = &mut *(ctx as *mut AaxInstance<P>);
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

pub unsafe fn _editor_open<P: PluginExport>(
    ctx: *mut c_void,
    parent_view: *mut c_void,
    platform: i32,
    callbacks: *const TruceAaxGuiCallbacks,
) {
    let inst = &mut *(ctx as *mut AaxInstance<P>);
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
    let plugin_ptr = SendPtr::new(&inst.plugin as *const P);
    let params_for_set = params.clone();
    let params_for_get = params.clone();
    let params_for_plain = params.clone();
    let params_for_fmt = params.clone();
    let transport_slot = inst.transport_slot.clone();

    let context = EditorContext {
        begin_edit: Arc::new(move |id| unsafe {
            touch_fn(aax_ctx.as_ptr() as *mut c_void, id);
        }),
        set_param: Arc::new(move |id, value| unsafe {
            params_for_set.set_normalized(id, value);
            let normalized = params_for_set.get_normalized(id).unwrap_or(0.0);
            set_fn(aax_ctx.as_ptr() as *mut c_void, id, normalized);
        }),
        end_edit: Arc::new(move |id| unsafe {
            release_fn(aax_ctx.as_ptr() as *mut c_void, id);
        }),
        request_resize: Arc::new(move |w, h| unsafe {
            resize_fn(aax_ctx.as_ptr() as *mut c_void, w, h) != 0
        }),
        get_param: Arc::new(move |id| params_for_get.get_normalized(id).unwrap_or(0.0)),
        get_param_plain: Arc::new(move |id| params_for_plain.get_plain(id).unwrap_or(0.0)),
        format_param: Arc::new(move |id| {
            let val = params_for_fmt.get_plain(id).unwrap_or(0.0);
            params_for_fmt
                .format_value(id, val)
                .unwrap_or_else(|| format!("{:.1}", val))
        }),
        get_meter: Arc::new(move |id| unsafe {
            let plugin = plugin_ptr.get();
            plugin.get_meter(id)
        }),
        get_state: Arc::new(move || unsafe {
            let plugin = plugin_ptr.get();
            plugin.save_state().unwrap_or_default()
        }),
        set_state: Arc::new(move |data| unsafe {
            let plugin = &mut *(plugin_ptr.as_ptr() as *mut P);
            plugin.load_state(&data);
        }),
        transport: Arc::new(move || transport_slot.read()),
    };

    let handle = match platform {
        1 => RawWindowHandle::AppKit(parent_view),
        3 => RawWindowHandle::Win32(parent_view),
        _ => return,
    };

    editor.open(handle, context);
}

pub unsafe fn _editor_close<P: PluginExport>(ctx: *mut c_void) {
    let inst = &mut *(ctx as *mut AaxInstance<P>);
    if let Some(ref mut editor) = inst.editor {
        editor.close();
    }
}

pub unsafe fn _editor_idle<P: PluginExport>(ctx: *mut c_void) {
    let inst = &mut *(ctx as *mut AaxInstance<P>);
    if let Some(ref mut editor) = inst.editor {
        editor.idle();
    }
}

pub unsafe fn _editor_get_size<P: PluginExport>(ctx: *mut c_void, w: *mut u32, h: *mut u32) -> i32 {
    let inst = &*(ctx as *mut AaxInstance<P>);
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

pub unsafe fn _free_state(data: *mut u8, len: u32) {
    if !data.is_null() && len > 0 {
        unsafe { drop(Vec::from_raw_parts(data, len as usize, len as usize)) };
    }
}
