//! C ABI types for the VST2 shim boundary.

use std::ffi::c_void;
use std::os::raw::c_char;

#[repr(C)]
pub struct Vst2PluginDescriptor {
    pub component_type: [u8; 4],
    pub component_subtype: [u8; 4],
    pub name: *const c_char,
    pub vendor: *const c_char,
    pub version: u32,
    pub num_inputs: u32,
    pub num_outputs: u32,
}

#[repr(C)]
pub struct Vst2ParamDescriptor {
    pub id: u32,
    pub name: *const c_char,
    pub min: f64,
    pub max: f64,
    pub default_value: f64,
    pub step_count: u32,
    pub unit: *const c_char,
    pub group: *const c_char,
}

#[repr(C)]
pub struct Vst2MidiEvent {
    pub delta_frames: u32,
    pub status: u8,
    pub data1: u8,
    pub data2: u8,
    pub _pad: u8,
}

#[repr(C)]
pub struct Vst2Callbacks {
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
        events: *const Vst2MidiEvent,
        num_events: u32,
    ),
    pub param_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    pub param_get_descriptor:
        unsafe extern "C" fn(ctx: *mut c_void, index: u32, out: *mut Vst2ParamDescriptor),
    pub param_get_value: unsafe extern "C" fn(ctx: *mut c_void, id: u32) -> f64,
    pub param_set_value: unsafe extern "C" fn(ctx: *mut c_void, id: u32, value: f64),
    pub param_format_value: unsafe extern "C" fn(
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
    // Host notification
    pub set_effect_ptr: unsafe extern "C" fn(ctx: *mut c_void, effect: *mut c_void),
    // GUI
    pub gui_has_editor: unsafe extern "C" fn(ctx: *mut c_void) -> i32,
    pub gui_get_size: unsafe extern "C" fn(ctx: *mut c_void, w: *mut u32, h: *mut u32),
    pub gui_open: unsafe extern "C" fn(ctx: *mut c_void, parent: *mut c_void),
    pub gui_close: unsafe extern "C" fn(ctx: *mut c_void),
}

extern "C" {
    pub fn truce_vst2_register(
        descriptor: *const Vst2PluginDescriptor,
        callbacks: *const Vst2Callbacks,
        params: *const Vst2ParamDescriptor,
        num_params: u32,
    );
}
