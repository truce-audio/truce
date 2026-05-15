//! C ABI types for the Rust ↔ Objective-C boundary.

use std::ffi::c_void;
use std::os::raw::c_char;

/// Plugin descriptor passed from Rust to the `ObjC` shim at registration time.
#[repr(C)]
pub struct AuPluginDescriptor {
    /// AU component type: "aufx" (effect), "aumu" (instrument), "aumf" (MIDI effect)
    pub component_type: [u8; 4],
    /// AU component subtype (4 bytes, e.g., "Gain")
    pub component_subtype: [u8; 4],
    /// AU component manufacturer (4 bytes, e.g., "`OAPl`")
    pub component_manufacturer: [u8; 4],
    /// Display name
    pub name: *const c_char,
    /// Vendor name
    pub vendor: *const c_char,
    /// Version as u32 (e.g., 0x00010000 for 1.0.0)
    pub version: u32,
    /// Number of input channels (0 for instruments)
    pub num_inputs: u32,
    /// Number of output channels
    pub num_outputs: u32,
    /// Param ID flagged as `IS_BYPASS`, or `u32::MAX` if the plugin has
    /// no bypass param. The AU shim routes
    /// `kAudioUnitProperty_BypassEffect` get/set through this ID so the
    /// host's master-bypass UI tracks the param's value.
    pub bypass_param_id: u32,
    /// `1` if the plugin emits MIDI back to the host (note effects /
    /// arpeggiators / chord generators). The AU v3 shim gates
    /// `MIDIOutputNames` on this flag so a pure audio effect doesn't
    /// surface a phantom "MIDI Out" port in the host UI.
    pub has_midi_output: i32,
}

/// Parameter descriptor for the `ObjC` shim.
#[repr(C)]
pub struct AuParamDescriptor {
    pub id: u32,
    pub name: *const c_char,
    pub min: f64,
    pub max: f64,
    pub default_value: f64,
    /// 0 = continuous, >0 = number of discrete steps
    pub step_count: u32,
    /// Unit string (e.g., "dB", "Hz", "")
    pub unit: *const c_char,
    /// Group name (empty string for top-level)
    pub group: *const c_char,
}

/// Callbacks from the `ObjC` shim into Rust.
#[repr(C)]
pub struct AuCallbacks {
    /// Create a new plugin instance. Returns an opaque context pointer.
    pub create: unsafe extern "C" fn() -> *mut c_void,

    /// Destroy the plugin instance.
    pub destroy: unsafe extern "C" fn(ctx: *mut c_void),

    /// Reset the plugin (called when sample rate or max block size changes).
    pub reset: unsafe extern "C" fn(ctx: *mut c_void, sample_rate: f64, max_frames: u32),

    /// Process audio. The shim calls this from the render block.
    ///
    /// - `inputs`: array of `num_input_channels` float pointers
    /// - `outputs`: array of `num_output_channels` float pointers
    /// - `num_frames`: number of samples to process
    /// - `events`: pointer to packed MIDI event buffer (see `AuMidiEvent`)
    /// - `num_events`: number of MIDI events
    /// - `transport`: may be null when the host did not provide transport
    ///   info for this block (or is not capable of doing so).
    pub process: unsafe extern "C" fn(
        ctx: *mut c_void,
        inputs: *const *const f32,
        outputs: *mut *mut f32,
        num_input_channels: u32,
        num_output_channels: u32,
        num_frames: u32,
        events: *const AuMidiEvent,
        num_events: u32,
        events2: *const AuMidi2Event,
        num_events2: u32,
        transport: *const AuTransportSnapshot,
    ),

    /// Get parameter count.
    pub param_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,

    /// Get a parameter's current plain value.
    pub param_get_value: unsafe extern "C" fn(ctx: *mut c_void, id: u32) -> f64,

    /// Set a parameter's plain value.
    pub param_set_value: unsafe extern "C" fn(ctx: *mut c_void, id: u32, value: f64),

    /// Format a parameter value to a display string.
    /// Returns the number of bytes written (excluding null terminator), or 0 on failure.
    pub param_format_value: unsafe extern "C" fn(
        ctx: *mut c_void,
        id: u32,
        value: f64,
        out: *mut c_char,
        out_len: u32,
    ) -> u32,

    /// Save state. Returns a malloc'd buffer and its length.
    /// Caller (`ObjC` shim) is responsible for freeing via `state_free`.
    pub state_save:
        unsafe extern "C" fn(ctx: *mut c_void, out_data: *mut *mut u8, out_len: *mut u32),

    /// Load state from a buffer.
    pub state_load: unsafe extern "C" fn(ctx: *mut c_void, data: *const u8, len: u32),

    /// Free a buffer returned by `state_save`.
    pub state_free: unsafe extern "C" fn(data: *mut u8, len: u32),

    /// Number of *encodable* plugin → host MIDI events queued by the
    /// last `process()` call. Unsupported event types (MIDI 2.0,
    /// `ParamChange`, Transport) are filtered out so the shim can
    /// iterate `0..count` without checking for skipped slots.
    pub output_event_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    /// Fill `out` with the index-th encodable output event.
    pub output_event_at: unsafe extern "C" fn(ctx: *mut c_void, index: u32, out: *mut AuMidiEvent),

    // GUI
    pub gui_has_editor: unsafe extern "C" fn(ctx: *mut c_void) -> i32,
    pub gui_get_size: unsafe extern "C" fn(ctx: *mut c_void, w: *mut u32, h: *mut u32),
    pub gui_open: unsafe extern "C" fn(ctx: *mut c_void, parent: *mut c_void),
    pub gui_close: unsafe extern "C" fn(ctx: *mut c_void),
}

/// A MIDI event passed across the Rust ↔ `ObjC` boundary in both
/// directions (host → plugin via the input event array and plugin →
/// host via `output_event_at`).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct AuMidiEvent {
    /// Sample offset within the current block.
    pub sample_offset: u32,
    /// MIDI status byte (0x90 = note on, 0x80 = note off, etc.)
    pub status: u8,
    /// MIDI data byte 1 (note number for note on/off)
    pub data1: u8,
    /// MIDI data byte 2 (velocity for note on/off)
    pub data2: u8,
    // Trailing 1-byte pad keeping the struct's 8-byte alignment to
    // match `AuMidiEvent` in `au_shim_types.h`. Public so the C
    // shim's offset calculations agree.
    #[allow(clippy::pub_underscore_fields)]
    pub _pad: u8,
}

/// Universal MIDI Packet container — carries MIDI 2.0 channel-voice
/// messages (64-bit UMPs, words[0..2]) and forward-compat slots for
/// SysEx-8 / data (128-bit UMPs, all four words). AU v3 hosts on iOS
/// 17+ / macOS 14+ deliver MIDI through `AURenderEvent.MIDIEventList`
/// which carries UMPs natively; the Swift shim walks the packet list,
/// classifies each word group by its UMP message type nibble (top 4
/// bits of `words[0]`), and forwards MIDI 2.0 messages here while
/// continuing to down-convert MIDI 1.0 ones to the legacy `AuMidiEvent`
/// path.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct AuMidi2Event {
    /// Sample offset within the current block.
    pub sample_offset: u32,
    /// Up to four 32-bit UMP words, MSB-first. UMP message types:
    /// 0x0 = utility (32-bit), 0x1 = system real-time (32-bit),
    /// 0x2 = MIDI 1.0 CV (32-bit), 0x3 = SysEx-7 (64-bit),
    /// 0x4 = MIDI 2.0 CV (64-bit), 0x5 = data 128 (128-bit). Only
    /// MIDI 2.0 CV is decoded today; the rest are reserved.
    pub words: [u32; 4],
}

/// Transport snapshot filled by the shim from `HostCallbackInfo` (AU v2)
/// or `AUAudioUnit.musicalContextBlock` / `transportStateBlock` (AU v3).
///
/// Layout must match `AuTransportSnapshot` in `au_shim_types.h`.
#[repr(C)]
#[derive(Default)]
pub struct AuTransportSnapshot {
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

// Functions implemented in the ObjC shim, called from Rust.
unsafe extern "C" {
    /// Register the plugin with the AU system. Called once at load time.
    /// The descriptor and callbacks must remain valid for the lifetime of the process.
    pub fn truce_au_register(
        descriptor: *const AuPluginDescriptor,
        callbacks: *const AuCallbacks,
        param_descriptors: *const AuParamDescriptor,
        num_params: u32,
    );

}
