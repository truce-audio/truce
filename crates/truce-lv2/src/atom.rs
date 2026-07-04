//! LV2 Atom + Sequence parsing/writing.
//!
//! The Atom extension is LV2's typed event data format. For MIDI, events
//! arrive in an `LV2_Atom_Sequence` port: a header + a series of frame-
//! timestamped `LV2_Atom_Event`s. Each event carries a type URID; events
//! where the type is the URID for `midi:MidiEvent` contain raw MIDI bytes
//! in their body.
//!
//! We hand-write the decoder because the pure-C layout is simple and we
//! need zero allocations on the audio thread.

// LV2 atoms are 8-byte aligned by spec (pad fields enforce it at every
// nested header), so the host hands us byte buffers whose interior
// pointers are always at least as aligned as the typed struct we're
// reading into. Per-cast site allows would just be noise.
#![allow(clippy::cast_ptr_alignment)]

use std::ffi::c_void;

use truce_core::cast::{len_u32, sample_pos_i64};
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::midi::{downconvert_to_midi1, parse_midi1, pitch_bend_to_bytes, route_midi_port};

use crate::urid::{Urid, UridMap};

/// Layout of `LV2_Atom` - type + size prefix.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Atom {
    pub size: u32,
    pub type_: Urid,
}

/// `LV2_Atom_Sequence_Body` - unit/pad prefix for a sequence body.
#[repr(C)]
pub struct AtomSequenceBody {
    pub unit: Urid,
    pub pad: u32,
}

/// Full `LV2_Atom_Sequence` - header then body then events. The port
/// pointer the host hands us points here.
#[repr(C)]
pub struct AtomSequence {
    pub atom: Atom,
    pub body: AtomSequenceBody,
    // Followed by event data; we walk it manually.
}

/// `LV2_Atom_Event` - per-event header. Time is in frames relative to the
/// start of the current `run()` block.
#[repr(C)]
#[derive(Clone, Copy)]
struct AtomEventHeader {
    time_frames: i64,
    body: Atom,
    // Body bytes follow this struct, padded to 8-byte alignment.
}

/// Reader that walks an `LV2_Atom_Sequence` and yields its events.
pub struct AtomSequenceReader<'a> {
    seq: *const AtomSequence,
    urid: &'a UridMap,
}

impl<'a> AtomSequenceReader<'a> {
    pub fn new(seq: *const AtomSequence, urid: &'a UridMap) -> Self {
        Self { seq, urid }
    }

    /// Walk the sequence, calling `f(sample_offset, midi_bytes)` for every
    /// `midi:MidiEvent` entry.
    ///
    /// # Safety
    /// `self.seq` must point to a valid atom sequence for the duration of
    /// the iteration.
    pub fn for_each_midi(&self, mut f: impl FnMut(u32, &[u8])) {
        if self.seq.is_null() || self.urid.midi_event == 0 {
            return;
        }
        unsafe {
            self.walk(|frame, ev_type, body_ptr, body_bytes| {
                if ev_type == self.urid.midi_event {
                    let slice = core::slice::from_raw_parts(body_ptr, body_bytes);
                    // Frame offsets within a process block are
                    // bounded by `block_size <= u32::MAX`.
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let frame_u32 = frame.max(0) as u32;
                    f(frame_u32, slice);
                }
            });
        }
    }

    /// Walk the sequence, calling `f(sample_offset, property_urid, value)`
    /// for every `patch:Set` Object whose `patch:value` can be decoded
    /// as a numeric atom. The host emits one such Object per parameter
    /// update; `time_frames` on the event header carries the within-block
    /// sample offset for sample-accurate automation.
    ///
    /// # Safety
    /// `self.seq` must be valid for the duration of the call.
    pub fn for_each_patch_set(&self, mut f: impl FnMut(u32, Urid, f64)) {
        if self.seq.is_null() || self.urid.patch_set == 0 {
            return;
        }
        unsafe {
            self.walk(|frame, ev_type, body_ptr, body_bytes| {
                if ev_type != self.urid.atom_blank && ev_type != self.urid.atom_object {
                    return;
                }
                // LV2_Atom_Object_Body: { id: Urid; otype: Urid; props… }
                let header_size = core::mem::size_of::<Urid>() * 2;
                if body_bytes < header_size {
                    return;
                }
                let otype = *body_ptr.add(core::mem::size_of::<Urid>()).cast::<Urid>();
                if otype != self.urid.patch_set {
                    return;
                }
                let mut property: Option<Urid> = None;
                let mut value: Option<f64> = None;
                let mut offset = header_size;
                let prop_header_min =
                    core::mem::size_of::<Urid>() * 2 + core::mem::size_of::<Atom>();
                while offset + prop_header_min <= body_bytes {
                    let key = *body_ptr.add(offset).cast::<Urid>();
                    let value_header = body_ptr.add(offset + core::mem::size_of::<Urid>() * 2);
                    let value_atom = *value_header.cast::<Atom>();
                    let value_data = value_header.add(core::mem::size_of::<Atom>());
                    let value_size = value_atom.size as usize;
                    let entry_total = prop_header_min + value_size;
                    let padded = (entry_total + 7) & !7;
                    if key == self.urid.patch_property
                        && value_atom.type_ != 0
                        && value_size >= core::mem::size_of::<Urid>()
                    {
                        property = Some(*value_data.cast::<Urid>());
                    } else if key == self.urid.patch_value
                        && let Some(v) =
                            self.read_atom_number(value_atom.type_, value_data, value_size)
                        && v.is_finite()
                    {
                        value = Some(v);
                    }
                    if padded == 0 {
                        break;
                    }
                    offset += padded;
                }
                if let (Some(prop), Some(v)) = (property, value) {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let frame_u32 = frame.max(0) as u32;
                    f(frame_u32, prop, v);
                }
            });
        }
    }

    /// Walk the sequence and update `info` from the last `time:Position`
    /// object encountered. Returns `true` if at least one such event was
    /// found.
    ///
    /// LV2 hosts typically emit one `time:Position` per `run()` block when
    /// the transport changes (play / seek / tempo edit); a host like
    /// Ardour sends one at the start of each block while playing.
    ///
    /// # Safety
    /// `self.seq` must point to a valid atom sequence for the duration of
    /// the call.
    pub fn apply_time_position(&self, info: &mut TransportInfo) -> bool {
        if self.seq.is_null() || self.urid.time_position == 0 {
            return false;
        }
        let mut found = false;
        unsafe {
            self.walk(|_, ev_type, body_ptr, body_bytes| {
                if ev_type != self.urid.atom_blank && ev_type != self.urid.atom_object {
                    return;
                }
                if !self.read_time_position(body_ptr, body_bytes, info) {
                    return;
                }
                found = true;
            });
        }
        found
    }

    /// Low-level sequence walk. Calls `f(frame, body_type, body_ptr, body_size)`
    /// for each event.
    ///
    /// # Safety
    /// `self.seq` must be valid for the duration of the call.
    unsafe fn walk<F: FnMut(i64, Urid, *const u8, usize)>(&self, mut f: F) {
        unsafe {
            let seq = &*self.seq;
            let body_size = seq.atom.size as usize;
            if body_size < core::mem::size_of::<AtomSequenceBody>() {
                return;
            }
            let data_size = body_size - core::mem::size_of::<AtomSequenceBody>();
            let data_start = self
                .seq
                .cast::<u8>()
                .add(core::mem::size_of::<AtomSequence>());
            let mut offset = 0usize;
            while offset + core::mem::size_of::<AtomEventHeader>() <= data_size {
                let ev_ptr = data_start.add(offset).cast::<AtomEventHeader>();
                let ev = *ev_ptr;
                let body_bytes = ev.body.size as usize;
                let total = core::mem::size_of::<AtomEventHeader>() + body_bytes;
                let padded = (total + 7) & !7;
                if offset + padded > data_size {
                    break;
                }
                let body_ptr = data_start.add(offset + core::mem::size_of::<AtomEventHeader>());
                f(ev.time_frames, ev.body.type_, body_ptr, body_bytes);
                offset += padded;
            }
        }
    }

    /// Decode an `LV2_Atom_Object` body as a `time:Position` and merge
    /// its fields into `info`. Returns true on success.
    ///
    /// # Safety
    /// `body_ptr` must point to `body_bytes` bytes of valid atom-object
    /// body data.
    unsafe fn read_time_position(
        &self,
        body_ptr: *const u8,
        body_bytes: usize,
        info: &mut TransportInfo,
    ) -> bool {
        unsafe {
            // LV2_Atom_Object_Body per lv2/atom/atom.h:
            //   { uint32_t id; uint32_t otype; }
            // id is a per-object instance identifier (0 for blank); otype is
            // the class URID we key on.
            let header_size = core::mem::size_of::<Urid>() * 2;
            if body_bytes < header_size {
                return false;
            }
            let otype = *body_ptr.add(core::mem::size_of::<Urid>()).cast::<Urid>();
            if otype != self.urid.time_position {
                return false;
            }
            // Collect raw LV2 time:* fields first, then reconcile them to the
            // truce `TransportInfo` schema. The LV2 time extension reports
            // position as `bar` (0-based bar index) + `barBeat` (float beat
            // position within the bar) + `beatsPerBar`, while truce exposes
            // a monotonically-increasing `position_beats` since transport
            // start plus `bar_start_beats` as the anchor. We compute both
            // from the raw fields once we've read them all.
            let mut bar: Option<f64> = None;
            let mut bar_beat: Option<f64> = None;
            let mut beats_per_bar: Option<f64> = None;
            let mut beat_direct: Option<f64> = None;

            let mut offset = header_size;
            while offset + core::mem::size_of::<Urid>() * 2 + core::mem::size_of::<Atom>()
                <= body_bytes
            {
                // Property = { key: Urid, context: Urid, value: Atom + data }
                let key = *body_ptr.add(offset).cast::<Urid>();
                // `context` is unused by time:Position writers in practice.
                let value_header = body_ptr.add(offset + core::mem::size_of::<Urid>() * 2);
                let value_atom = *value_header.cast::<Atom>();
                let value_data = value_header.add(core::mem::size_of::<Atom>());
                let value_size = value_atom.size as usize;
                let entry_total =
                    core::mem::size_of::<Urid>() * 2 + core::mem::size_of::<Atom>() + value_size;
                let padded = (entry_total + 7) & !7;

                if let Some(v) = self.read_atom_number(value_atom.type_, value_data, value_size) {
                    // NaN slipping through `clamp` would narrow to 0 on
                    // `as u8` and propagate through every consumer of
                    // `position_samples` / `tempo` as silent data loss.
                    // Skip non-finite values entirely; consumers fall
                    // back to whatever was previously set (typically the
                    // wrapper's TransportInfo default).
                    if !v.is_finite() {
                        offset += padded;
                        if padded == 0 {
                            break;
                        }
                        continue;
                    }
                    if key == self.urid.time_beats_per_minute {
                        info.tempo = v;
                    } else if key == self.urid.time_bar {
                        bar = Some(v);
                    } else if key == self.urid.time_bar_beat {
                        bar_beat = Some(v);
                    } else if key == self.urid.time_beats_per_bar {
                        beats_per_bar = Some(v);
                        // Post-clamp f64 in `0..=255`; the lint can't
                        // see through `clamp` and `round`.
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let v_u8 = v.round().clamp(0.0, f64::from(u8::MAX)) as u8;
                        info.time_sig_num = v_u8;
                    } else if key == self.urid.time_beat {
                        // Non-standard but some hosts still emit it - treat
                        // it as the absolute beat position directly. Stash
                        // here and only apply if bar/barBeat aren't given.
                        beat_direct = Some(v);
                    } else if key == self.urid.time_frame {
                        info.position_samples = sample_pos_i64(v);
                    } else if key == self.urid.time_speed {
                        info.playing = v.abs() > 1e-9;
                    } else if key == self.urid.time_beat_unit {
                        // Post-clamp f64 in `0..=255`.
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let v_u8 = v.round().clamp(0.0, f64::from(u8::MAX)) as u8;
                        info.time_sig_den = v_u8;
                    }
                }

                offset += padded;
                if padded == 0 {
                    break;
                }
            }

            // Derive truce-shaped fields from the raw bar/barBeat/beatsPerBar
            // triple. `beatsPerBar` can be missing when the plugin asks for
            // transport before the host has finished reporting all fields;
            // fall back to the current time_sig_num if so, and finally to 4.
            let bpb = beats_per_bar
                .or({
                    if info.time_sig_num > 0 {
                        Some(f64::from(info.time_sig_num))
                    } else {
                        None
                    }
                })
                .unwrap_or(4.0);
            // Precedence: spec-canonical `bar` + `barBeat` wins, then
            // `bar` alone, then non-standard `time:beat`, then `barBeat`
            // alone (legacy fallback).
            if let Some(b) = bar {
                info.bar_start_beats = b * bpb;
                if let Some(bb) = bar_beat {
                    info.position_beats = info.bar_start_beats + bb;
                } else if let Some(bd) = beat_direct {
                    info.position_beats = bd;
                } else {
                    info.position_beats = info.bar_start_beats;
                }
            } else if let Some(bd) = beat_direct {
                info.position_beats = bd;
            } else if let Some(bb) = bar_beat {
                // No bar field - best we can do is surface the intra-bar
                // offset as the position, matching our previous behavior
                // for hosts that only emit `time:barBeat`.
                info.position_beats = bb;
            }

            true
        }
    }

    /// Read a numeric atom value as f64, handling the common number types
    /// LV2 hosts use for time:Position fields.
    //
    // The atom_long branch widens `i64 as f64`; sample-frame counts in
    // host-delivered transport messages stay well below 2^52 in practice.
    #[allow(clippy::cast_precision_loss)]
    unsafe fn read_atom_number(
        &self,
        atom_type: Urid,
        data: *const u8,
        size: usize,
    ) -> Option<f64> {
        unsafe {
            if atom_type == self.urid.atom_float && size >= core::mem::size_of::<f32>() {
                Some(f64::from(*data.cast::<f32>()))
            } else if atom_type == self.urid.atom_double && size >= core::mem::size_of::<f64>() {
                Some(*data.cast::<f64>())
            } else if atom_type == self.urid.atom_int && size >= core::mem::size_of::<i32>() {
                Some(f64::from(*data.cast::<i32>()))
            } else if atom_type == self.urid.atom_long && size >= core::mem::size_of::<i64>() {
                Some(*data.cast::<i64>() as f64)
            } else if atom_type == self.urid.atom_bool && size >= core::mem::size_of::<i32>() {
                Some(if *data.cast::<i32>() != 0 { 1.0 } else { 0.0 })
            } else {
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Decode raw MIDI bytes into truce EventBody variants
// ---------------------------------------------------------------------------

pub fn midi_bytes_to_event(sample_offset: u32, bytes: &[u8]) -> Option<Event> {
    // LV2 carries legacy MIDI 1.0 byte streams with no UMP group, so
    // decode at group 0 through the shared channel-voice decoder.
    parse_midi1(0, bytes).map(|body| Event::new(sample_offset, body))
}

// ---------------------------------------------------------------------------
// Encode truce EventList into an LV2_Atom_Sequence output port
// ---------------------------------------------------------------------------

/// Overwrite the port's sequence body with the events destined for
/// MIDI output port `port`, setting the header/atom sizes so the host
/// knows how many bytes to read. `port_count` is the plugin's declared
/// output-port count; an event whose [`Event::port`] exceeds it routes
/// to port 0. Single-port plugins call with `port = 0`,
/// `port_count = 1`.
///
/// # Safety
/// `out` must point to a writable atom sequence buffer with capacity the
/// host allocated (typically a few KB).
pub unsafe fn write_midi_out_sequence(
    out: *mut AtomSequence,
    events: &EventList,
    urid: &UridMap,
    port: u8,
    port_count: u8,
) {
    unsafe {
        if out.is_null() || urid.midi_event == 0 {
            return;
        }
        // Host passes us a sequence where atom.size is the *capacity* of the
        // body buffer on entry. We overwrite it with the actual size on exit.
        let capacity = (*out).atom.size as usize;
        let atom_size = core::mem::size_of::<Atom>();
        let header_size = core::mem::size_of::<AtomSequenceBody>();
        let body_start = out.cast::<u8>().add(atom_size + header_size);
        let mut offset = 0usize;
        // Reset sequence metadata.
        (*out).atom.type_ = urid.atom_sequence;
        (*out).body.unit = 0;
        (*out).body.pad = 0;
        for event in events.iter() {
            // Only this port's events land in this sequence; an
            // out-of-range port collapses to port 0.
            if route_midi_port(event.port, port_count) != port {
                continue;
            }
            // `SysEx` events have a variable-length payload that
            // can't fit in the fixed-size `buf` below; handle them
            // here, reading the bytes out of the `EventList`'s pool
            // and writing the framed `0xF0 ... 0xF7` atom directly.
            if let EventBody::SysEx { .. } = &event.body {
                let inner = events.sysex_bytes(&event.body);
                let body_len = inner.len() + 2; // +2 for the 0xF0/0xF7 framing
                let total = core::mem::size_of::<AtomEventHeader>() + body_len;
                let padded = (total + 7) & !7;
                if offset + padded > capacity {
                    break;
                }
                let ev_ptr = body_start.add(offset).cast::<AtomEventHeader>();
                (*ev_ptr).time_frames = i64::from(event.sample_offset);
                (*ev_ptr).body.size = len_u32(body_len);
                (*ev_ptr).body.type_ = urid.midi_event;
                let body_ptr = body_start.add(offset + core::mem::size_of::<AtomEventHeader>());
                *body_ptr = 0xF0;
                core::ptr::copy_nonoverlapping(inner.as_ptr(), body_ptr.add(1), inner.len());
                *body_ptr.add(1 + inner.len()) = 0xF7;
                for i in body_len..(padded - core::mem::size_of::<AtomEventHeader>()) {
                    *body_ptr.add(i) = 0;
                }
                offset += padded;
                continue;
            }
            let mut buf = [0u8; 3];
            // LV2 carries MIDI 1.0 byte streams only; down-convert any
            // 2.0 output so it isn't dropped.
            let cv = downconvert_to_midi1(&event.body).unwrap_or(event.body);
            let (n, frame) = match &cv {
                EventBody::NoteOn {
                    channel,
                    note,
                    velocity,
                    ..
                } => {
                    buf[0] = 0x90 | (channel & 0x0F);
                    buf[1] = note & 0x7F;
                    buf[2] = velocity & 0x7F;
                    (3, event.sample_offset)
                }
                EventBody::NoteOff {
                    channel,
                    note,
                    velocity,
                    ..
                } => {
                    buf[0] = 0x80 | (channel & 0x0F);
                    buf[1] = note & 0x7F;
                    buf[2] = velocity & 0x7F;
                    (3, event.sample_offset)
                }
                EventBody::ControlChange {
                    channel, cc, value, ..
                } => {
                    buf[0] = 0xB0 | (channel & 0x0F);
                    buf[1] = cc & 0x7F;
                    buf[2] = value & 0x7F;
                    (3, event.sample_offset)
                }
                EventBody::Aftertouch {
                    channel,
                    note,
                    pressure,
                    ..
                } => {
                    buf[0] = 0xA0 | (channel & 0x0F);
                    buf[1] = note & 0x7F;
                    buf[2] = pressure & 0x7F;
                    (3, event.sample_offset)
                }
                EventBody::ChannelPressure {
                    channel, pressure, ..
                } => {
                    buf[0] = 0xD0 | (channel & 0x0F);
                    buf[1] = pressure & 0x7F;
                    // 2-byte channel pressure - emit a 2-byte MIDI msg.
                    (2, event.sample_offset)
                }
                EventBody::PitchBend { channel, value, .. } => {
                    let (lsb, msb) = pitch_bend_to_bytes(*value);
                    buf[0] = 0xE0 | (channel & 0x0F);
                    buf[1] = lsb;
                    buf[2] = msb;
                    (3, event.sample_offset)
                }
                EventBody::ProgramChange {
                    channel, program, ..
                } => {
                    buf[0] = 0xC0 | (channel & 0x0F);
                    buf[1] = program & 0x7F;
                    (2, event.sample_offset)
                }
                // MIDI 2.0 channel-voice, ParamChange, Transport,
                // per-note events: not encodable as 1- to 3-byte
                // MIDI 1.0 messages; drop rather than emit a
                // malformed atom.
                _ => continue,
            };
            let total = core::mem::size_of::<AtomEventHeader>() + n;
            let padded = (total + 7) & !7;
            if offset + padded > capacity {
                break; // out of buffer space; drop remaining events
            }
            let ev_ptr = body_start.add(offset).cast::<AtomEventHeader>();
            (*ev_ptr).time_frames = i64::from(frame);
            (*ev_ptr).body.size = len_u32(n);
            (*ev_ptr).body.type_ = urid.midi_event;
            let body_ptr = body_start.add(offset + core::mem::size_of::<AtomEventHeader>());
            core::ptr::copy_nonoverlapping(buf.as_ptr(), body_ptr, n);
            // Zero the padding bytes.
            for i in n..(padded - core::mem::size_of::<AtomEventHeader>()) {
                *body_ptr.add(i) = 0;
            }
            offset += padded;
        }
        (*out).atom.size = len_u32(header_size + offset);
    }
}

/// Write a single `time:Position` atom object into a notify-out sequence.
/// Called each `run()` block so the UI's `port_event` receives the latest
/// transport info.
///
/// If `extra_atom` is non-null (caller-supplied `atom:eventTransfer` URID
/// context), the sequence body's `unit` field is set to the URID of
/// `atom:Sequence` so hosts that validate atom-type match it.
///
/// # Safety
/// `out` must be a writable atom sequence of at least a few hundred bytes.
/// `info` is read by value; the sequence body is overwritten.
pub unsafe fn write_time_position_sequence(
    out: *mut AtomSequence,
    info: &TransportInfo,
    urid: &UridMap,
) {
    unsafe {
        if out.is_null() || urid.time_position == 0 || urid.atom_object == 0 {
            return;
        }
        let capacity = (*out).atom.size as usize;
        let atom_size = core::mem::size_of::<Atom>();
        let body_header = core::mem::size_of::<AtomSequenceBody>();
        let body_start = out.cast::<u8>().add(atom_size + body_header);

        (*out).atom.type_ = urid.atom_sequence;
        (*out).body.unit = 0;
        (*out).body.pad = 0;

        // Build the Object body's properties. Layout per LV2 atom-object spec:
        //
        //   AtomEventHeader { time_frames, body: Atom { size, type: Object } }
        //   LV2_Atom_Object_Body { id: Urid, otype: Urid }
        //   LV2_Atom_Property_Body { key: Urid, context: Urid, value: Atom, data[] }
        //   ...
        //
        // We emit a small fixed set of properties and carry Double values for
        // all of them so the UI-side decoder can use one reader.
        let ev_header_size = core::mem::size_of::<AtomEventHeader>();
        let obj_header_size = core::mem::size_of::<Urid>() * 2; // id + otype

        // Reserve the whole event in-place so property writers can align.
        let ev_ptr = body_start.cast::<AtomEventHeader>();
        if ev_header_size + obj_header_size > capacity {
            (*out).atom.size = len_u32(body_header);
            return;
        }
        (*ev_ptr).time_frames = 0;
        (*ev_ptr).body.type_ = urid.atom_object;
        let obj_body_start = body_start.add(ev_header_size);
        // Per lv2/atom/atom.h: `id` first, then `otype`.
        *obj_body_start.cast::<Urid>() = 0; // id = blank
        *obj_body_start
            .add(core::mem::size_of::<Urid>())
            .cast::<Urid>() = urid.time_position;

        let mut prop_offset = obj_header_size;
        let prop_header_size = core::mem::size_of::<Urid>() * 2 + core::mem::size_of::<Atom>();

        // Both writers share `prop_offset` and the same prop-header
        // layout. `atom_typed_size` is parameterized so `time:bar` /
        // `time:frame` / `time:beatsPerBar` / `time:beatUnit` can use
        // the spec's `xsd:long` / `xsd:int` types instead of `xsd:double`.
        let mut write_typed = |key: Urid,
                               atom_type: Urid,
                               value_size: usize,
                               write_value: &dyn Fn(*mut u8)|
         -> bool {
            if key == 0 || atom_type == 0 {
                return false;
            }
            let total = prop_header_size + value_size;
            let padded = (total + 7) & !7;
            if ev_header_size + prop_offset + padded > capacity {
                return false;
            }
            let entry = obj_body_start.add(prop_offset);
            *entry.cast::<Urid>() = key;
            *entry.add(core::mem::size_of::<Urid>()).cast::<Urid>() = 0; // context
            let atom_hdr = entry.add(core::mem::size_of::<Urid>() * 2).cast::<Atom>();
            (*atom_hdr).size = len_u32(value_size);
            (*atom_hdr).type_ = atom_type;
            let value_ptr = entry.add(prop_header_size);
            write_value(value_ptr);
            prop_offset += padded;
            true
        };

        // LV2 `time:bar` is an integer bar index (0-based); `time:barBeat`
        // is the float position within that bar in [0, beatsPerBar). Our
        // TransportInfo stores `bar_start_beats` as the absolute beat
        // position at which the current bar started, so the bar index is
        // `bar_start_beats / beatsPerBar`. Writers never emit a raw global
        // beat count - the reader reconstructs it on the other side.
        let bpb = if info.time_sig_num > 0 {
            f64::from(info.time_sig_num)
        } else {
            4.0
        };
        // `bar_start_beats / bpb` is a small bar count (rarely > 10⁵
        // for a normal session); the cast is provably lossless here.
        #[allow(clippy::cast_possible_truncation)]
        let bar_index = (info.bar_start_beats / bpb).round() as i64;
        let bar_beat = info.position_beats - info.bar_start_beats;
        // Bail at the first overflow so the partial atom-object we'd
        // otherwise emit (with a body.size derived from `prop_offset`
        // but missing later properties) doesn't end up in the wire
        // stream - strict hosts (Ardour, Carla) reject malformed
        // objects.
        let mut ok = true;
        ok = ok
            && write_typed(urid.time_speed, urid.atom_double, 8, &|p| {
                *p.cast::<f64>() = if info.playing { 1.0 } else { 0.0 };
            });
        ok = ok
            && write_typed(urid.time_beats_per_minute, urid.atom_double, 8, &|p| {
                *p.cast::<f64>() = info.tempo;
            });
        ok = ok
            && write_typed(urid.time_bar_beat, urid.atom_float, 4, &|p| {
                // bar_beat is bounded by `time_sig_num` (typically 4-12);
                // f32 has 7 decimals of precision, far more than needed.
                #[allow(clippy::cast_possible_truncation)]
                let v = bar_beat as f32;
                *p.cast::<f32>() = v;
            });
        // LV2 spec types: `time:bar` is `xsd:long`, `time:frame` is
        // `xsd:long`, `time:beatsPerBar` is `xsd:int`, `time:beatUnit`
        // is `xsd:int`. Strict hosts (Ardour, Carla) type-check the
        // atom value and reject `xsd:double` for these. Round-trip
        // works in-tree because our reader (`read_atom_number`)
        // accepts any numeric type, but cross-host interop suffers.
        ok = ok
            && write_typed(urid.time_bar, urid.atom_long, 8, &|p| {
                *p.cast::<i64>() = bar_index;
            });
        ok = ok
            && write_typed(urid.time_frame, urid.atom_long, 8, &|p| {
                *p.cast::<i64>() = info.position_samples;
            });
        ok = ok
            && write_typed(urid.time_beats_per_bar, urid.atom_int, 4, &|p| {
                *p.cast::<i32>() = i32::from(info.time_sig_num);
            });
        ok = ok
            && write_typed(urid.time_beat_unit, urid.atom_int, 4, &|p| {
                *p.cast::<i32>() = i32::from(info.time_sig_den);
            });
        if !ok {
            // Drop the whole notify event if any property didn't fit.
            // The notify-out port's `rsz:minimumSize 4096` is sized
            // for the worst case (~150B for this object) so we
            // shouldn't hit this in practice - but the bail makes the
            // wire format strictly correct rather than relying on the
            // size declaration.
            return;
        }

        // `prop_offset` already includes `obj_header_size` - it started at
        // that value and advanced for each property written.
        (*ev_ptr).body.size = len_u32(prop_offset);
        // Sequence size = body header + event header + event body size.
        let event_total = ev_header_size + prop_offset;
        (*out).atom.size = len_u32(body_header + event_total);
    }
}

// Dead-import quiet: keep c_void referenced so future extension code
// compiles without edits.
const _: Option<*mut c_void> = None;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::urid::UridMap;

    /// Build a `UridMap` by hand so tests don't need a real LV2 host.
    /// Every URI gets a unique deterministic id.
    fn test_urid_map() -> UridMap {
        let mut u = UridMap::default();
        // Any non-zero values will do - the codec only compares for equality.
        u.midi_event = 1;
        u.atom_sequence = 2;
        u.atom_chunk = 3;
        u.atom_blank = 4;
        u.atom_object = 5;
        u.atom_bool = 6;
        u.atom_int = 7;
        u.atom_long = 8;
        u.atom_float = 9;
        u.atom_double = 10;
        u.time_position = 100;
        u.time_bar = 101;
        u.time_bar_beat = 102;
        u.time_beat = 103;
        u.time_beat_unit = 104;
        u.time_beats_per_bar = 105;
        u.time_beats_per_minute = 106;
        u.time_frame = 107;
        u.time_speed = 108;
        u
    }

    #[test]
    fn time_position_roundtrip() {
        let urid = test_urid_map();
        let mut buf = vec![0u8; 4096];
        let seq = buf.as_mut_ptr().cast::<AtomSequence>();
        // Caller contract: atom.size = capacity on entry.
        unsafe {
            (*seq).atom.size = len_u32(buf.len() - core::mem::size_of::<Atom>());
        }

        // `bar_start_beats` must align to a whole-bar boundary for the
        // LV2 `time:bar` + `time:barBeat` round-trip to recover the
        // original `position_beats` exactly. Here: 7/8 time, bar 2
        // starts at beat 14, and we're 2.25 beats into it.
        let source = TransportInfo {
            playing: true,
            recording: false,
            tempo: 132.5,
            time_sig_num: 7,
            time_sig_den: 8,
            position_samples: 48000,
            position_seconds: 0.0,
            position_beats: 16.25,
            bar_start_beats: 14.0,
            loop_active: false,
            loop_start_beats: 0.0,
            loop_end_beats: 0.0,
        };

        unsafe {
            write_time_position_sequence(seq, &source, &urid);
        }

        let mut decoded = TransportInfo::default();
        let reader = AtomSequenceReader::new(seq.cast_const(), &urid);
        assert!(reader.apply_time_position(&mut decoded));

        assert!(decoded.playing, "playing flag round-tripped");
        assert!((decoded.tempo - source.tempo).abs() < 1e-9);
        assert!((decoded.position_beats - source.position_beats).abs() < 1e-9);
        assert!((decoded.bar_start_beats - source.bar_start_beats).abs() < 1e-9);
        assert_eq!(decoded.position_samples, source.position_samples);
        assert_eq!(decoded.time_sig_num, source.time_sig_num);
        assert_eq!(decoded.time_sig_den, source.time_sig_den);
    }

    /// Encode a small MIDI stream through `write_midi_out_sequence`,
    /// read it back via `AtomSequenceReader::for_each_midi` +
    /// `midi_bytes_to_event`, and check the round-trip is lossless.
    ///
    /// Independent codec-level regression guard: the port-layout fix
    /// in `truce-derive` is the reason MIDI works at all for
    /// note-effect plugins, but this test pins the codec separately
    /// so future refactors don't regress either layer silently.
    #[test]
    fn midi_roundtrip() {
        let urid = test_urid_map();
        let mut buf = vec![0u8; 4096];
        let seq = buf.as_mut_ptr().cast::<AtomSequence>();
        unsafe {
            (*seq).atom.size = len_u32(buf.len() - core::mem::size_of::<Atom>());
        }

        let mut source = EventList::default();
        source.push(Event::new(
            0,
            EventBody::NoteOn {
                group: 0,
                channel: 0,
                note: 60,
                velocity: 95,
            },
        ));
        source.push(Event::new(
            128,
            EventBody::NoteOff {
                group: 0,
                channel: 0,
                note: 60,
                velocity: 0,
            },
        ));
        source.push(Event::new(
            256,
            EventBody::ControlChange {
                group: 0,
                channel: 3,
                cc: 7,
                value: 64,
            },
        ));

        unsafe {
            write_midi_out_sequence(seq, &source, &urid, 0, 1);
        }

        let reader = AtomSequenceReader::new(seq.cast_const(), &urid);
        let mut decoded = Vec::new();
        reader.for_each_midi(|sample_offset, bytes| {
            if let Some(event) = midi_bytes_to_event(sample_offset, bytes) {
                decoded.push(event);
            }
        });

        assert_eq!(decoded.len(), source.len(), "all events round-tripped");
        assert_eq!(decoded[0].sample_offset, 0);
        assert_eq!(decoded[1].sample_offset, 128);
        assert_eq!(decoded[2].sample_offset, 256);

        match decoded[0].body {
            EventBody::NoteOn {
                channel,
                note,
                velocity,
                ..
            } => {
                assert_eq!(channel, 0);
                assert_eq!(note, 60);
                assert_eq!(velocity, 95);
            }
            _ => panic!("expected NoteOn at index 0, got {:?}", decoded[0].body),
        }
        match decoded[1].body {
            EventBody::NoteOff { channel, note, .. } => {
                assert_eq!(channel, 0);
                assert_eq!(note, 60);
            }
            _ => panic!("expected NoteOff at index 1, got {:?}", decoded[1].body),
        }
        match decoded[2].body {
            EventBody::ControlChange {
                channel, cc, value, ..
            } => {
                assert_eq!(channel, 3);
                assert_eq!(cc, 7);
                assert_eq!(value, 64);
            }
            _ => panic!(
                "expected ControlChange at index 2, got {:?}",
                decoded[2].body
            ),
        }
    }

    /// Build a single `patch:Set` Object atom event by hand (mirroring
    /// the layout a host like Ardour writes), pass it through
    /// `for_each_patch_set`, and verify the property URID + value +
    /// sample offset round-trip.
    ///
    /// Independent codec-level regression guard for the LV2 1.18+
    /// parameter automation path. A future refactor of the property
    /// walker (or a clippy auto-fix on the loop bookkeeping)
    /// shouldn't silently lose `patch:property` recognition.
    #[test]
    fn patch_set_roundtrip() {
        let mut urid = test_urid_map();
        urid.patch_set = 200;
        urid.patch_property = 201;
        urid.patch_value = 202;
        urid.patch_subject = 203;
        let target_property: Urid = 9001;

        let mut buf = vec![0u8; 4096];
        let seq = buf.as_mut_ptr().cast::<AtomSequence>();
        unsafe {
            (*seq).atom.size = len_u32(buf.len() - core::mem::size_of::<Atom>());
            (*seq).atom.type_ = urid.atom_sequence;
            (*seq).body.unit = 0;
            (*seq).body.pad = 0;
        }

        // Event layout:
        //   AtomEventHeader { time_frames: 432, body: { size, type=atom_object } }
        //   ObjectBody       { id: 0, otype: patch_set }
        //   Property         { key: patch_property, ctx: 0, atom { size=4, type=atom_int } } + i32
        //   Property         { key: patch_value,    ctx: 0, atom { size=4, type=atom_float } } + f32
        let prop_header = core::mem::size_of::<Urid>() * 2 + core::mem::size_of::<Atom>();
        let prop_total = prop_header + 4;
        let prop_padded = (prop_total + 7) & !7;
        let obj_body_size = core::mem::size_of::<Urid>() * 2 + prop_padded * 2;
        let event_size = core::mem::size_of::<AtomEventHeader>() + obj_body_size;
        let event_padded = (event_size + 7) & !7;

        unsafe {
            let body_offset =
                core::mem::size_of::<Atom>() + core::mem::size_of::<AtomSequenceBody>();
            let event_ptr = buf.as_mut_ptr().add(body_offset).cast::<AtomEventHeader>();
            (*event_ptr).time_frames = 432;
            (*event_ptr).body.size = len_u32(obj_body_size);
            (*event_ptr).body.type_ = urid.atom_object;

            let obj_body = buf
                .as_mut_ptr()
                .add(body_offset + core::mem::size_of::<AtomEventHeader>());
            // Object body header: id (0) + otype (patch_set).
            *obj_body.cast::<Urid>() = 0;
            *obj_body.add(core::mem::size_of::<Urid>()).cast::<Urid>() = urid.patch_set;

            let mut prop = obj_body.add(core::mem::size_of::<Urid>() * 2);
            // patch:property = target_property (atom:Int)
            *prop.cast::<Urid>() = urid.patch_property;
            *prop.add(core::mem::size_of::<Urid>()).cast::<Urid>() = 0;
            let atom_hdr = prop.add(core::mem::size_of::<Urid>() * 2).cast::<Atom>();
            (*atom_hdr).size = 4;
            (*atom_hdr).type_ = urid.atom_int;
            *prop.add(prop_header).cast::<i32>() = target_property.cast_signed();
            prop = prop.add(prop_padded);

            // patch:value = 0.625 (atom:Float)
            *prop.cast::<Urid>() = urid.patch_value;
            *prop.add(core::mem::size_of::<Urid>()).cast::<Urid>() = 0;
            let atom_hdr = prop.add(core::mem::size_of::<Urid>() * 2).cast::<Atom>();
            (*atom_hdr).size = 4;
            (*atom_hdr).type_ = urid.atom_float;
            *prop.add(prop_header).cast::<f32>() = 0.625;

            // Set the sequence atom.size = body header + first event padded.
            let total = core::mem::size_of::<AtomSequenceBody>() + event_padded;
            (*seq).atom.size = len_u32(total);
        }

        let reader = AtomSequenceReader::new(seq.cast_const(), &urid);
        let mut seen: Vec<(u32, Urid, f64)> = Vec::new();
        reader.for_each_patch_set(|sample_offset, property, value| {
            seen.push((sample_offset, property, value));
        });
        assert_eq!(seen.len(), 1, "exactly one patch:Set decoded");
        assert_eq!(seen[0].0, 432, "sample_offset = event.time_frames");
        assert_eq!(seen[0].1, target_property, "patch:property recovered");
        assert!(
            (seen[0].2 - 0.625).abs() < 1e-9,
            "patch:value f32 recovered"
        );
    }
}
