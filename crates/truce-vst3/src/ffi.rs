//! C ABI types for the Rust / C++ VST3 shim boundary.

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
    /// Number of MIDI output ports (event output buses). `0` disables
    /// output events entirely - the host never allocates
    /// `ProcessData::outputEvents` and the drain loop after `process()`
    /// is a no-op. The shim advertises this many `kEvent | kOutput`
    /// buses; the plugin routes each event to a bus via `Event::port`.
    pub midi_output_ports: i32,
    /// Number of MIDI input ports (event input buses). `0` means the
    /// plugin takes no MIDI (decoupled from `num_inputs` so an audio
    /// effect can also take MIDI). The shim advertises this many
    /// `kEvent | kInput` buses and stamps each event's `Event::port`
    /// from the bus it arrived on.
    pub midi_input_ports: i32,
    /// Non-zero when the plugin's `Sample` is `f64`. The shim then
    /// answers `canProcessSampleSize(kSample64)` with `kResultOk`,
    /// accepts a 64-bit `setupProcessing`, and routes blocks through
    /// `process_f64` so the plugin reads/writes host memory directly
    /// with no precision conversion.
    pub supports_f64: i32,
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

/// MIDI event passed across the Rust ↔ C++ boundary in both
/// directions (host → plugin via `events` / `num_events` and plugin →
/// host via `cb_get_output_event`).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct Vst3MidiEvent {
    pub sample_offset: u32,
    pub status: u8,
    pub data1: u8,
    pub data2: u8,
    /// Event bus index the event arrived on / goes out on, mapped to
    /// [`truce_core::Event::port`]. `0` for single-port plugins.
    pub port: u8,
    /// The host's VST3 `noteId` on note on/off and note-expression
    /// events; `-1` when the host assigned none (and on every other
    /// event kind). Full `i32` because hosts hand out arbitrary
    /// per-voice counters, not pitches.
    pub note_id: i32,
    /// Full-precision note-expression value (`0..=1`) for
    /// status-`0xF0` events; `0.0` otherwise. Carried separately from
    /// `data2` so the host's `f64` survives the crossing unquantized.
    pub ne_value: f64,
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
    /// `process_mode` is the VST3 `ProcessSetup::processMode`
    /// (`kRealtime` / `kPrefetch` / `kOffline`).
    pub reset: unsafe extern "C" fn(
        ctx: *mut c_void,
        sample_rate: f64,
        max_frames: u32,
        process_mode: i32,
    ),
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
        // VST3 `ProcessData::processMode` for this block.
        process_mode: i32,
    ),
    /// 64-bit twin of `process`. The shim calls exactly one of the
    /// two per block, chosen by the sample size the host negotiated
    /// in `setupProcessing` (only offered when
    /// `Vst3PluginDescriptor::supports_f64` is set).
    pub process_f64: unsafe extern "C" fn(
        ctx: *mut c_void,
        inputs: *const *const f64,
        outputs: *mut *mut f64,
        num_input_channels: u32,
        num_output_channels: u32,
        num_frames: u32,
        events: *const Vst3MidiEvent,
        num_events: u32,
        transport: *const Vst3Transport,
        param_changes: *const Vst3ParamChange,
        num_param_changes: u32,
        process_mode: i32,
    ),
    pub param_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
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
    /// Returns `1` when the blob was accepted (truce envelope, or
    /// `migrate_state` translated it), `0` when the load failed -
    /// the shim's `setState` forwards that as `kResultFalse`.
    pub state_load: unsafe extern "C" fn(ctx: *mut c_void, data: *const u8, len: u32) -> i32,
    pub state_free: unsafe extern "C" fn(data: *mut u8, len: u32),
    // Latency + tail
    pub get_latency: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    pub get_tail: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    // Output events
    pub get_output_event_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    pub get_output_event:
        unsafe extern "C" fn(ctx: *mut c_void, index: u32, out: *mut Vst3MidiEvent),
    // `SysEx` input. The shim calls this once per `kDataEvent` /
    // `kMidiSysEx` event seen in the host's input event list,
    // before invoking `process`. Bytes are the inner `SysEx`
    // payload - VST3 hosts deliver `DataEvent::bytes` without the
    // `0xF0` / `0xF7` framing per the SDK convention - and are
    // valid only for the duration of this call. The Rust side
    // copies into [`truce_core::EventList::sysex_pool`] so the
    // plug-in's `process()` sees a stable view.
    pub push_sysex_input: unsafe extern "C" fn(
        ctx: *mut c_void,
        sample_offset: u32,
        port: u8,
        bytes: *const u8,
        len: u32,
    ),
    /// Count of `SysEx`-shaped events the plug-in pushed during
    /// `process()`. The shim queries this once after the call to
    /// drain into the host's output event list.
    pub get_output_sysex_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    /// Fill `out_sample_offset`, `out_bytes`, `out_len` with the
    /// index-th `SysEx` output event. Bytes point into the
    /// plug-in's `EventList` pool; valid until the next `process()`
    /// call clears it. The shim copies (via the host's
    /// `IEventList::addEvent`) before that happens.
    pub get_output_sysex_event: unsafe extern "C" fn(
        ctx: *mut c_void,
        index: u32,
        out_sample_offset: *mut u32,
        out_port: *mut u8,
        out_bytes: *mut *const u8,
        out_len: *mut u32,
    ),
    /// Count of per-note MIDI 2.0 events the plug-in pushed that map to
    /// VST3 note expression. The shim drains them into
    /// `kNoteExpressionValueEvent` on the event output bus.
    pub get_output_note_expression_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    /// Fill the index-th note-expression event: `type_id` is the VST3
    /// `NoteExpressionTypeID`, `note_id` correlates to the emitted
    /// `NoteOn`, `value` is normalized `0..=1`, `port` is the event
    /// output bus the correlated note rode - hosts scope `noteId`s per
    /// bus, so an expression on a different bus than its note would
    /// never correlate.
    pub get_output_note_expression: unsafe extern "C" fn(
        ctx: *mut c_void,
        index: u32,
        out_type_id: *mut u32,
        out_note_id: *mut i32,
        out_sample_offset: *mut u32,
        out_value: *mut f64,
        out_port: *mut u8,
    ),
    // GUI
    pub gui_has_editor: unsafe extern "C" fn(ctx: *mut c_void) -> i32,
    pub gui_get_size: unsafe extern "C" fn(ctx: *mut c_void, w: *mut u32, h: *mut u32),
    pub gui_open: unsafe extern "C" fn(ctx: *mut c_void, parent: *mut c_void),
    pub gui_close: unsafe extern "C" fn(ctx: *mut c_void),
    /// Host-driven DPI notification via `IPlugViewContentScaleSupport::
    /// setContentScaleFactor`. Passed to the Rust side so the instance
    /// can remember the host scale (used for physical-pixel size
    /// reporting on Windows/Linux) and forward it to the editor.
    pub gui_set_content_scale: unsafe extern "C" fn(ctx: *mut c_void, scale: f64),
    /// `IPlugView::canResize`. Returns `1` for `kResultTrue` if the
    /// editor advertised `can_resize() == true`, else `0`.
    pub gui_can_resize: unsafe extern "C" fn(ctx: *mut c_void) -> i32,
    /// `IPlugView::checkSizeConstraint`. Clamps the requested
    /// physical width / height in place to the editor's
    /// min / max / aspect constraints. Returns `1` for `kResultOk`.
    /// The Ableton-Live behaviour (host calls this even when
    /// `canResize` is false) is handled host-side here: for fixed
    /// editors we snap to the editor's current size and still
    /// return `kResultOk`.
    pub gui_check_size_constraint: unsafe extern "C" fn(ctx: *mut c_void, w: *mut u32, h: *mut u32),
    /// `IPlugView::onSize`. Host commits a new size; delegate to
    /// `Editor::set_size`. Width / height are in physical pixels;
    /// the Rust side scales to logical using the cached host
    /// content-scale before handing to the editor.
    pub gui_set_size: unsafe extern "C" fn(ctx: *mut c_void, w: u32, h: u32),
    /// `IMidiMapping::getMidiControllerAssignment`. Given the event-input
    /// bus, MIDI channel (0..=15), and a VST3 `ControllerNumbers` value
    /// (`0..=127` CC, `kAfterTouch`/`kPitchBend`/`kCtrlProgramChange`),
    /// resolve the bound parameter from the static `midi_map` metadata.
    /// Writes the id via `out_param_id` and returns `1` (`kResultOk`)
    /// on a hit, `0` (`kResultFalse`) otherwise.
    pub midi_mapping_get_param_id: unsafe extern "C" fn(
        ctx: *mut c_void,
        bus_index: i32,
        channel: i16,
        controller: i16,
        out_param_id: *mut u32,
    ) -> i32,
    /// Process-emitted parameter output. The shim drains these after
    /// `process` into the host's `outputParameterChanges` queue, so
    /// hosts see parameter changes the plugin makes during processing.
    /// `value` is normalized `[0,1]`; `sample_offset` is block-relative.
    pub get_output_param_count: unsafe extern "C" fn(ctx: *mut c_void) -> u32,
    pub get_output_param: unsafe extern "C" fn(
        ctx: *mut c_void,
        index: u32,
        out_id: *mut u32,
        out_sample_offset: *mut i32,
        out_value: *mut f64,
    ),
    /// `IComponent::setActive`. `active != 0` between activate and
    /// deactivate. Lets `cb_state_load` tell whether the audio thread
    /// will drain the pending-state queue (active) or whether it must
    /// apply the custom-state blob synchronously (inactive).
    pub set_active: unsafe extern "C" fn(ctx: *mut c_void, active: i32),
}

unsafe extern "C" {
    /// Register the plugin with the VST3 shim.
    pub fn truce_vst3_register(
        descriptor: *const Vst3PluginDescriptor,
        callbacks: *const Vst3Callbacks,
        param_descriptors: *const Vst3ParamDescriptor,
        num_params: u32,
    );

    /// Get the VST3 factory COM object. Called by `GetPluginFactory`.
    pub fn truce_vst3_get_factory() -> *mut std::ffi::c_void;

    /// Notify host: begin editing a parameter (mouse-down).
    pub fn truce_vst3_begin_edit(ctx: *mut std::ffi::c_void, id: u32);

    /// Notify host: parameter value changed during a gesture.
    pub fn truce_vst3_perform_edit(ctx: *mut std::ffi::c_void, id: u32, normalized: f64);

    /// Notify host: end editing a parameter (mouse-up).
    pub fn truce_vst3_end_edit(ctx: *mut std::ffi::c_void, id: u32);

    /// Plugin -> host resize request. Looks up the owning
    /// component via the ctx mapping, walks to the live plug view's
    /// stored `IPlugFrame*`, and calls `IPlugFrame::resizeView`.
    /// Returns `1` on success; `0` when no live view / frame is
    /// available (e.g. editor closed) or when the host refused.
    /// Routes through the component (not the plug view) so that
    /// view re-creation between calls (Cubase theme change, Live
    /// dock/undock) doesn't leave a stale pointer in the closure.
    pub fn truce_vst3_request_resize(ctx: *mut std::ffi::c_void, w: u32, h: u32) -> i32;

    /// Flag a pending `IComponentHandler::restartComponent`. `flags` is a
    /// VST3 `RestartFlags` bitmask - `kLatencyChanged` (8) for a latency
    /// change. Only sets bits on an atomic, so it is safe to call from the
    /// audio thread; the shim drains them via `restartComponent` on the
    /// next host main-thread callback (param read / edit gesture), keeping
    /// the actual host call on the UI thread.
    pub fn truce_vst3_mark_restart(ctx: *mut std::ffi::c_void, flags: i32);
}
