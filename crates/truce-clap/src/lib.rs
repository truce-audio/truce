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

#[doc(hidden)]
pub mod __macro_deps {
    pub use truce_core;
}

use std::ffi::{CStr, CString, c_char, c_void};
use std::marker::PhantomData;
use std::ptr;
use std::sync::Arc;

use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_IS_LIVE, CLAP_EVENT_MIDI, CLAP_EVENT_NOTE_OFF,
    CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END,
    CLAP_EVENT_PARAM_MOD, CLAP_EVENT_PARAM_VALUE, CLAP_EVENT_TRANSPORT,
    CLAP_TRANSPORT_HAS_BEATS_TIMELINE, CLAP_TRANSPORT_HAS_SECONDS_TIMELINE,
    CLAP_TRANSPORT_HAS_TEMPO, CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_LOOP_ACTIVE,
    CLAP_TRANSPORT_IS_PLAYING, CLAP_TRANSPORT_IS_RECORDING, clap_event_header, clap_event_midi,
    clap_event_note, clap_event_param_gesture, clap_event_param_value, clap_event_transport,
    clap_input_events, clap_output_events,
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
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_plugin_tail};
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

use truce_core::buffer::AudioBuffer;
use truce_core::bus::ChannelConfig;
use truce_core::cast::param_f32;
use truce_core::editor::{ClosureBridge, Editor, PluginContext, RawWindowHandle, SendPtr};
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::{PluginCategory, PluginInfo};
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_core::state;
use truce_params::Params;
use truce_params::{ParamFlags, ParamInfo, ParamRange};

/// Re-export for backward compatibility.
pub use truce_core::export::PluginExport as ClapExport;

// ---------------------------------------------------------------------------
// GUI → host parameter change queue
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum GuiParamChange {
    GestureBegin(u32),
    Value(u32, f64), // (param_id, plain_value)
    GestureEnd(u32),
}

/// Thread-safe queue for GUI-initiated parameter changes.
/// GUI thread pushes, audio/main thread drains.
struct GuiChangeQueue {
    pending: std::sync::Mutex<Vec<GuiParamChange>>,
}

impl GuiChangeQueue {
    fn new() -> Self {
        Self {
            pending: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn push(&self, change: GuiParamChange) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(change);
    }

    fn drain_to(&self, out: &mut Vec<GuiParamChange>) {
        let mut pending = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        out.append(&mut pending);
    }
}

// ---------------------------------------------------------------------------
// Internal wrapper struct held as plugin_data
// ---------------------------------------------------------------------------

struct ClapPluginData<P: PluginExport> {
    /// The user's plugin instance.
    plugin: P,
    /// Re-usable event list for converting CLAP events each process call.
    event_list: EventList,
    /// Re-usable output event list for the process context.
    output_events: EventList,
    /// Cached parameter infos (built once at init).
    param_infos: Vec<ParamInfo>,
    /// Current sample rate.
    sample_rate: f64,
    /// Current max block size.
    max_block_size: usize,
    /// Cached plugin info.
    _info: PluginInfo,
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
    /// Scratch buffer for draining GUI changes (avoids allocation).
    gui_drain_buf: Vec<GuiParamChange>,
    /// Flag: GUI changed params, need rescan on main thread.
    needs_rescan: Arc<std::sync::atomic::AtomicBool>,
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
    /// process callbacks so the audio thread doesn't `Vec::new()` per
    /// block. The 'static lifetime is a structural lie — same trick
    /// `truce_core::buffer::RawBufferScratch` uses; each `process()`
    /// rebuilds the slices and the borrow lives only for that call.
    input_slices: Vec<&'static [f32]>,
    output_slices: Vec<&'static mut [f32]>,
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

/// Install-time override for the plugin's display name in host
/// browsers, set by `cargo truce install` via the `clap_name` field
/// in `truce.toml`. Empty / unset falls back to `PluginInfo::name`.
const CLAP_NAME_OVERRIDE: Option<&'static str> = option_env!("TRUCE_CLAP_NAME_OVERRIDE");

fn resolved_name(info: &PluginInfo) -> &'static str {
    truce_core::info::resolve_name_override(CLAP_NAME_OVERRIDE, info.name)
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
            PluginCategory::Effect => vec![CLAP_PLUGIN_FEATURE_AUDIO_EFFECT],
            PluginCategory::Analyzer => vec![CLAP_PLUGIN_FEATURE_AUDIO_EFFECT],
            PluginCategory::Tool => vec![CLAP_PLUGIN_FEATURE_AUDIO_EFFECT],
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
        dst[i] = b as c_char;
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
//   exclusive access — the host never calls them concurrently.
// - Main-thread callbacks (init, destroy, activate, deactivate,
//   gui_*, params on main thread) are serialized by the host.
// - params_flush may be called from the audio thread while process
//   is not active, or from the main thread — never concurrently
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
        // Drop the ClapPluginData
        let ptr = (*plugin).plugin_data.cast::<ClapPluginData<P>>();
        drop(Box::from_raw(ptr));
        // Drop the clap_plugin itself (we boxed it in create_plugin)
        drop(Box::from_raw(plugin.cast_mut()));
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
        data.max_block_size = max_frames_count as usize;
        data.plugin.reset(sample_rate, max_frames_count as usize);
        data.plugin.params().set_sample_rate(sample_rate);
        data.plugin.params().snap_smoothers();
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
        if data
            .needs_rescan
            .swap(false, std::sync::atomic::Ordering::Relaxed)
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
/// Same flag-driven decoding is needed in two places — the
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
        // CLAP doesn't expose sample-position in transport — tracked
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
unsafe fn convert_input_events<P: PluginExport>(
    data: &mut ClapPluginData<P>,
    in_events: *const clap_input_events,
    sort: bool,
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
                    data.event_list.push(Event {
                        sample_offset,
                        body: EventBody::NoteOn {
                            channel,
                            note,
                            velocity: param_f32(note_event.velocity),
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
                            channel,
                            note,
                            velocity: param_f32(note_event.velocity),
                        },
                    });
                }
                CLAP_EVENT_PARAM_VALUE => {
                    let param_event = &*header.cast::<clap_event_param_value>();
                    // CLAP param values are plain values.
                    // Apply to the params immediately AND push a ParamChange event
                    // so the plugin's process() can react to it.
                    data.plugin
                        .params()
                        .set_plain(param_event.param_id, param_event.value);
                    data.event_list.push(Event {
                        sample_offset,
                        body: EventBody::ParamChange {
                            id: param_event.param_id,
                            value: param_event.value,
                        },
                    });
                }
                CLAP_EVENT_PARAM_MOD => {
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
                    // other format. Mirrors the encoder at the output
                    // path (`process()`'s `EventBody::*` → `clap_event_midi`
                    // arms): without this, hosts that route raw MIDI
                    // (`CLAP_NOTE_DIALECT_MIDI` ports) silently drop
                    // CC / PitchBend / Aftertouch / ChannelPressure /
                    // ProgramChange at the wrapper.
                    let midi = &*header.cast::<clap_event_midi>();
                    let status = midi.data[0];
                    let channel = status & 0x0F;
                    let d1 = midi.data[1];
                    let d2 = midi.data[2];
                    let body = match status & 0xF0 {
                        0x80 => Some(EventBody::NoteOff {
                            channel,
                            note: d1,
                            velocity: f32::from(d2) / 127.0,
                        }),
                        0x90 => {
                            // MIDI 1.0 quirk: NoteOn with velocity 0 = NoteOff.
                            if d2 == 0 {
                                Some(EventBody::NoteOff {
                                    channel,
                                    note: d1,
                                    velocity: 0.0,
                                })
                            } else {
                                Some(EventBody::NoteOn {
                                    channel,
                                    note: d1,
                                    velocity: f32::from(d2) / 127.0,
                                })
                            }
                        }
                        0xA0 => Some(EventBody::Aftertouch {
                            channel,
                            note: d1,
                            pressure: f32::from(d2) / 127.0,
                        }),
                        0xB0 => Some(EventBody::ControlChange {
                            channel,
                            cc: d1,
                            value: f32::from(d2) / 127.0,
                        }),
                        0xC0 => Some(EventBody::ProgramChange {
                            channel,
                            program: d1,
                        }),
                        0xD0 => Some(EventBody::ChannelPressure {
                            channel,
                            pressure: f32::from(d1) / 127.0,
                        }),
                        0xE0 => {
                            // 14-bit unsigned 0..16383 → signed [-1, 1]
                            // with 8192 = center. Mirrors the encoder.
                            let n = (u16::from(d2) << 7) | u16::from(d1);
                            let v = (f32::from(n) - 8192.0) / 8192.0;
                            Some(EventBody::PitchBend {
                                channel,
                                value: v.clamp(-1.0, 1.0),
                            })
                        }
                        _ => None,
                    };
                    if let Some(body) = body {
                        data.event_list.push(Event {
                            sample_offset,
                            body,
                        });
                    }
                }
                _ => {
                    // Unsupported event type (system real-time, sysex,
                    // MIDI 2.0) — skip silently. MIDI 2.0 demux is a
                    // future extension if we add `EventBody::*2` input
                    // support.
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

        data.gui_drain_buf.clear();
        data.gui_changes.drain_to(&mut data.gui_drain_buf);

        for change in &data.gui_drain_buf {
            match *change {
                GuiParamChange::GestureBegin(id) => {
                    let event = clap_event_param_gesture {
                        header: clap_event_header {
                            size: truce_core::cast::size_of_u32::<clap_event_param_gesture>(),
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
                            size: truce_core::cast::size_of_u32::<clap_event_param_value>(),
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
                            size: truce_core::cast::size_of_u32::<clap_event_param_gesture>(),
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
        // Reclaim memory after a burst of GUI gestures (automation
        // pass, MIDI-learn drag) so the buffer doesn't hold its
        // high-water capacity for the plugin's lifetime. 64 events
        // covers the steady-state per-block load (≤ 1 gesture begin
        // + setting + end per visible widget) without rallocating
        // on every flush.
        data.gui_drain_buf.shrink_to(64);
    }
}

// ---------------------------------------------------------------------------
// Process callback
// ---------------------------------------------------------------------------

unsafe extern "C" fn clap_plugin_process<P: PluginExport>(
    plugin: *const clap_plugin,
    process: *const clap_process,
) -> i32 {
    unsafe {
        if process.is_null() {
            return CLAP_PROCESS_ERROR;
        }

        let proc = &*process;
        let data = data_from_plugin::<P>(plugin);
        let num_frames = proc.frames_count as usize;

        if num_frames == 0 {
            return CLAP_PROCESS_CONTINUE;
        }

        // Convert CLAP input events to our EventList — sort by
        // sample offset so the plugin sees them in time order.
        convert_input_events::<P>(data, proc.in_events, true);

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
        //    rather than being dropped — preserving channel layout
        //    avoids the silent re-mapping the densifying loop used to
        //    produce when only some channels were null.
        // 3. **No auto input→output copy.** Earlier revisions copied
        //    each input into the matching output as a "convenience for
        //    in-place effects"; that clobbered the previous-block tail
        //    of any plugin reading its own output (delay/reverb
        //    feedback). Plugins that want pass-through must do
        //    `output.copy_from_slice(input)` themselves.
        debug_assert!(
            num_frames <= data.max_block_size,
            "host violated CLAP contract: process() got {num_frames} frames \
             but activate() declared max {}",
            data.max_block_size
        );

        data.input_slices.clear();
        for bus_idx in 0..proc.audio_inputs_count {
            let buf = &*proc.audio_inputs.add(bus_idx as usize);
            if buf.data32.is_null() {
                continue;
            }
            for ch in 0..buf.channel_count {
                let ptr = *buf.data32.add(ch as usize);
                let slice: &[f32] = if ptr.is_null() {
                    &[]
                } else {
                    std::slice::from_raw_parts(ptr, num_frames)
                };
                data.input_slices
                    .push(std::mem::transmute::<&[f32], &'static [f32]>(slice));
            }
        }

        data.output_slices.clear();
        for bus_idx in 0..proc.audio_outputs_count {
            let buf = &mut *proc.audio_outputs.add(bus_idx as usize);
            if buf.data32.is_null() {
                continue;
            }
            for ch in 0..buf.channel_count {
                let ptr = *buf.data32.add(ch as usize);
                let slice: &mut [f32] = if ptr.is_null() {
                    &mut []
                } else {
                    std::slice::from_raw_parts_mut(ptr, num_frames)
                };
                data.output_slices
                    .push(std::mem::transmute::<&mut [f32], &'static mut [f32]>(slice));
            }
        }

        // Construct the AudioBuffer with a borrow scope tied to this
        // call only. Without the transmute, the borrow checker
        // propagates the `'static` lifetimes inside `input_slices`
        // out to the AudioBuffer's lifetime parameter — which would
        // pin `data` mutably for the rest of the function. Same
        // pattern as `RawBufferScratch::build`.
        let data_ptr: *mut ClapPluginData<P> = data;
        let s = &mut *data_ptr;
        let mut audio_buffer = std::mem::transmute::<AudioBuffer<'static>, AudioBuffer<'_>>(
            AudioBuffer::from_slices(&s.input_slices, &mut s.output_slices, num_frames),
        );

        data.output_events.clear();

        // Publish transport to the editor slot before the plugin runs.
        data.transport_slot.write(&transport);

        let mut context = ProcessContext::new(
            &transport,
            data.sample_rate,
            num_frames,
            &mut data.output_events,
        );

        let status = data
            .plugin
            .process(&mut audio_buffer, &data.event_list, &mut context);

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
                    } => {
                        let ev = clap_event_note {
                            header: clap_event_header {
                                size: truce_core::cast::size_of_u32::<clap_event_note>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_NOTE_ON,
                                flags: 0,
                            },
                            note_id: -1,
                            port_index: 0,
                            channel: i16::from(*channel),
                            key: i16::from(*note),
                            velocity: f64::from(*velocity),
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    EventBody::NoteOff {
                        channel,
                        note,
                        velocity,
                    } => {
                        let ev = clap_event_note {
                            header: clap_event_header {
                                size: truce_core::cast::size_of_u32::<clap_event_note>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_NOTE_OFF,
                                flags: 0,
                            },
                            note_id: -1,
                            port_index: 0,
                            channel: i16::from(*channel),
                            key: i16::from(*note),
                            velocity: f64::from(*velocity),
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    // CLAP carries MIDI 1.0 control / channel events as
                    // `CLAP_EVENT_MIDI` 3-byte packets. The host
                    // demuxes them on the receiving side; we just
                    // build the standard MIDI status byte and pass
                    // the data bytes through.
                    EventBody::ControlChange { channel, cc, value } => {
                        let v = truce_core::cast::midi_7bit(*value);
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: truce_core::cast::size_of_u32::<clap_event_midi>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI,
                                flags: 0,
                            },
                            port_index: 0,
                            data: [0xB0 | (channel & 0x0F), *cc, v],
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    EventBody::Aftertouch {
                        channel,
                        note,
                        pressure,
                    } => {
                        let p = truce_core::cast::midi_7bit(*pressure);
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: truce_core::cast::size_of_u32::<clap_event_midi>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI,
                                flags: 0,
                            },
                            port_index: 0,
                            data: [0xA0 | (channel & 0x0F), *note, p],
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    EventBody::ChannelPressure { channel, pressure } => {
                        let p = truce_core::cast::midi_7bit(*pressure);
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: truce_core::cast::size_of_u32::<clap_event_midi>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_MIDI,
                                flags: 0,
                            },
                            port_index: 0,
                            data: [0xD0 | (channel & 0x0F), p, 0],
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    EventBody::PitchBend { channel, value } => {
                        // 14-bit signed [-1, 1] → unsigned 0..16383 with
                        // 8192 = center. LSB first per MIDI spec.
                        let n = truce_core::cast::midi_14bit_pb(*value);
                        // Bit-extraction: `& 0x7F` already constrains
                        // each result to the low 7 bits.
                        #[allow(clippy::cast_possible_truncation)]
                        let lsb = (n & 0x7F) as u8;
                        #[allow(clippy::cast_possible_truncation)]
                        let msb = ((n >> 7) & 0x7F) as u8;
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: truce_core::cast::size_of_u32::<clap_event_midi>(),
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
                    EventBody::ProgramChange { channel, program } => {
                        let ev = clap_event_midi {
                            header: clap_event_header {
                                size: truce_core::cast::size_of_u32::<clap_event_midi>(),
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
                        let ev = clap_event_param_value {
                            header: clap_event_header {
                                size: truce_core::cast::size_of_u32::<clap_event_param_value>(),
                                time: event.sample_offset,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_PARAM_VALUE,
                                flags: 0,
                            },
                            param_id: *id,
                            cookie: ptr::null_mut(),
                            note_id: -1,
                            port_index: 0,
                            channel: -1,
                            key: -1,
                            value: *value,
                        };
                        try_push(proc.out_events, &raw const ev.header);
                    }
                    // MIDI 2.0, ParamMod, Transport, and per-note
                    // events: the plugin-output direction isn't
                    // routinely emitted by truce plugins; leave them
                    // as silent skips rather than building partial
                    // encoders.
                    _ => {}
                }
            }
        }

        match status {
            ProcessStatus::Normal => CLAP_PROCESS_CONTINUE,
            ProcessStatus::Tail(0) => CLAP_PROCESS_SLEEP,
            ProcessStatus::Tail(_) => CLAP_PROCESS_TAIL,
            ProcessStatus::KeepAlive => CLAP_PROCESS_CONTINUE_IF_NOT_QUIET,
        }
    }
}

// ---------------------------------------------------------------------------
// Extension: params
// ---------------------------------------------------------------------------

unsafe extern "C" fn params_count<P: PluginExport>(plugin: *const clap_plugin) -> u32 {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        truce_core::cast::len_u32(data.param_infos.len())
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
        match data.plugin.params().get_plain(param_id) {
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
        match data.plugin.params().format_value(param_id, value) {
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
        match data.plugin.params().parse_value(param_id, text) {
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
        convert_input_events::<P>(data, in_events, false);
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
        let (ids, values) = data.plugin.params().collect_values();
        let extra = data.plugin.save_state();
        let blob = state::serialize_state(data.plugin_id_hash, &ids, &values, extra.as_deref());

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

        data.plugin.params().restore_values(&deserialized.params);
        data.plugin.params().snap_smoothers();

        if let Some(extra) = &deserialized.extra {
            data.plugin.load_state(extra);
        }

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
        truce_core::cast::len_u32(layout.inputs.len())
    } else {
        truce_core::cast::len_u32(layout.outputs.len())
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
use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_plugin_gui, clap_window};

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
        // Create the editor from the plugin
        data.editor = data.plugin.editor();
        data.gui_created = data.editor.is_some();
        data.gui_created
    }
}

unsafe extern "C" fn gui_destroy<P: PluginExport>(plugin: *const clap_plugin) {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        if let Some(ref mut editor) = data.editor {
            editor.close();
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
                // Round-to-nearest, not truncate — `(w * scale) as u32`
                // would round 199.9 → 199, drifting one pixel on
                // fractional scales. Matches VST3 / AAX / the
                // `to_physical_px` helper used elsewhere.
                *width = (w as f64 * data.host_scale).round() as u32;
                *height = (h as f64 * data.host_scale).round() as u32;
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
                eprintln!("[truce-clap] gui_set_parent panicked: {msg}");
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
        // by the host's plugin slot — outlives the editor. Params fields are
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
            // `clap_host_params` extension (the spec marks it
            // optional). Earlier revisions dereferenced it
            // unconditionally and crashed any host that didn't
            // implement params. The `clap_plugin_on_main_thread` path
            // checks the same way (line 364).
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
        let transport_slot = data.transport_slot.clone();
        let context = PluginContext::from_closures(
            ClosureBridge {
                begin_edit: Box::new(move |id| {
                    gui_changes.push(GuiParamChange::GestureBegin(id));
                    request_flush();
                }),
                set_param: Box::new(move |id, value| {
                    let plain = params_for_set.set_normalized_returning_plain(id, value);
                    gui_changes2.push(GuiParamChange::Value(id, plain));
                    request_flush2();
                    // Symmetry with the host-pointer null guards at
                    // `:297, :365, :1404`. CLAP guarantees a valid
                    // host on init, but a host that creates a plugin
                    // without ever calling `clap_plugin_init`-style
                    // setup (rare validators) could leave `data.host`
                    // null; the deref below would crash inside the
                    // GUI thread.
                    let host_ptr = host_for_callback.as_ptr();
                    if !needs_rescan.swap(true, std::sync::atomic::Ordering::Relaxed)
                        && !host_ptr.is_null()
                        && let Some(req_cb) = (*host_ptr).request_callback
                    {
                        req_cb(host_ptr);
                    }
                }),
                end_edit: Box::new(move |id| {
                    gui_changes3.push(GuiParamChange::GestureEnd(id));
                    request_flush3();
                }),
                request_resize: Box::new(|_w, _h| false),
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

        #[cfg(target_os = "macos")]
        let handle = RawWindowHandle::AppKit(parent_ptr);
        #[cfg(target_os = "windows")]
        let handle = RawWindowHandle::Win32(parent_ptr);
        #[cfg(target_os = "linux")]
        let handle = RawWindowHandle::X11(parent_ptr as u64);

        editor.open(handle, context);
        true
    }
}

unsafe extern "C" fn gui_show<P: PluginExport>(_plugin: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn gui_hide<P: PluginExport>(_plugin: *const clap_plugin) -> bool {
    true
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
        get_resize_hints: None,
        adjust_size: None,
        set_size: None,
        set_parent: Some(gui_set_parent::<P>),
        set_transient: None,
        suggest_title: None,
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
        data.plugin.latency()
    }
}

unsafe extern "C" fn tail_get<P: PluginExport>(plugin: *const clap_plugin) -> u32 {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        data.plugin.tail()
    }
}

impl<P: PluginExport> Extensions<P> {
    fn new() -> Self {
        Self {
            params: make_params_extension::<P>(),
            state: make_state_extension::<P>(),
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
    /// `usize` because Rust forbids generic statics — a literal
    /// `OnceLock<Extensions<P>>` static can't reference the outer
    /// generic parameter, so we erase to `usize` and re-attach the
    /// type on read. `OnceLock::get_or_init` runs the constructor at
    /// most once across all threads, so unlike the previous
    /// `AtomicPtr<u8>` + manual `compare_exchange` shape we never
    /// build-and-throw-away a losing `Box` on a race.
    ///
    /// CLAP libraries only ship one plugin type per shared object, so
    /// there's exactly one monomorphization and one `OnceLock` per
    /// binary in practice.
    fn get() -> &'static Self {
        use std::sync::OnceLock;
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

    let data = Box::new(ClapPluginData::<P> {
        plugin: instance,
        event_list: EventList::new(),
        output_events: EventList::new(),
        param_infos,
        sample_rate: 44100.0,
        max_block_size: 1024,
        _info: info,
        plugin_id_hash,
        editor: None,
        gui_created: false,
        host,
        host_params: ptr::null(),
        gui_changes: Arc::new(GuiChangeQueue::new()),
        gui_drain_buf: Vec::new(),
        needs_rescan: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        transport_slot: truce_core::TransportSlot::new(),
        host_scale: 1.0,
        host_scale_set_by_host: false,
        input_slices: Vec::new(),
        output_slices: Vec::new(),
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
            use ::clap_sys::host::clap_host;
            use ::clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
            use ::clap_sys::version::CLAP_VERSION;

            use ::truce_clap::__macro_deps::truce_core::plugin::Plugin;
            use ::truce_clap::DescriptorHolder;

            static DESCRIPTOR: OnceLock<DescriptorHolder> = OnceLock::new();

            fn get_descriptor() -> &'static DescriptorHolder {
                DESCRIPTOR.get_or_init(|| {
                    let info = <$plugin_type as Plugin>::info();
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
                let info = <$plugin_type as Plugin>::info();
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

            unsafe extern "C" fn entry_init(_plugin_path: *const c_char) -> bool {
                // Force descriptor initialization.
                let _ = get_descriptor();
                true
            }

            unsafe extern "C" fn entry_deinit() {}

            unsafe extern "C" fn entry_get_factory(factory_id: *const c_char) -> *const c_void {
                if factory_id.is_null() {
                    return ptr::null();
                }
                let id = CStr::from_ptr(factory_id);
                if id == CLAP_PLUGIN_FACTORY_ID {
                    &FACTORY as *const clap_plugin_factory as *const c_void
                } else {
                    ptr::null()
                }
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
