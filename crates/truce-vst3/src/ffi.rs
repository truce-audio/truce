//! C ABI types for the Rust ↔ C++ VST3 shim boundary.
//! Mirrors the AU wrapper's ffi.rs pattern.

use std::ffi::c_void;
use std::os::raw::c_char;

/// Plugin descriptor passed from Rust to the C++ shim.
#[repr(C)]
pub struct Vst3PluginDescriptor {
    pub name: *const c_char,
    pub vendor: *const c_char,
    pub url: *const c_char,
    pub email: *const c_char,
    pub version: *const c_char,
    /// VST3 class ID (16 bytes)
    pub cid: [u8; 16],
    /// "Audio Module Class" for processors
    pub category: *const c_char,
    /// Subcategories like "Fx" or "Instrument|Synth"
    pub subcategories: *const c_char,
    pub num_inputs: u32,
    pub num_outputs: u32,
}

/// Parameter descriptor.
#[repr(C)]
pub struct Vst3ParamDescriptor {
    pub id: u32,
    pub name: *const c_char,
    pub short_name: *const c_char,
    pub units: *const c_char,
    pub min: f64,
    pub max: f64,
    pub default_normalized: f64,
    pub step_count: i32,
    pub flags: i32,
    pub group: *const c_char,
}

/// MIDI event passed from the C++ shim to Rust.
#[repr(C)]
pub struct Vst3MidiEvent {
    pub sample_offset: u32,
    pub status: u8,
    pub data1: u8,
    pub data2: u8,
    pub _pad: u8,
}

/// Transport info passed from the C++ shim to Rust.
#[repr(C)]
pub struct Vst3Transport {
    pub playing: i32,
    pub recording: i32,
    pub tempo: f64,
    pub time_sig_num: i32,
    pub time_sig_den: i32,
    pub position_samples: f64,
    pub position_beats: f64,
    pub bar_start_beats: f64,
    pub cycle_start_beats: f64,
    pub cycle_end_beats: f64,
    pub cycle_active: i32,
}

/// Parameter change event with sample offset (for sample-accurate automation).
#[repr(C)]
pub struct Vst3ParamChange {
    pub id: u32,
    pub sample_offset: i32,
    pub value: f64, // plain value (already denormalized)
}

/// Callbacks from the C++ shim into Rust.
#[repr(C)]
pub struct Vst3Callbacks {
    pub create: unsafe extern "C" fn() -> *mut c_void,
    pub destroy: unsafe extern "C" fn(ctx: *mut c_void),
    pub reset: unsafe extern "C" fn(ctx: *mut c_void, sample_rate: f64, max_frames: u32),
    pub process: unsafe extern "C" fn(
        ctx: *mut c_void,
        inputs: *const *const f32,
        outputs: *mut *mut f32,
        num_input_channels: u32,
        num_output_channels: u32,
        num_frames: u32,
        events: *const Vst3MidiEvent,
        num_events: u32,
        transport: *const Vst3Transport,
        param_changes: *const Vst3ParamChange,
        num_param_changes: u32,
    ),
    pub param_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    pub param_get_descriptor:
        unsafe extern "C" fn(ctx: *mut c_void, index: u32, out: *mut Vst3ParamDescriptor),
    pub param_get_value: unsafe extern "C" fn(ctx: *mut c_void, id: u32) -> f64,
    pub param_set_value: unsafe extern "C" fn(ctx: *mut c_void, id: u32, value: f64),
    pub param_normalize: unsafe extern "C" fn(ctx: *mut c_void, id: u32, plain: f64) -> f64,
    pub param_denormalize: unsafe extern "C" fn(ctx: *mut c_void, id: u32, normalized: f64) -> f64,
    pub param_format: unsafe extern "C" fn(
        ctx: *mut c_void,
        id: u32,
        value: f64,
        out: *mut c_char,
        out_len: u32,
    ) -> u32,
    pub state_save:
        unsafe extern "C" fn(ctx: *mut c_void, out_data: *mut *mut u8, out_len: *mut u32),
    pub state_load: unsafe extern "C" fn(ctx: *mut c_void, data: *const u8, len: u32),
    pub state_free: unsafe extern "C" fn(data: *mut u8, len: u32),
    // Latency + tail
    pub get_latency: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    pub get_tail: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    // Output events
    pub get_output_event_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    pub get_output_event: unsafe extern "C" fn(ctx: *mut c_void, index: u32, out: *mut Vst3MidiEvent),
    // GUI
    pub gui_has_editor: unsafe extern "C" fn(ctx: *mut c_void) -> i32,
    pub gui_get_size: unsafe extern "C" fn(ctx: *mut c_void, w: *mut u32, h: *mut u32),
    pub gui_open: unsafe extern "C" fn(ctx: *mut c_void, parent: *mut c_void),
    pub gui_close: unsafe extern "C" fn(ctx: *mut c_void),
}

extern "C" {
    /// Register the plugin with the VST3 shim.
    pub fn truce_vst3_register(
        descriptor: *const Vst3PluginDescriptor,
        callbacks: *const Vst3Callbacks,
        param_descriptors: *const Vst3ParamDescriptor,
        num_params: u32,
    );

    /// Get the VST3 factory COM object. Called by GetPluginFactory.
    pub fn truce_vst3_get_factory() -> *mut std::ffi::c_void;

    /// Notify host: begin editing a parameter (mouse-down).
    pub fn truce_vst3_begin_edit(ctx: *mut std::ffi::c_void, id: u32);

    /// Notify host: parameter value changed during a gesture.
    pub fn truce_vst3_perform_edit(ctx: *mut std::ffi::c_void, id: u32, normalized: f64);

    /// Notify host: end editing a parameter (mouse-up).
    pub fn truce_vst3_end_edit(ctx: *mut std::ffi::c_void, id: u32);
}
