//! Audio Unit v3 format wrapper for truce.
//!
//! Uses an Objective-C shim compiled via `cc` that implements the
//! `AUAudioUnit` subclass. The shim calls back into Rust for all
//! plugin logic via C FFI.

pub mod ffi;

use std::ffi::CString;
use std::os::raw::c_char;
use std::slice;

use truce_core::cast::{len_u32, param_f32, sample_pos_i64};
use truce_core::editor::{ClosureBridge, Editor, PluginContext, RawWindowHandle, SendPtr};
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_core::process::ProcessContext;
use truce_core::state;
use truce_params::{ParamFlags, Params};

use ffi::{AuCallbacks, AuMidiEvent, AuParamDescriptor, AuPluginDescriptor, AuTransportSnapshot};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Instance wrapper — one per plugin instance, stored as the opaque ctx
// ---------------------------------------------------------------------------

struct AuInstance<P: PluginExport> {
    plugin: P,
    event_list: EventList,
    output_events: EventList,
    plugin_id_hash: u64,
    sample_rate: f64,
    /// Max block size declared by the host via
    /// `kAudioUnitProperty_MaximumFramesPerSlice` (delivered through
    /// `cb_reset`'s `max_frames`).
    max_block_size: usize,
    /// Reused per-block scratch for `RawBufferScratch::build`. Lives
    /// on the instance so the audio thread doesn't heap-allocate.
    scratch: truce_core::buffer::RawBufferScratch,
    editor: Option<Box<dyn Editor>>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    transport_slot: Arc<truce_core::TransportSlot>,
}

// ---------------------------------------------------------------------------
// Intentional leaks
//
// Every `CString::into_raw()` and `Vec::leak()` / `param_descs.leak()`
// in this file feeds a `*const c_char` (or `*const SomeDesc`) into a
// descriptor that the AU host caches for the process lifetime. Hosts
// re-read these pointers on demand (display, parameter sweeps,
// validation) — there's no signal back to Rust saying "you may free
// this now". Freeing is therefore unsound.
//
// The leak is bounded: O(plugin_count × (param_count + a few strings))
// per process, allocated once at registration time. No leak per audio
// callback, per render, per editor open. AU bundles get unloaded with
// the host process, which reclaims the allocation.
//
// `Box::into_raw(boxed_instance)` in `cb_create` follows the same
// pattern but is *paired* with `cb_destroy` reconstituting the Box —
// so it isn't a leak, just a C-lifetime handoff.
//
// ---------------------------------------------------------------------------
// C callback implementations (generic over P)
//
// SAFETY for all unsafe extern "C" fn below:
// - `ctx` is a *mut c_void created by Box::into_raw in cb_create().
//   Valid until cb_destroy() (called exactly once by the AU shim).
// - The AU v2 shim (au_v2_shim.c) and v3 shim (au_shim.m) own the
//   Rust context. The AU host guarantees: render callback on the
//   audio thread with exclusive access; all other callbacks on the
//   main thread, serialized.
// - Audio buffer pointers come from the host's AudioBufferList and
//   are valid for the declared channel count × frame count.
// - MIDI events come from MusicDeviceMIDIEvent (v2) or
//   AURenderEvent linked list (v3).
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_create<P: PluginExport>() -> *mut std::ffi::c_void {
    let mut plugin = P::create();
    plugin.init();
    let info = P::info();
    let instance = Box::new(AuInstance::<P> {
        plugin,
        event_list: EventList::new(),
        output_events: EventList::new(),
        plugin_id_hash: state::shared_plugin_state_hash(&info),
        sample_rate: 44100.0,
        max_block_size: 0,
        scratch: truce_core::buffer::RawBufferScratch::default(),
        editor: None,
        transport_slot: truce_core::TransportSlot::new(),
    });
    Box::into_raw(instance).cast::<std::ffi::c_void>()
}

unsafe extern "C" fn cb_destroy<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    unsafe {
        if !ctx.is_null() {
            drop(Box::from_raw(ctx.cast::<AuInstance<P>>()));
        }
    }
}

unsafe extern "C" fn cb_reset<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    sample_rate: f64,
    max_frames: u32,
) {
    unsafe {
        let inst = &mut *ctx.cast::<AuInstance<P>>();
        inst.sample_rate = sample_rate;
        inst.max_block_size = max_frames as usize;
        inst.plugin.reset(sample_rate, max_frames as usize);
        inst.plugin.params().set_sample_rate(sample_rate);
        inst.plugin.params().snap_smoothers();
    }
}

unsafe extern "C" fn cb_process<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    inputs: *const *const f32,
    outputs: *mut *mut f32,
    num_input_channels: u32,
    num_output_channels: u32,
    num_frames: u32,
    events: *const AuMidiEvent,
    num_events: u32,
    transport_ptr: *const AuTransportSnapshot,
) {
    unsafe {
        let inst = &mut *ctx.cast::<AuInstance<P>>();
        let num_frames = num_frames as usize;

        // Convert MIDI events
        inst.event_list.clear();
        if !events.is_null() && num_events > 0 {
            let event_slice = slice::from_raw_parts(events, num_events as usize);
            for ev in event_slice {
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
                        sample_offset: ev.sample_offset,
                        body,
                    });
                }
            }
        }
        inst.event_list.sort();

        // Build AudioBuffer from raw pointers, reusing the per-instance scratch.
        debug_assert!(
            num_frames <= inst.max_block_size,
            "host violated AU contract: render() got {num_frames} frames \
             but kAudioUnitProperty_MaximumFramesPerSlice declared max {}",
            inst.max_block_size
        );
        let mut audio_buffer = inst.scratch.build(
            inputs,
            outputs,
            num_input_channels,
            num_output_channels,
            len_u32(num_frames),
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
            .process(&mut audio_buffer, &inst.event_list, &mut context);
    }
}

unsafe extern "C" fn cb_param_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        len_u32(inst.plugin.params().count())
    }
}

unsafe extern "C" fn cb_param_get_descriptor<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out: *mut AuParamDescriptor,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let infos = inst.plugin.params().param_infos();
        if let Some(info) = infos.get(index as usize) {
            // Store strings in leaked CStrings (they live for the process lifetime)
            let name = CString::new(info.name).unwrap_or_default();
            let unit = CString::new(info.unit.as_str()).unwrap_or_default();
            let group = CString::new(info.group).unwrap_or_default();

            let desc = &mut *out;
            desc.id = info.id;
            desc.name = name.into_raw();
            desc.min = info.range.min();
            desc.max = info.range.max();
            desc.default_value = info.default_plain;
            desc.step_count = info.range.step_count().map_or(0, std::num::NonZero::get);
            desc.unit = unit.into_raw();
            desc.group = group.into_raw();
        }
    }
}

unsafe extern "C" fn cb_param_get_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
) -> f64 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        inst.plugin.params().get_plain(id).unwrap_or(0.0)
    }
}

unsafe extern "C" fn cb_param_set_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        inst.plugin.params().set_plain(id, value);
    }
}

unsafe extern "C" fn cb_param_format_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
    out: *mut c_char,
    out_len: u32,
) -> u32 {
    unsafe {
        // `out_len == 0` would underflow on `out_len as usize - 1`
        // and let `copy_nonoverlapping` write past the host-supplied
        // buffer. Treat zero capacity as "host wants nothing".
        if out_len == 0 || out.is_null() {
            return 0;
        }
        let inst = &*ctx.cast::<AuInstance<P>>();
        match inst.plugin.params().format_value(id, value) {
            Some(text) => {
                let bytes = text.as_bytes();
                let len = bytes.len().min((out_len as usize) - 1);
                std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out, len);
                *out.add(len) = 0;
                len_u32(len)
            }
            None => 0,
        }
    }
}

unsafe extern "C" fn cb_state_save<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    out_data: *mut *mut u8,
    out_len: *mut u32,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let (ids, values) = inst.plugin.params().collect_values();
        let extra = inst.plugin.save_state();
        let blob = state::serialize_state(inst.plugin_id_hash, &ids, &values, extra.as_deref());

        let len = blob.len();
        let ptr = malloc(len).cast::<u8>();
        if ptr.is_null() {
            // malloc failed — tell the host we wrote nothing rather
            // than leaving `*out_data` as a stale value while
            // `*out_len = len` claims `len` bytes are there.
            *out_data = std::ptr::null_mut();
            *out_len = 0;
            return;
        }
        std::ptr::copy_nonoverlapping(blob.as_ptr(), ptr, len);
        *out_data = ptr;
        *out_len = len_u32(len);
    }
}

unsafe extern "C" fn cb_state_load<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    data: *const u8,
    len: u32,
) {
    unsafe {
        let inst = &mut *ctx.cast::<AuInstance<P>>();
        // `slice::from_raw_parts(null, n)` for `n > 0` is UB. Treat
        // `(null, *)` and `(_, 0)` the same as "host gave us nothing".
        if data.is_null() || len == 0 {
            return;
        }
        let blob = slice::from_raw_parts(data, len as usize);
        if let Some(deserialized) = state::deserialize_state(blob, inst.plugin_id_hash) {
            inst.plugin.params().restore_values(&deserialized.params);
            inst.plugin.params().snap_smoothers();
            if let Some(extra) = &deserialized.extra {
                inst.plugin.load_state(extra);
            }
            if let Some(ref mut editor) = inst.editor {
                editor.state_changed();
            }
        }
    }
}

unsafe extern "C" fn cb_state_free(data: *mut u8, _len: u32) {
    unsafe {
        if !data.is_null() {
            free(data.cast::<std::ffi::c_void>());
        }
    }
}

// ---------------------------------------------------------------------------
// Output event callbacks (plugin → host MIDI)
// ---------------------------------------------------------------------------

/// Map a truce `Event` body to a 3-byte AU MIDI packet. Returns
/// `None` for event types that don't fit (MIDI 2.0, `ParamChange`,
/// Transport, etc.). Mirrors the VST2/VST3 encoders.
fn try_encode_au_midi(event: &Event) -> Option<AuMidiEvent> {
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
        EventBody::ControlChange { channel, cc, value } => (
            0xB0 | (channel & 0x0F),
            *cc,
            truce_core::cast::midi_7bit(*value),
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
    Some(AuMidiEvent {
        sample_offset: event.sample_offset,
        status,
        data1,
        data2,
        _pad: 0,
    })
}

unsafe extern "C" fn cb_output_event_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let n = inst.output_events
            .iter()
            .filter(|e| try_encode_au_midi(e).is_some())
            .count();
        len_u32(n)
    }
}

unsafe extern "C" fn cb_output_event_at<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    index: u32,
    out: *mut AuMidiEvent,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        if let Some(packet) = inst
            .output_events
            .iter()
            .filter_map(try_encode_au_midi)
            .nth(index as usize)
        {
            *out = packet;
        }
    }
}

// ---------------------------------------------------------------------------
// GUI callbacks
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_gui_has_editor<P: PluginExport>(ctx: *mut std::ffi::c_void) -> i32 {
    unsafe {
        if ctx.is_null() {
            return 0;
        }
        let inst = &mut *ctx.cast::<AuInstance<P>>();
        if inst.editor.is_none() {
            inst.editor = inst.plugin.editor();
        }
        i32::from(inst.editor.is_some())
    }
}

unsafe extern "C" fn cb_gui_get_size<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    w: *mut u32,
    h: *mut u32,
) {
    unsafe {
        if ctx.is_null() {
            return;
        }
        // Lazily install the editor here too — some AU validators
        // (`auval`, Logic Pro's plugin validator) call `..._get_size`
        // before `..._has_editor`, which is the canonical install
        // site. Without this, those validators saw `inst.editor ==
        // None` and silently received a 0×0 view, which shows up as
        // "plugin reports invalid size" in their reports. Mirrors
        // `cb_gui_has_editor`'s `&mut *` install.
        let inst = &mut *ctx.cast::<AuInstance<P>>();
        if inst.editor.is_none() {
            inst.editor = inst.plugin.editor();
        }
        if let Some(ref editor) = inst.editor {
            // AU is macOS-only; hosts embed our NSView inside a Cocoa
            // container at logical-point coordinates and AppKit handles
            // the Retina backing transparently. Report the editor size
            // as-is — no scaling.
            let (ew, eh) = editor.size();
            *w = ew;
            *h = eh;
        }
    }
}

unsafe extern "C" fn cb_gui_open<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    parent: *mut std::ffi::c_void,
) {
    unsafe {
        let inst = &mut *ctx.cast::<AuInstance<P>>();
        if let Some(ref mut editor) = inst.editor {
            let params = inst.plugin.params_arc();
            let plugin_ptr = SendPtr::new(&raw const inst.plugin);
            let ctx_raw = SendPtr::new(ctx);
            let params_for_set = params.clone();
            let params_for_get = params.clone();
            let params_for_plain = params.clone();
            let params_for_fmt = params.clone();
            let params_for_ctx = params.clone();
            let transport_slot = inst.transport_slot.clone();
            let ctx_for_begin = ctx_raw;
            let ctx_for_end = ctx_raw;
            let context = PluginContext::from_closures(
                ClosureBridge {
                    begin_edit: Box::new(move |id| {
                        // Broadcasts kAudioUnitEvent_BeginParameterChangeGesture
                        // via AUEventListenerNotify so hosts (Logic, Live,
                        // Reaper) group subsequent set_param calls into one
                        // undo step and one automation gesture.
                        truce_au_v2_host_begin_param_gesture(
                            ctx_for_begin.as_ptr().cast_mut(),
                            id,
                        );
                    }),
                    set_param: Box::new(move |id, value| {
                        // One combined trait dispatch (set_normalized
                        // + get_plain) instead of two — the
                        // `#[derive(Params)]` impl can compute both in
                        // a single match-arm walk.
                        let plain = param_f32(params_for_set.set_normalized_returning_plain(id, value));
                        truce_au_v2_host_set_param(
                            ctx_raw.as_ptr().cast_mut(),
                            id,
                            plain,
                        );
                    }),
                    end_edit: Box::new(move |id| {
                        // Closes the gesture started by begin_edit so the
                        // host commits the undo group / stops automation
                        // recording.
                        truce_au_v2_host_end_param_gesture(
                            ctx_for_end.as_ptr().cast_mut(),
                            id,
                        );
                    }),
                    request_resize: Box::new(|_w, _h| false),
                    get_param: Box::new(move |id| params_for_get.get_normalized(id).unwrap_or(0.0)),
                    get_param_plain: Box::new(move |id| {
                        params_for_plain.get_plain(id).unwrap_or(0.0)
                    }),
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
            let handle = RawWindowHandle::AppKit(parent);
            editor.open(handle, context);
        }
    }
}

unsafe extern "C" fn cb_gui_close<P: PluginExport>(ctx: *mut std::ffi::c_void) {
    unsafe {
        let inst = &mut *ctx.cast::<AuInstance<P>>();
        if let Some(ref mut editor) = inst.editor {
            editor.close();
        }
        // Keep the editor alive — just closed, not dropped.
        //
        // Dropping the editor here would synchronously deallocate its
        // baseview NSWindow + content NSView. Logic / Pro Tools tend
        // to call `gui_close` from inside their own NSTimer fire or
        // a `[CALayer display]` callback, both of which run inside an
        // implicit autorelease pool that's about to pop. If
        // `[NSTimer invalidate]` (which baseview's drop chain calls
        // via `WindowHandle::drop`) re-enters that pool's pop
        // sequence, the host crashes inside `objc_release` on a
        // freed `NSAutoreleasePool*` (same root cause as the AAX
        // incident in `aax_editor_crash` memory note). The editor's
        // `close()` has already released the NSView contents and
        // Metal resources; the lightweight Rust struct that survives
        // is reopened in-place by the next `gui_open` call.
    }
}

unsafe extern "C" {
    fn malloc(size: usize) -> *mut std::ffi::c_void;
    fn free(ptr: *mut std::ffi::c_void);
    fn truce_au_v2_host_set_param(ctx: *mut std::ffi::c_void, param_id: u32, value: f32);
    fn truce_au_v2_host_begin_param_gesture(ctx: *mut std::ffi::c_void, param_id: u32);
    fn truce_au_v2_host_end_param_gesture(ctx: *mut std::ffi::c_void, param_id: u32);
}

// ---------------------------------------------------------------------------
// Registration: called once from the export_au! macro
// ---------------------------------------------------------------------------

/// Register the plugin with the AU system. Must be called once at load time
/// (typically from a constructor function generated by `export_au!`).
/// Install-time override for the host-facing AU display name.
/// Populated by `cargo truce install` from the `au_name` (AU v2) or
/// `au3_name` (AU v3) field in `truce.toml`; each build is targeted
/// to one AU version so one env var covers both.
const AU_NAME_OVERRIDE: Option<&'static str> = option_env!("TRUCE_AU_NAME_OVERRIDE");

fn resolved_plugin_name(info: &truce_core::info::PluginInfo) -> &'static str {
    truce_core::info::resolve_name_override(AU_NAME_OVERRIDE, info.name)
}

pub fn register_au<P: PluginExport>() {
    let info = P::info();

    // Static metadata path: derive emits a `LazyLock`-cached
    // `Vec<ParamInfo>` so registration doesn't construct a plugin
    // instance just to read parameter shape. Hand-written
    // `PluginExport` impls without a `Params::param_infos_static`
    // override fall back to the historical
    // `Self::create().params().param_infos()` walk inside the trait
    // default — see `PluginExport::param_infos_static`.
    let param_infos = P::param_infos_static();
    let mut param_descs: Vec<AuParamDescriptor> = Vec::with_capacity(param_infos.len());

    for pi in &param_infos {
        let cs = truce_core::wrapper::ParamCStrings::from_info(pi);
        param_descs.push(AuParamDescriptor {
            id: pi.id,
            name: cs.name.into_raw(),
            min: pi.range.min(),
            max: pi.range.max(),
            default_value: pi.default_plain,
            step_count: pi.range.step_count().map_or(0, std::num::NonZero::get),
            unit: cs.unit.into_raw(),
            group: cs.group.into_raw(),
        });
    }

    let name = CString::new(resolved_plugin_name(&info)).unwrap_or_default();
    let vendor = CString::new(info.vendor).unwrap_or_default();

    let bypass_param_id = param_infos
        .iter()
        .find(|pi| pi.flags.contains(ParamFlags::IS_BYPASS))
        .map_or(u32::MAX, |pi| pi.id);

    // NoteEffect plugins (arpeggiators, chord generators) emit MIDI
    // back to the host. Instruments could in theory too but it's rare
    // and we don't want to advertise a "MIDI Out" port in every synth's
    // host UI without an explicit opt-in. Effects and analyzers never do.
    let has_midi_output = i32::from(matches!(info.category, PluginCategory::NoteEffect));

    let descriptor = Box::leak(Box::new(AuPluginDescriptor {
        component_type: info.au_type,
        component_subtype: info.fourcc,
        component_manufacturer: info.au_manufacturer,
        name: name.into_raw(),
        vendor: vendor.into_raw(),
        version: 0x0001_0000, // 1.0.0
        num_inputs: truce_core::wrapper::default_io_channels::<P>().0,
        num_outputs: truce_core::wrapper::default_io_channels::<P>().1,
        bypass_param_id,
        has_midi_output,
    }));

    let callbacks = Box::leak(Box::new(AuCallbacks {
        create: cb_create::<P>,
        destroy: cb_destroy::<P>,
        reset: cb_reset::<P>,
        process: cb_process::<P>,
        param_count: cb_param_count::<P>,
        param_get_descriptor: cb_param_get_descriptor::<P>,
        param_get_value: cb_param_get_value::<P>,
        param_set_value: cb_param_set_value::<P>,
        param_format_value: cb_param_format_value::<P>,
        state_save: cb_state_save::<P>,
        state_load: cb_state_load::<P>,
        state_free: cb_state_free,
        output_event_count: cb_output_event_count::<P>,
        output_event_at: cb_output_event_at::<P>,
        gui_has_editor: cb_gui_has_editor::<P>,
        gui_get_size: cb_gui_get_size::<P>,
        gui_open: cb_gui_open::<P>,
        gui_close: cb_gui_close::<P>,
    }));

    let param_descs = param_descs.leak();

    unsafe {
        ffi::truce_au_register(
            std::ptr::from_ref::<AuPluginDescriptor>(descriptor),
            std::ptr::from_ref::<AuCallbacks>(callbacks),
            param_descs.as_ptr(),
            len_u32(param_descs.len()),
        );
    }
}

// ---------------------------------------------------------------------------
// export_au! macro
// ---------------------------------------------------------------------------

/// Export an Audio Unit v3 plugin entry point.
///
/// Usage:
/// ```ignore
/// export_au!(MyPlugin);
/// ```
///
/// Where `MyPlugin` implements `PluginExport`.
#[macro_export]
macro_rules! export_au {
    ($plugin_type:ty) => {
        #[cfg(target_os = "macos")]
        mod _au_entry {
            use super::*;

            /// Called by the constructor to init the plugin.
            #[unsafe(no_mangle)]
            pub extern "C" fn truce_au_init() {
                ::truce_au::register_au::<$plugin_type>();
            }

            // AU v2 factory — delegates to au_v2_shim.c if compiled,
            // otherwise the weak stub in au_shim_common.c returns NULL.
            unsafe extern "C" {
                fn truce_au_v2_factory_bridge(
                    desc: *const ::std::ffi::c_void,
                ) -> *mut ::std::ffi::c_void;
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn TruceAUFactory(
                desc: *const ::std::ffi::c_void,
            ) -> *mut ::std::ffi::c_void {
                truce_au_v2_factory_bridge(desc)
            }
        }
    };
}
