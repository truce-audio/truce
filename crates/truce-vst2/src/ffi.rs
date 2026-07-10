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
    /// Param ID flagged as `IS_BYPASS`, or `u32::MAX` for "no bypass
    /// param". The C shim handles `effSetBypass` (opcode 44) by
    /// writing `0.0`/`1.0` to this param so the host's master-bypass
    /// UI tracks the param value.
    pub bypass_param_id: u32,
    /// `1` if the plugin accepts MIDI input. Gates the `receiveVst*`
    /// canDo replies.
    pub accepts_midi_in: i32,
    /// `1` if the plugin emits MIDI to the host. Gates the `sendVst*`
    /// canDo replies.
    pub emits_midi: i32,
    /// Non-zero when the plugin's `Sample` is `f64`. The shim then
    /// sets `effFlagsCanDoubleReplacing`, wires
    /// `AEffect::processDoubleReplacing`, and routes those blocks
    /// through `process_f64` so the plugin reads/writes host memory
    /// directly with no precision conversion.
    pub supports_f64: i32,
    /// Sidechain (non-main) input width from the first bus layout. VST2
    /// has no separate-bus concept, so the sidechain rides the last
    /// `sidechain_in_channels` of `num_inputs`; the shim labels those
    /// pins "Sidechain N" via `effGetInputProperties`.
    pub sidechain_in_channels: u32,
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
#[derive(Copy, Clone)]
pub struct Vst2MidiEvent {
    pub delta_frames: u32,
    pub status: u8,
    pub data1: u8,
    pub data2: u8,
    // Trailing 1-byte pad; preserves the 8-byte struct layout the C
    // shim writes into. Public so it stays addressable in the same
    // crate's `mem::offset_of!` callers; the leading `_` reserves
    // it from external use.
    #[allow(clippy::pub_underscore_fields)]
    pub _pad: u8,
}

#[repr(C)]
pub struct Vst2Callbacks {
    pub create: unsafe extern "C" fn() -> *mut c_void,
    pub destroy: unsafe extern "C" fn(ctx: *mut c_void),
    pub reset: unsafe extern "C" fn(ctx: *mut c_void, sample_rate: f64, max_frames: u32),
    /// `process_level` is the host's
    /// `audioMasterGetCurrentProcessLevel` (realtime / prefetch /
    /// offline) polled per block.
    pub process: unsafe extern "C" fn(
        ctx: *mut c_void,
        inputs: *const *const f32,
        outputs: *mut *mut f32,
        num_input_channels: u32,
        num_output_channels: u32,
        num_frames: u32,
        events: *const Vst2MidiEvent,
        num_events: u32,
        process_level: i32,
    ),
    /// 64-bit twin of `process`, called from
    /// `AEffect::processDoubleReplacing` (only wired when
    /// `Vst2PluginDescriptor::supports_f64` is set).
    pub process_f64: unsafe extern "C" fn(
        ctx: *mut c_void,
        inputs: *const *const f64,
        outputs: *mut *mut f64,
        num_input_channels: u32,
        num_output_channels: u32,
        num_frames: u32,
        events: *const Vst2MidiEvent,
        num_events: u32,
        process_level: i32,
    ),
    pub param_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    /// VST2 hosts work in normalized `[0, 1]` space. The Rust side
    /// is responsible for routing through `ParamRange::denormalize`
    /// so non-linear tapers (e.g. `Logarithmic` for a 20 Hz – 20 kHz
    /// freq knob) round-trip correctly.
    pub param_get_normalized: unsafe extern "C" fn(ctx: *mut c_void, id: u32) -> f64,
    pub param_set_normalized: unsafe extern "C" fn(ctx: *mut c_void, id: u32, value: f64),
    /// Format the param's *current* plain value for display. The shim
    /// can call this directly inside `effGetParamDisplay` without
    /// having to round-trip a value through normalize/denormalize.
    pub param_format_current:
        unsafe extern "C" fn(ctx: *mut c_void, id: u32, out: *mut c_char, out_len: u32) -> u32,
    /// Parse host text-entry (UTF-8) and apply it to the param, backing
    /// `effString2Parameter`. Returns `1` on success, `0` when the text
    /// can't be parsed. The parse + set happen Rust-side (VST2 has no
    /// plain<->normalized callback for the shim to bridge).
    pub param_parse:
        unsafe extern "C" fn(ctx: *mut c_void, id: u32, text: *const c_char) -> i32,
    /// Number of *encodable* plugin → host MIDI events queued by the
    /// last `process()` call. Unsupported event types (MIDI 2.0,
    /// `ParamChange`, Transport) are filtered out so the C shim can
    /// iterate `0..count` without checking for skipped slots.
    pub output_event_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    /// Fill `out` with the index-th encodable output event. The
    /// `Vst2MidiEventCompact` shape mirrors the input direction.
    pub output_event_at:
        unsafe extern "C" fn(ctx: *mut c_void, index: u32, out: *mut Vst2MidiEvent),
    /// `SysEx` input - shim calls once per `kVstSysExType` event in
    /// `effProcessEvents`, **after** stripping the leading `0xF0`
    /// / trailing `0xF7` framing the host includes (Steinberg
    /// vendor-extension convention; real-world hosts like Cubase
    /// and Reaper deliver framed bytes). Rust sees inner bytes
    /// only; valid for the duration of this call.
    pub push_sysex_input:
        unsafe extern "C" fn(ctx: *mut c_void, delta_frames: u32, bytes: *const u8, len: u32),
    /// Count of `SysEx`-shaped events the plug-in pushed during
    /// `process()`.
    pub output_sysex_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    /// Fill `out_delta_frames`, `out_bytes`, `out_len` with the
    /// index-th `SysEx` output event. Returns inner bytes (no
    /// `0xF0` / `0xF7` framing) - the shim re-adds framing into
    /// its per-block scratch before handing the bytes to the host.
    /// Pointer is valid until the next `process()` call clears the
    /// pool (which happens after the host has consumed the event).
    pub output_sysex_at: unsafe extern "C" fn(
        ctx: *mut c_void,
        index: u32,
        out_delta_frames: *mut u32,
        out_bytes: *mut *const u8,
        out_len: *mut u32,
    ),
    pub state_save:
        unsafe extern "C" fn(ctx: *mut c_void, out_data: *mut *mut u8, out_len: *mut u32),
    /// Returns `1` when the blob was accepted (truce envelope, or
    /// `migrate_state` translated it), `0` when the load failed -
    /// the shim's `effSetChunk` forwards that to the host.
    pub state_load: unsafe extern "C" fn(ctx: *mut c_void, data: *const u8, len: u32) -> i32,
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

unsafe extern "C" {
    pub fn truce_vst2_register(
        descriptor: *const Vst2PluginDescriptor,
        callbacks: *const Vst2Callbacks,
        params: *const Vst2ParamDescriptor,
        num_params: u32,
    );
}
