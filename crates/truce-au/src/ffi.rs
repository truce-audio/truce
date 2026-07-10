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
    /// `1` if the plugin emits MIDI back to the host. The shim gates
    /// the MIDI output callback (v2) / `MIDIOutputNames` (v3) on this
    /// flag so a pure audio effect doesn't surface a phantom "MIDI
    /// Out" port in the host UI.
    pub has_midi_output: i32,
    /// `1` if the plugin accepts MIDI input. The v2 shim gates its
    /// `MusicDeviceMIDIEvent` handler lookup on this (decoupled from
    /// the `aumu` component type, so an `aumf` `MusicEffect` - an audio
    /// effect that opts into MIDI input - is also handed events).
    pub accepts_midi_in: i32,
    /// Number of MIDI input ports. AU v2 ignores this (single stream);
    /// AU v3 multi-cable input is not wired yet, so it's informational.
    pub midi_input_ports: u32,
    /// Number of MIDI output ports. The AU v3 appex sizes
    /// `MIDIOutputNames` to this and routes each event to its cable.
    pub midi_output_ports: u32,
    /// `1` if the MIDI input port is MIDI 2.0 dialect. The appex
    /// declares `audioUnitMIDIProtocol` = 2.0 when set (host delivers
    /// native UMP 2.0), else 1.0 (host down-converts). AU v2 ignores it.
    pub midi2_input: i32,
    /// `1` if the MIDI output port is MIDI 2.0 dialect. The appex's
    /// output drain emits UMP via `midiOutputEventListBlock` when set;
    /// the framework converts it to the host's protocol. AU v2 ignores it.
    pub midi2_output: i32,
    /// Main-bus input channel counts of each `bus_layouts()` entry.
    /// `null`/`num_layouts == 0` falls back to the single
    /// `(num_inputs, num_outputs)` pair.
    pub layout_in_channels: *const i16,
    /// Main-bus output channel counts, parallel to `layout_in_channels`.
    pub layout_out_channels: *const i16,
    /// Length of the two layout arrays. AU v2 reports them via
    /// `SupportedNumChannels`, AU v3 via `channelCapabilities`.
    pub num_layouts: u32,
    /// Channel count of the sidechain input bus (the second input bus),
    /// or `0` when the plugin declares no sidechain. Non-zero makes the
    /// shim expose a second `kAudioUnitScope_Input` element the host can
    /// route a separate source to. `num_inputs` stays the **main** input
    /// bus width (element 0).
    pub sidechain_in_channels: u32,
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
    /// MIDI status high-nibble of the default host-learn binding
    /// (`0xB0` CC, `0xE0` pitch bend, `0xD0` channel pressure, `0xC0`
    /// program change), or `0` for no binding.
    pub midi_status: u8,
    /// CC number when `midi_status` is `0xB0`; `0` otherwise.
    pub midi_data1: u8,
    /// Wire channel `0..=15`, or `-1` for any channel.
    pub midi_channel: i16,
}

/// AU shim ABI version, mirroring `#define TRUCE_AU_ABI_VERSION` in
/// `au_shim_types.h`. Stamped into [`AuCallbacks::abi_version`] at
/// registration so a v3 appex built by a newer `cargo-truce` can tell
/// how far the append-only callback tail extends on an older plugin
/// binary before calling into it. The high three bytes are the
/// `'TAu\0'` magic tag, the low byte the version - readers check the
/// magic so a non-versioned binary's leading function pointer can't
/// masquerade as a version. Bump the low byte and the header
/// `#define` together when appending a callback (or changing an
/// unreleased tail callback's signature); a test asserts they match.
/// v2: `output_ump_count` / `output_ump_at` gained the `protocol`
/// argument. v3: appended `latency_samples` / `tail_samples`. v4:
/// appended `set_render_mode`. v5: appended `param_parse_value`.
pub const TRUCE_AU_ABI_VERSION: u32 = 0x5441_7505;

/// Callbacks from the `ObjC` shim into Rust.
#[repr(C)]
pub struct AuCallbacks {
    /// ABI version this plugin was built against
    /// ([`TRUCE_AU_ABI_VERSION`]). MUST stay the first field so a
    /// shim/appex of any version reads it at offset 0 before touching
    /// the rest; consumers gate the append-only tail callbacks on it.
    /// Matches `AuCallbacks::abi_version` in `au_shim_types.h`.
    pub abi_version: u32,
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
        param_events: *const AuParamEvent,
        num_param_events: u32,
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
    /// Count of `SysEx`-shaped events the plug-in pushed during the
    /// most recent `process()` call. The AU v3 shim drains these
    /// after the channel-voice events, fragments each into UMP
    /// `SysEx`-8 packets, and emits via
    /// `midiOutputEventListBlock`. The AU v2 shim uses the
    /// `midiOutputCallback` framed-bytestream path.
    pub output_sysex_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    /// Fill `out_delta_frames`, `out_bytes`, `out_len` with the
    /// index-th `SysEx` output event. Returns inner bytes (no
    /// `0xF0` / `0xF7` framing); the shim re-adds framing for the
    /// AU v2 legacy callback path, and fragments into UMP packets
    /// for the AU v3 path.
    pub output_sysex_at: unsafe extern "C" fn(
        ctx: *mut c_void,
        index: u32,
        out_delta_frames: *mut u32,
        out_bytes: *mut *const u8,
        out_len: *mut u32,
    ),

    // GUI
    pub gui_has_editor: unsafe extern "C" fn(ctx: *mut c_void) -> i32,
    pub gui_get_size: unsafe extern "C" fn(ctx: *mut c_void, w: *mut u32, h: *mut u32),
    pub gui_open: unsafe extern "C" fn(ctx: *mut c_void, parent: *mut c_void),
    pub gui_close: unsafe extern "C" fn(ctx: *mut c_void),
    /// `1` if the editor opted into host-driven resize. The AU v3
    /// Swift shim consults this to decide whether to propagate the
    /// host's bounds change to `gui_set_size` from
    /// `viewDidLayoutSubviews`.
    pub gui_can_resize: unsafe extern "C" fn(ctx: *mut c_void) -> i32,
    /// Host -> plugin `set_size`. The AU v3 Swift shim calls this
    /// when its parent view's bounds change (host drag-resize) and
    /// the editor opted into resize.
    pub gui_set_size: unsafe extern "C" fn(ctx: *mut c_void, w: u32, h: u32),

    /// Number of factory presets bundled into the component
    /// (`Contents/Resources/Presets/*.trucepreset`). `0` makes the
    /// shim report `kAudioUnitProperty_FactoryPresets` as invalid.
    /// Fields from here on are append-only - see `au_shim_types.h`.
    pub factory_preset_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    /// UTF-8 display name of the index-th factory preset. The
    /// returned pointer is owned by the Rust side and valid for the
    /// process lifetime.
    pub factory_preset_name: unsafe extern "C" fn(ctx: *mut c_void, index: u32) -> *const c_char,
    /// Load the index-th factory preset - the same apply path as
    /// `state_load`. Returns 1 on success.
    pub factory_preset_load: unsafe extern "C" fn(ctx: *mut c_void, index: u32) -> i32,
    /// Host → plugin `SysEx` input (AU v2). The shim strips the
    /// `0xF0`/`0xF7` framing and passes the inner bytes; the Rust side
    /// copies into the `EventList` `SysEx` pool. `sample_offset` is the
    /// block-relative frame (0 for AU v2's untimed `MusicDeviceSysEx`).
    pub push_sysex_input:
        unsafe extern "C" fn(ctx: *mut c_void, sample_offset: u32, bytes: *const u8, len: u32),
    /// UMP channel-voice output for AU v3's `MIDIEventList` block. The
    /// appex passes `protocol` (1 = MIDI 1.0, 2 = MIDI 2.0, from the
    /// host's `hostMIDIProtocol`) and the Rust side encodes a *pure*
    /// stream in it - all MT 0x2 for 1.0, all MT 0x4 for 2.0, events
    /// converting across dialects as needed (the UMP spec forbids
    /// mixing the two channel-voice types in one protocol stream).
    /// The count depends on the protocol, so both calls take it.
    /// Appended here (not beside the other `output_*` callbacks) so
    /// the field is added at the struct tail per the append-only rule -
    /// a mid-struct insert would shift every later offset and skew a
    /// newer appex against an older plugin binary.
    pub output_ump_count: unsafe extern "C" fn(ctx: *mut c_void, protocol: u32) -> u32,
    pub output_ump_at:
        unsafe extern "C" fn(ctx: *mut c_void, protocol: u32, index: u32, out: *mut AuUmpEvent),
    /// Number of legacy `ClassInfo` dictionary keys to probe when
    /// truce's own `truce_state` entry is absent (`au_keys` in
    /// `truce.toml`'s `[plugin.legacy_state]`).
    pub legacy_state_key_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    /// The index-th legacy key as a NUL-terminated UTF-8 string owned
    /// by the Rust side, valid for the instance lifetime.
    pub legacy_state_key_at: unsafe extern "C" fn(ctx: *mut c_void, index: u32) -> *const c_char,
    /// Offer bytes found under a legacy key to the plugin's
    /// `migrate_state` hook. Returns 1 when the plugin translated and
    /// accepted them, 0 otherwise (the shim then tries the next key).
    pub state_load_foreign: unsafe extern "C" fn(
        ctx: *mut c_void,
        key: *const c_char,
        data: *const u8,
        len: u32,
    ) -> i32,
    /// Plugin latency in samples, for host delay compensation. The
    /// shim divides by the sample rate to report seconds. Tracks the
    /// plugin's `latency()`; refreshed on reset and every process
    /// block. Appended per the append-only rule (gate on `abi_version`
    /// cross-binary).
    pub latency_samples: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    /// Plugin release-tail length in samples. Mirrors
    /// [`Self::latency_samples`] for `kAudioUnitProperty_TailTime` /
    /// `AUAudioUnit.tailTime`.
    pub tail_samples: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    /// Host → plugin render-mode signal, as a `ProcessMode`
    /// discriminant (0 realtime, 1 buffered, 2 offline). The shim calls
    /// it when the host toggles AU offline rendering
    /// (`kAudioUnitProperty_OfflineRender` on v2,
    /// `AUAudioUnit.isRenderingOffline` on v3); the Rust side stashes it
    /// in an atomic that `cb_reset` and every `cb_process` block read.
    /// Appended per the append-only rule (gate on `abi_version`
    /// cross-binary).
    pub set_render_mode: unsafe extern "C" fn(ctx: *mut c_void, mode: u32),
    /// Parse host text-entry (UTF-8) into a plain param value. Returns
    /// `1` and writes `out_plain` on success, `0` when unparseable.
    /// Backs `kAudioUnitProperty_ParameterValueFromString` (v2) and the
    /// v3 appex's `implementorValueFromStringCallback`. Appended per the
    /// append-only rule (gate on `abi_version` cross-binary).
    pub param_parse_value: unsafe extern "C" fn(
        ctx: *mut c_void,
        id: u32,
        text: *const c_char,
        out_plain: *mut f64,
    ) -> i32,
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
    /// MIDI cable / port. Output: the AU v3 appex passes it as the
    /// `cable` to `midiOutputEventBlock` (0 on AU v2, single stream).
    /// Input: currently always 0 (multi-cable input unwired). Occupies
    /// the trailing byte that keeps the struct 8-byte aligned to match
    /// `au_shim_types.h`.
    pub port: u8,
}

/// Universal MIDI Packet container - carries MIDI 2.0 channel-voice
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
    /// 0x4 = MIDI 2.0 CV (64-bit), 0x5 = data 128 (128-bit).
    /// Types 0x3 (SysEx-7), 0x4 (MIDI 2.0 CV), and 0x5 (data 128
    /// / SysEx-8) are decoded; 0x0 / 0x1 / 0x2 are reserved.
    pub words: [u32; 4],
}

/// Plugin -> host UMP output event (AU v3, MIDI 2.0 protocol mode).
/// Mirrors `AuUmpEvent` in `au_shim_types.h`. `word_count` is 1
/// (MT 0x2, MIDI 1.0 CV) or 2 (MT 0x4, MIDI 2.0 CV); only that many
/// `words` are valid. `cable` is the MIDI output port.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct AuUmpEvent {
    pub sample_offset: u32,
    pub cable: u8,
    pub word_count: u8,
    /// Padding to 4-byte-align `words`; matches `_reserved` in
    /// `au_shim_types.h` by position.
    pub reserved: [u8; 2],
    pub words: [u32; 4],
}

/// Host-side parameter automation event. The AU v3 Swift shim
/// decodes `AURenderEvent.parameter` / `.parameterRamp` entries
/// into this shape (one row per host event) with `sample_offset`
/// relative to the start of the current render block. The Rust
/// `process` callback converts each row into an
/// `EventBody::ParamChange` so the chunker splits the audio block
/// at each automation point. AU v2's `AudioUnitSetParameter`
/// carries no sample-offset, so the v2 shim passes `NULL` / `0`
/// for the array and parameter updates land synchronously through
/// `param_set_value` instead.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct AuParamEvent {
    /// Sample offset within the current block.
    pub sample_offset: u32,
    /// Plugin parameter ID (matches the `id` from `#[derive(Params)]`'s
    /// generated `*ParamId` enum and `AuParamDescriptor::id`).
    pub param_id: u32,
    /// Plain value. AU represents parameter values as `AUValue`
    /// (a `float`); the Rust side widens to `f64` when building
    /// the `EventBody::ParamChange`.
    pub value: f32,
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
    pub fn truce_au_register_v2(
        descriptor: *const AuPluginDescriptor,
        callbacks: *const AuCallbacks,
        param_descriptors: *const AuParamDescriptor,
        num_params: u32,
    );

}
