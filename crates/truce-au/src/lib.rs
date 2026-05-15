//! Audio Unit v3 format wrapper for truce.
//!
//! Uses an Objective-C shim compiled via `cc` that implements the
//! `AUAudioUnit` subclass. The shim calls back into Rust for all
//! plugin logic via C FFI.

pub mod ffi;

use std::ffi::CString;
use std::os::raw::c_char;
use std::slice;

// `Float::from_f64` is only invoked from the macOS-only `set_param`
// closure in `cb_gui_open` (the AU v2 host notifier path). Gate the
// import so iOS builds, which take a `_id`-no-op branch instead,
// don't flag it as unused.
#[cfg(target_os = "macos")]
use truce_core::Float;
use truce_core::cast::{len_u32, sample_pos_i64};
use truce_core::editor::Editor;
// `ClosureBridge`, `PluginContext`, `SendPtr`, `RawWindowHandle` are
// consumed only inside the apple-gated body of `cb_gui_open` — the
// AppKit/UiKit variants don't exist on Linux/Windows. Importing them
// from a non-apple module would also trigger the unused-import lint
// there.
#[cfg(any(target_os = "macos", target_os = "ios"))]
use truce_core::editor::{ClosureBridge, PluginContext, RawWindowHandle, SendPtr};
use truce_core::events::{EVENT_LIST_PREALLOC, Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_core::midi::{pitch_bend_from_bytes, pitch_bend_to_bytes};
use truce_core::process::ProcessContext;
use truce_core::state;
use truce_core::wrapper::{
    default_io_channels, log_missing_bus_layout, run_audio_block, run_extern_callback_with,
    run_register,
};
use truce_params::{ParamFlags, Params};

use ffi::{
    AuCallbacks, AuMidi2Event, AuMidiEvent, AuParamDescriptor, AuPluginDescriptor,
    AuTransportSnapshot,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

// ---------------------------------------------------------------------------
// Instance wrapper — one per plugin instance, stored as the opaque ctx
// ---------------------------------------------------------------------------

/// Bounded handoff slot for state loads. Capacity 1: presets don't
/// arrive faster than the audio thread completes a block, and on
/// overflow we want most-recent-wins (`force_push`) so a rapid
/// double-recall doesn't get the audio thread to apply a stale state
/// after the host already moved on.
type StateLoadQueue = crossbeam_queue::ArrayQueue<state::DeserializedState>;

struct AuInstance<P: PluginExport> {
    plugin: P,
    /// Stable handle to the params Arc, set once at instance creation.
    /// Host-thread callbacks (`cb_param_*`, `cb_state_save`) read params
    /// through this handle so they never form a `&inst.plugin`
    /// reference. Params are atomic-backed and `Sync`.
    params_arc: Arc<P::Params>,
    /// Atomic snapshots of the plugin's most recent `latency()` /
    /// `tail()`. Updated by the audio thread (or `cb_reset`).
    latency_cache: AtomicU32,
    tail_cache: AtomicU32,
    event_list: EventList,
    output_events: EventList,
    plugin_id_hash: u64,
    sample_rate: f64,
    /// Max block size declared by the host via
    /// `kAudioUnitProperty_MaximumFramesPerSlice` (delivered through
    /// `cb_reset`'s `max_frames`). A generous default keeps the
    /// contract assert in `cb_process` from tripping for hosts that
    /// send process before declaring a max.
    max_block_size: usize,
    /// `true` once `cb_reset` has run. `cb_process` early-returns and
    /// zeros outputs while false so DSP doesn't run with un-snapped
    /// smoothers / unset sample rate.
    prepared: bool,
    /// Reused per-block scratch for `RawBufferScratch::build`. Lives
    /// on the instance so the audio thread doesn't heap-allocate.
    ///
    /// Parameterised by `P::Sample`; widens/narrows host-`f32`
    /// buffers around `plugin.process()` for plugins on `prelude64`.
    scratch: truce_core::buffer::RawBufferScratch<<P as truce_core::plugin::Plugin>::Sample>,
    editor: Option<Box<dyn Editor>>,
    /// Shared transport slot: audio thread writes each block, editor reads.
    transport_slot: Arc<truce_core::TransportSlot>,
    /// Bounded SPSC handoff for state loads. Host (`cb_state_load`)
    /// and editor (`set_state` callback) deserialize on their thread
    /// and push the result; the audio thread pops at the top of
    /// `cb_process` and calls [`state::apply_state`]
    /// under its exclusive `&mut plugin`.
    pending_state: Arc<StateLoadQueue>,
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
    let params_arc = plugin.params_arc();
    let latency_cache = AtomicU32::new(plugin.latency());
    let tail_cache = AtomicU32::new(plugin.tail());
    let instance = Box::new(AuInstance::<P> {
        plugin,
        params_arc,
        latency_cache,
        tail_cache,
        event_list: EventList::with_capacity(EVENT_LIST_PREALLOC),
        output_events: EventList::with_capacity(EVENT_LIST_PREALLOC),
        plugin_id_hash: state::shared_plugin_state_hash(&info),
        sample_rate: 44100.0,
        max_block_size: 8192,
        prepared: false,
        scratch: truce_core::buffer::RawBufferScratch::default(),
        editor: None,
        transport_slot: truce_core::TransportSlot::new(),
        pending_state: Arc::new(StateLoadQueue::new(1)),
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
        // Clamp host-supplied max_frames to a sane minimum.
        let max_frames = (max_frames as usize).max(1024);
        inst.sample_rate = sample_rate;
        inst.max_block_size = max_frames;
        let (num_in, num_out) = default_io_channels::<P>().unwrap_or((2, 2));
        inst.scratch
            .ensure_capacity(num_in as usize, num_out as usize, max_frames);
        inst.plugin.reset(sample_rate, max_frames);
        inst.plugin.params().set_sample_rate(sample_rate);
        inst.plugin.params().snap_smoothers();
        inst.latency_cache
            .store(inst.plugin.latency(), Ordering::Relaxed);
        inst.tail_cache.store(inst.plugin.tail(), Ordering::Relaxed);
        inst.prepared = true;
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
    events2: *const AuMidi2Event,
    num_events2: u32,
    transport_ptr: *const AuTransportSnapshot,
) {
    let nf = num_frames as usize;
    let ok = run_audio_block::<P>("AU", || unsafe {
        let inst = &mut *ctx.cast::<AuInstance<P>>();
        let num_frames = nf;

        // Host called render before AU initialized us — sample rate
        // and smoothers haven't been primed. Zero outputs and bail.
        if !inst.prepared {
            for ch in 0..num_output_channels as usize {
                let ptr = *outputs.add(ch);
                if !ptr.is_null() {
                    std::ptr::write_bytes(ptr, 0, num_frames);
                }
            }
            return;
        }

        // Apply any pending state-load before per-block work so the
        // plugin sees consistent params and extra state for the
        // entire block. See `pending_state` field comment for the
        // queue-overflow policy.
        if let Some(state) = inst.pending_state.pop() {
            state::apply_state(&mut inst.plugin, &state);
        }

        // Convert MIDI events
        inst.event_list.clear();
        if !events.is_null() && num_events > 0 {
            let event_slice = slice::from_raw_parts(events, num_events as usize);
            for ev in event_slice {
                let status = ev.status & 0xF0;
                let channel = ev.status & 0x0F;
                let body = match status {
                    0x90 if ev.data2 > 0 => Some(EventBody::NoteOn {
                        group: 0,
                        channel,
                        note: ev.data1,
                        velocity: ev.data2,
                    }),
                    0x90 => Some(EventBody::NoteOff {
                        group: 0,
                        channel,
                        note: ev.data1,
                        velocity: 0,
                    }),
                    0x80 => Some(EventBody::NoteOff {
                        group: 0,
                        channel,
                        note: ev.data1,
                        velocity: ev.data2,
                    }),
                    0xA0 => Some(EventBody::Aftertouch {
                        group: 0,
                        channel,
                        note: ev.data1,
                        pressure: ev.data2,
                    }),
                    0xB0 => Some(EventBody::ControlChange {
                        group: 0,
                        channel,
                        cc: ev.data1,
                        value: ev.data2,
                    }),
                    0xE0 => Some(EventBody::PitchBend {
                        group: 0,
                        channel,
                        value: pitch_bend_from_bytes(ev.data1, ev.data2),
                    }),
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
        // MIDI 2.0 UMP decode. AU v3 hosts on iOS 17+ / macOS 14+
        // deliver per-note expression + 32-bit-resolution channel
        // voice messages through `AURenderEvent.MIDIEventList`; the
        // Swift shim hands them here as 64-bit UMPs (MIDI 2.0 CV
        // message type 0x4). Other UMP types (utility, system,
        // SysEx, data) are not yet mapped — skipped here.
        if !events2.is_null() && num_events2 > 0 {
            let slice2 = slice::from_raw_parts(events2, num_events2 as usize);
            for ev in slice2 {
                if let Some(body) = decode_ump_channel_voice_2(ev.words) {
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
            P::supports_in_place(),
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
        let _ = audio_buffer;
        // Narrow rendered f64 output back to host f32 when needed.
        // No-op for `f32` plugins.
        inst.scratch
            .finish_widening_f32(outputs, num_output_channels, len_u32(num_frames));

        // Refresh latency / tail caches so the host's main-thread
        // queries don't have to call into `inst.plugin`.
        inst.latency_cache
            .store(inst.plugin.latency(), Ordering::Relaxed);
        inst.tail_cache.store(inst.plugin.tail(), Ordering::Relaxed);
    });
    if !ok {
        unsafe {
            for ch in 0..num_output_channels as usize {
                let ptr = *outputs.add(ch);
                if !ptr.is_null() {
                    std::ptr::write_bytes(ptr, 0, nf);
                }
            }
        }
    }
}

unsafe extern "C" fn cb_param_count<P: PluginExport>(ctx: *mut std::ffi::c_void) -> u32 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        len_u32(inst.params_arc.count())
    }
}

unsafe extern "C" fn cb_param_get_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
) -> f64 {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        inst.params_arc.get_plain(id).unwrap_or(0.0)
    }
}

unsafe extern "C" fn cb_param_set_value<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    id: u32,
    value: f64,
) {
    unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        inst.params_arc.set_plain(id, value);
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
        match inst.params_arc.format_value(id, value) {
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
    // Pre-zero the out pointers so a panic anywhere in the body below
    // leaves the host seeing an empty blob rather than a stale buffer
    // pointer paired with whatever length was last written.
    unsafe {
        *out_data = std::ptr::null_mut();
        *out_len = 0;
    }
    run_extern_callback_with::<P, ()>("au", "save_state", (), || unsafe {
        let inst = &*ctx.cast::<AuInstance<P>>();
        let (ids, values) = inst.params_arc.collect_values();
        // `plugin.save_state()` reads through the plugin reference: a
        // user impl that mutates non-atomic state from `process` while
        // also reading it from `save_state` races here. The contract
        // is "save_state must be safe to call concurrently with
        // process"; impls that copy from atomic params are fine.
        //
        // Allocator pin: this wrapper allocates with libc `malloc` and
        // the AU shim frees with libc `free`. The Rust global allocator
        // must not appear on either side. (VST2 uses the Rust global
        // allocator for both save + free; do not cross wires when
        // refactoring `_save_state` paths together.)
        let extra = inst.plugin.save_state();
        let blob = state::serialize_state(inst.plugin_id_hash, &ids, &values, &extra);

        let len = blob.len();
        let ptr = malloc(len).cast::<u8>();
        if ptr.is_null() {
            // malloc failed — `*out_data` is already null and
            // `*out_len` already 0 from the pre-zero above.
            return;
        }
        std::ptr::copy_nonoverlapping(blob.as_ptr(), ptr, len);
        *out_data = ptr;
        *out_len = len_u32(len);
    });
}

unsafe extern "C" fn cb_state_load<P: PluginExport>(
    ctx: *mut std::ffi::c_void,
    data: *const u8,
    len: u32,
) {
    run_extern_callback_with::<P, ()>("au", "load_state", (), || unsafe {
        let inst = &mut *ctx.cast::<AuInstance<P>>();
        // `slice::from_raw_parts(null, n)` for `n > 0` is UB. Treat
        // `(null, *)` and `(_, 0)` the same as "host gave us nothing".
        if data.is_null() || len == 0 {
            return;
        }
        let blob = slice::from_raw_parts(data, len as usize);
        if let Some(deserialized) = state::deserialize_state(blob, inst.plugin_id_hash) {
            // Apply params synchronously on the host thread (atomic-safe)
            // so host queries that read parameter values right after
            // `setFullState:` see the restored values without first
            // running a render block.
            state::apply_params(&*inst.params_arc, &deserialized);
            // Hand the deserialized state to the audio thread for
            // application. `force_push` overwrites any older pending
            // blob — see the `pending_state` field comment for why
            // newest-wins is the right policy.
            let _ = inst.pending_state.force_push(deserialized);
            if let Some(ref mut editor) = inst.editor {
                editor.state_changed();
            }
        }
    });
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

/// Decode a Universal MIDI Packet's first two words into a MIDI 2.0
/// channel-voice [`EventBody`]. Returns `None` for non-channel-voice
/// UMPs (utility, system, `SysEx`, data) — those are not surfaced to
/// plugins yet. Spec reference: MIDI 2.0 M2-104-UM, §4.1 (MIDI 2.0
/// Channel Voice Messages).
#[allow(clippy::cast_possible_truncation)] // UMP fields are bit-packed; truncation is intentional
fn decode_ump_channel_voice_2(words: [u32; 4]) -> Option<EventBody> {
    // Bit layout (word 0):
    //   31..28 mt (message type, 0x4 = MIDI 2.0 CV)
    //   27..24 group (0..=15)
    //   23..20 status nibble (0x8 = NoteOff, 0x9 = NoteOn, ...)
    //   19..16 channel (0..=15)
    //   15..0  status-specific (note + attribute-type, cc number, ...)
    let w0 = words[0];
    let w1 = words[1];
    let mt = ((w0 >> 28) & 0xF) as u8;
    if mt != 0x4 {
        return None;
    }
    let group = ((w0 >> 24) & 0xF) as u8;
    let status = ((w0 >> 20) & 0xF) as u8;
    let channel = ((w0 >> 16) & 0xF) as u8;
    let byte_a = ((w0 >> 8) & 0xFF) as u8; // note / cc number / etc.
    let byte_b = (w0 & 0xFF) as u8; // attribute-type / index / etc.
    let body = match status {
        0x8 => EventBody::NoteOff2 {
            group,
            channel,
            note: byte_a & 0x7F,
            velocity: (w1 >> 16) as u16,
            attribute_type: byte_b,
            attribute: (w1 & 0xFFFF) as u16,
        },
        0x9 => EventBody::NoteOn2 {
            group,
            channel,
            note: byte_a & 0x7F,
            velocity: (w1 >> 16) as u16,
            attribute_type: byte_b,
            attribute: (w1 & 0xFFFF) as u16,
        },
        0xA => EventBody::PolyPressure2 {
            group,
            channel,
            note: byte_a & 0x7F,
            pressure: w1,
        },
        // 0x0 = Registered Per-Note (RPN-like), 0x1 = Assignable
        // Per-Note. MIDI 2.0 §4.1.4. The lower 8 bits of word 0
        // carry the per-note controller index; word 1 is the value.
        0x0 | 0x1 => EventBody::PerNoteCC {
            group,
            channel,
            note: byte_a & 0x7F,
            cc: byte_b,
            value: w1,
            registered: status == 0x0,
        },
        // 0x6 = Per-Note Pitch Bend.
        0x6 => EventBody::PerNotePitchBend {
            group,
            channel,
            note: byte_a & 0x7F,
            value: w1,
        },
        // 0xF = Per-Note Management. The flags live in byte_b (per
        // §4.1.6); only the low two bits are defined today.
        0xF => EventBody::PerNoteManagement {
            group,
            channel,
            note: byte_a & 0x7F,
            flags: byte_b,
        },
        0xB => EventBody::ControlChange2 {
            group,
            channel,
            cc: byte_a & 0x7F,
            value: w1,
        },
        0xD => EventBody::ChannelPressure2 {
            group,
            channel,
            pressure: w1,
        },
        0xE => EventBody::PitchBend2 {
            group,
            channel,
            value: w1,
        },
        // 0x2 = Registered Controller (RPN), 0x3 = Assignable
        // Controller (NRPN). Bank lives in `byte_a` (lower 7 bits),
        // index in `byte_b` (lower 7 bits).
        0x2 => EventBody::RegisteredController {
            group,
            channel,
            bank: byte_a & 0x7F,
            index: byte_b & 0x7F,
            value: w1,
        },
        0x3 => EventBody::AssignableController {
            group,
            channel,
            bank: byte_a & 0x7F,
            index: byte_b & 0x7F,
            value: w1,
        },
        0xC => EventBody::ProgramChange2 {
            group,
            channel,
            program: (w1 >> 24) as u8 & 0x7F,
            // Word 0 bit 0 carries the "B" (bank-valid) flag; the
            // bank bytes live in word 1's bottom half (MSB then LSB).
            bank: if w0 & 0x01 == 1 {
                Some(((w1 >> 8) as u8 & 0x7F, w1 as u8 & 0x7F))
            } else {
                None
            },
        },
        _ => return None,
    };
    Some(body)
}

/// Map a truce `Event` body to a 3-byte AU MIDI packet. Returns
/// `None` for event types that don't fit (MIDI 2.0, `ParamChange`,
/// Transport, etc.). Mirrors the VST2/VST3 encoders.
fn try_encode_au_midi(event: &Event) -> Option<AuMidiEvent> {
    let (status, data1, data2) = match &event.body {
        EventBody::NoteOn {
            channel,
            note,
            velocity,
            ..
        } => (0x90 | (channel & 0x0F), *note, *velocity),
        EventBody::NoteOff {
            channel,
            note,
            velocity,
            ..
        } => (0x80 | (channel & 0x0F), *note, *velocity),
        EventBody::ControlChange {
            channel, cc, value, ..
        } => (0xB0 | (channel & 0x0F), *cc, *value),
        EventBody::Aftertouch {
            channel,
            note,
            pressure,
            ..
        } => (0xA0 | (channel & 0x0F), *note, *pressure),
        EventBody::ChannelPressure {
            channel, pressure, ..
        } => (0xD0 | (channel & 0x0F), *pressure, 0),
        EventBody::PitchBend { channel, value, .. } => {
            let (lsb, msb) = pitch_bend_to_bytes(*value);
            (0xE0 | (channel & 0x0F), lsb, msb)
        }
        EventBody::ProgramChange {
            channel, program, ..
        } => (0xC0 | (channel & 0x0F), *program, 0),
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
        let n = inst
            .output_events
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
    // AU is macOS+iOS-only at runtime. Linux/Windows builds compile
    // the wrapper crate for completeness (it's part of the workspace
    // build matrix) but the body references AppKit / UIKit /
    // AUEventListener APIs that don't exist off-Apple. Stubbing the
    // body keeps the FFI table population in `register_au_inner`
    // type-checking on every platform.
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        let _ = ctx;
        let _ = parent;
        let _ = std::marker::PhantomData::<P>;
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
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
            let params_for_state = params.clone();
            let pending_state_for_set = inst.pending_state.clone();
            let plugin_id_hash_for_set = inst.plugin_id_hash;
            let transport_slot = inst.transport_slot.clone();
            let ctx_for_begin = ctx_raw;
            let ctx_for_end = ctx_raw;
            // iOS AU v3 hosts the editor inside an .appex; v2's
            // `AUEventListener` doesn't exist there. Parameter
            // changes from the editor flow to the host directly
            // through the AUParameterTree's setter (handled by the
            // Swift shim). The begin/set/end closures are no-ops on
            // iOS so the plugin's editor code stays platform-agnostic.
            let context = PluginContext::from_closures(
                ClosureBridge {
                    #[cfg(target_os = "macos")]
                    begin_edit: Box::new(move |id| {
                        // Broadcasts kAudioUnitEvent_BeginParameterChangeGesture
                        // via AUEventListenerNotify so hosts (Logic, Live,
                        // Reaper) group subsequent set_param calls into one
                        // undo step and one automation gesture.
                        truce_au_v2_host_begin_param_gesture(ctx_for_begin.as_ptr().cast_mut(), id);
                    }),
                    #[cfg(target_os = "ios")]
                    begin_edit: Box::new(move |_id| {
                        let _ = ctx_for_begin;
                    }),
                    #[cfg(target_os = "macos")]
                    set_param: Box::new(move |id, value| {
                        // One combined trait dispatch (set_normalized
                        // + get_plain) instead of two — the
                        // `#[derive(Params)]` impl can compute both in
                        // a single match-arm walk.
                        let plain =
                            f32::from_f64(params_for_set.set_normalized_returning_plain(id, value));
                        truce_au_v2_host_set_param(ctx_raw.as_ptr().cast_mut(), id, plain);
                    }),
                    #[cfg(target_os = "ios")]
                    set_param: Box::new(move |id, value| {
                        // No host-notify on iOS; just write the
                        // normalised value through. The Swift shim
                        // polls the parameter tree.
                        let _ = ctx_raw;
                        let _ = params_for_set.set_normalized_returning_plain(id, value);
                    }),
                    #[cfg(target_os = "macos")]
                    end_edit: Box::new(move |id| {
                        // Closes the gesture started by begin_edit so the
                        // host commits the undo group / stops automation
                        // recording.
                        truce_au_v2_host_end_param_gesture(ctx_for_end.as_ptr().cast_mut(), id);
                    }),
                    #[cfg(target_os = "ios")]
                    end_edit: Box::new(move |_id| {
                        let _ = ctx_for_end;
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
                        plugin.save_state()
                    }),
                    set_state: Box::new(move |bytes| {
                        if let Some(deserialized) =
                            state::deserialize_state(&bytes, plugin_id_hash_for_set)
                        {
                            // Apply params synchronously so the editor
                            // sees the restore on its own thread.
                            // Mirrors `cb_state_load`.
                            state::apply_params(&*params_for_state, &deserialized);
                            let _ = pending_state_for_set.force_push(deserialized);
                        }
                    }),
                    transport: Box::new(move || transport_slot.read()),
                },
                params_for_ctx,
            );
            #[cfg(target_os = "macos")]
            let handle = RawWindowHandle::AppKit(parent);
            #[cfg(target_os = "ios")]
            let handle = RawWindowHandle::UiKit(parent);
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
}

// AU v2 host-side automation notifiers live in `au_v2_shim.c`,
// which only compiles on macOS. iOS doesn't have AU v2 at all —
// AU v3 host notifies via the parameter tree directly.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn truce_au_v2_host_set_param(ctx: *mut std::ffi::c_void, param_id: u32, value: f32);
    fn truce_au_v2_host_begin_param_gesture(ctx: *mut std::ffi::c_void, param_id: u32);
    fn truce_au_v2_host_end_param_gesture(ctx: *mut std::ffi::c_void, param_id: u32);
}

// ---------------------------------------------------------------------------
// Registration: called once from the export_au! macro
// ---------------------------------------------------------------------------

/// Register the plugin with the AU system. Must be called once at load time
/// (typically from a constructor function generated by `export_au!`).
/// Host-facing AU display name. Reads `truce.toml`'s `au_name`
/// (baked into `PluginInfo` by `truce::plugin_info!`), falling back
/// to `PluginInfo::name`. The v3 host gets its display name out of
/// the appex's `Info.plist` (`AUNAME`, populated by
/// `cargo truce install --au3` from `au3_name`), not from this
/// function — `g_descriptor->name` only feeds the v2 bridge's
/// internal scanning responses, so the same value works for both
/// build flavours.
fn resolved_plugin_name(info: &truce_core::info::PluginInfo) -> &'static str {
    truce_core::info::resolve_name_override(info.au_name, info.name)
}

pub fn register_au<P: PluginExport>() {
    // Called from the export macro's `extern "C" fn init()` static
    // initializer. Catch any panic so it doesn't cross the FFI
    // boundary and abort the host process.
    run_register::<P>("AU", || {
        let Some((num_inputs, num_outputs)) = default_io_channels::<P>() else {
            log_missing_bus_layout::<P>("AU");
            return;
        };
        register_au_inner::<P>(num_inputs, num_outputs);
    });
}

fn register_au_inner<P: PluginExport>(num_inputs: u32, num_outputs: u32) {
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
        num_inputs,
        num_outputs,
        bypass_param_id,
        has_midi_output,
    }));

    let callbacks = Box::leak(Box::new(AuCallbacks {
        create: cb_create::<P>,
        destroy: cb_destroy::<P>,
        reset: cb_reset::<P>,
        process: cb_process::<P>,
        param_count: cb_param_count::<P>,
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
        // macOS: register both AU v2 (`.component`) and AU v3 (`.appex`)
        // entry points. AU v2's factory delegates to the C shim.
        #[cfg(target_os = "macos")]
        mod _au_entry {
            use super::*;

            /// Called by the constructor to init the plugin.
            #[unsafe(no_mangle)]
            pub extern "C" fn truce_au_init() {
                ::truce_au::register_au::<$plugin_type>();
            }

            // AU v2 factory — delegates to au_v2_shim.c, which the
            // build.rs always compiles into the shim static lib.
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
        // iOS: AU v3 only. The Swift `AudioUnitFactory` /
        // `TruceAUAudioUnit` in the .appex bundle reads our exported
        // globals (g_callbacks / g_descriptor / ...) at runtime via
        // the dynamic symbol table; we just need `truce_au_init` to
        // run from the dylib constructor.
        #[cfg(target_os = "ios")]
        mod _au_entry {
            use super::*;

            #[unsafe(no_mangle)]
            pub extern "C" fn truce_au_init() {
                ::truce_au::register_au::<$plugin_type>();
            }
        }
    };
}
