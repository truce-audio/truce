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

use std::ffi::c_void;

use truce_core::events::{Event, EventBody, EventList, TransportInfo};

use crate::urid::{Urid, UridMap};

/// Layout of `LV2_Atom` — type + size prefix.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Atom {
    pub size: u32,
    pub type_: Urid,
}

/// `LV2_Atom_Sequence_Body` — unit/pad prefix for a sequence body.
#[repr(C)]
pub struct AtomSequenceBody {
    pub unit: Urid,
    pub pad: u32,
}

/// Full `LV2_Atom_Sequence` — header then body then events. The port
/// pointer the host hands us points here.
#[repr(C)]
pub struct AtomSequence {
    pub atom: Atom,
    pub body: AtomSequenceBody,
    // Followed by event data; we walk it manually.
}

/// `LV2_Atom_Event` — per-event header. Time is in frames relative to the
/// start of the current `run()` block.
#[repr(C)]
#[derive(Clone, Copy)]
struct AtomEventHeader {
    time_frames: i64,
    body: Atom,
    // Body bytes follow this struct, padded to 8-byte alignment.
}

/// Reader that walks an `LV2_Atom_Sequence` and yields its events.
pub(crate) struct AtomSequenceReader<'a> {
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
                    f(frame.max(0) as u32, slice);
                }
            });
        }
    }

    /// Walk the sequence and update `info` from the last `time:Position`
    /// object encountered. Returns `true` if at least one such event was
    /// found.
    ///
    /// LV2 hosts typically emit one `time:Position` per run() block when
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
        let seq = &*self.seq;
        let body_size = seq.atom.size as usize;
        if body_size < core::mem::size_of::<AtomSequenceBody>() {
            return;
        }
        let data_size = body_size - core::mem::size_of::<AtomSequenceBody>();
        let data_start = (self.seq as *const u8).add(core::mem::size_of::<AtomSequence>());
        let mut offset = 0usize;
        while offset + core::mem::size_of::<AtomEventHeader>() <= data_size {
            let ev_ptr = data_start.add(offset) as *const AtomEventHeader;
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
        // LV2_Atom_Object_Body = { otype: Urid, id: Urid, props: Property[] }
        let header_size = core::mem::size_of::<Urid>() * 2;
        if body_bytes < header_size {
            return false;
        }
        let otype = *(body_ptr as *const Urid);
        if otype != self.urid.time_position {
            return false;
        }
        let mut offset = header_size;
        while offset + core::mem::size_of::<Urid>() * 2 + core::mem::size_of::<Atom>() <= body_bytes {
            // Property = { key: Urid, context: Urid, value: Atom + data }
            let key = *(body_ptr.add(offset) as *const Urid);
            // `context` is unused by time:Position writers in practice.
            let value_header = body_ptr.add(offset + core::mem::size_of::<Urid>() * 2);
            let value_atom = *(value_header as *const Atom);
            let value_data = value_header.add(core::mem::size_of::<Atom>());
            let value_size = value_atom.size as usize;
            let entry_total = core::mem::size_of::<Urid>() * 2
                + core::mem::size_of::<Atom>()
                + value_size;
            let padded = (entry_total + 7) & !7;

            if let Some(v) = self.read_atom_number(value_atom.type_, value_data, value_size) {
                if key == self.urid.time_beats_per_minute {
                    info.tempo = v;
                } else if key == self.urid.time_beat || key == self.urid.time_bar_beat {
                    info.position_beats = v;
                } else if key == self.urid.time_bar {
                    info.bar_start_beats = v;
                } else if key == self.urid.time_frame {
                    info.position_samples = v as i64;
                } else if key == self.urid.time_speed {
                    info.playing = v.abs() > 1e-9;
                } else if key == self.urid.time_beats_per_bar {
                    info.time_sig_num = v.round().clamp(0.0, u8::MAX as f64) as u8;
                } else if key == self.urid.time_beat_unit {
                    info.time_sig_den = v.round().clamp(0.0, u8::MAX as f64) as u8;
                }
            }

            offset += padded;
            if padded == 0 {
                break;
            }
        }
        true
    }

    /// Read a numeric atom value as f64, handling the common number types
    /// LV2 hosts use for time:Position fields.
    unsafe fn read_atom_number(
        &self,
        atom_type: Urid,
        data: *const u8,
        size: usize,
    ) -> Option<f64> {
        if atom_type == self.urid.atom_float && size >= core::mem::size_of::<f32>() {
            Some(*(data as *const f32) as f64)
        } else if atom_type == self.urid.atom_double && size >= core::mem::size_of::<f64>() {
            Some(*(data as *const f64))
        } else if atom_type == self.urid.atom_int && size >= core::mem::size_of::<i32>() {
            Some(*(data as *const i32) as f64)
        } else if atom_type == self.urid.atom_long && size >= core::mem::size_of::<i64>() {
            Some(*(data as *const i64) as f64)
        } else if atom_type == self.urid.atom_bool && size >= core::mem::size_of::<i32>() {
            Some(if *(data as *const i32) != 0 { 1.0 } else { 0.0 })
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Decode raw MIDI bytes into truce EventBody variants
// ---------------------------------------------------------------------------

pub fn midi_bytes_to_event(sample_offset: u32, bytes: &[u8]) -> Option<Event> {
    if bytes.is_empty() {
        return None;
    }
    let status = bytes[0];
    let channel = status & 0x0F;
    let body = match status & 0xF0 {
        0x80 if bytes.len() >= 3 => EventBody::NoteOff {
            channel,
            note: bytes[1] & 0x7F,
            velocity: (bytes[2] & 0x7F) as f32 / 127.0,
        },
        0x90 if bytes.len() >= 3 => {
            let vel = bytes[2] & 0x7F;
            if vel == 0 {
                EventBody::NoteOff {
                    channel,
                    note: bytes[1] & 0x7F,
                    velocity: 0.0,
                }
            } else {
                EventBody::NoteOn {
                    channel,
                    note: bytes[1] & 0x7F,
                    velocity: vel as f32 / 127.0,
                }
            }
        }
        0xA0 if bytes.len() >= 3 => EventBody::Aftertouch {
            channel,
            note: bytes[1] & 0x7F,
            pressure: (bytes[2] & 0x7F) as f32 / 127.0,
        },
        0xB0 if bytes.len() >= 3 => EventBody::ControlChange {
            channel,
            cc: bytes[1] & 0x7F,
            value: (bytes[2] & 0x7F) as f32 / 127.0,
        },
        0xC0 if bytes.len() >= 2 => EventBody::ProgramChange {
            channel,
            program: bytes[1] & 0x7F,
        },
        0xD0 if bytes.len() >= 2 => EventBody::ChannelPressure {
            channel,
            pressure: (bytes[1] & 0x7F) as f32 / 127.0,
        },
        0xE0 if bytes.len() >= 3 => {
            let raw = ((bytes[2] as u16 & 0x7F) << 7) | (bytes[1] as u16 & 0x7F);
            EventBody::PitchBend {
                channel,
                // Normalize to [-1, 1] with 0x2000 as center.
                value: (raw as f32 - 8192.0) / 8192.0,
            }
        }
        _ => return None,
    };
    Some(Event {
        sample_offset,
        body,
    })
}

// ---------------------------------------------------------------------------
// Encode truce EventList into an LV2_Atom_Sequence output port
// ---------------------------------------------------------------------------

/// Overwrite the port's sequence body with MIDI events from `events`. Sets
/// the proper header/atom sizes so the host knows how many bytes to read.
///
/// # Safety
/// `out` must point to a writable atom sequence buffer with capacity the
/// host allocated (typically a few KB).
pub unsafe fn write_midi_out_sequence(
    out: *mut AtomSequence,
    events: &EventList,
    urid: &UridMap,
) {
    if out.is_null() || urid.midi_event == 0 {
        return;
    }
    // Host passes us a sequence where atom.size is the *capacity* of the
    // body buffer on entry. We overwrite it with the actual size on exit.
    let capacity = (*out).atom.size as usize;
    let atom_size = core::mem::size_of::<Atom>();
    let header_size = core::mem::size_of::<AtomSequenceBody>();
    let body_start = (out as *mut u8).add(atom_size + header_size);
    let mut offset = 0usize;
    // Reset sequence metadata.
    (*out).atom.type_ = urid.atom_sequence;
    (*out).body.unit = 0;
    (*out).body.pad = 0;
    for event in events.iter() {
        let mut buf = [0u8; 3];
        let (n, frame) = match &event.body {
            EventBody::NoteOn {
                channel,
                note,
                velocity,
            } => {
                buf[0] = 0x90 | (channel & 0x0F);
                buf[1] = note & 0x7F;
                buf[2] = ((velocity * 127.0).clamp(0.0, 127.0)) as u8;
                (3, event.sample_offset)
            }
            EventBody::NoteOff {
                channel,
                note,
                velocity,
            } => {
                buf[0] = 0x80 | (channel & 0x0F);
                buf[1] = note & 0x7F;
                buf[2] = ((velocity * 127.0).clamp(0.0, 127.0)) as u8;
                (3, event.sample_offset)
            }
            EventBody::ControlChange { channel, cc, value } => {
                buf[0] = 0xB0 | (channel & 0x0F);
                buf[1] = cc & 0x7F;
                buf[2] = ((value * 127.0).clamp(0.0, 127.0)) as u8;
                (3, event.sample_offset)
            }
            _ => continue,
        };
        let total = core::mem::size_of::<AtomEventHeader>() + n;
        let padded = (total + 7) & !7;
        if offset + padded > capacity {
            break; // out of buffer space; drop remaining events
        }
        let ev_ptr = body_start.add(offset) as *mut AtomEventHeader;
        (*ev_ptr).time_frames = frame as i64;
        (*ev_ptr).body.size = n as u32;
        (*ev_ptr).body.type_ = urid.midi_event;
        let body_ptr = body_start.add(offset + core::mem::size_of::<AtomEventHeader>());
        core::ptr::copy_nonoverlapping(buf.as_ptr(), body_ptr, n);
        // Zero the padding bytes.
        for i in n..(padded - core::mem::size_of::<AtomEventHeader>()) {
            *body_ptr.add(i) = 0;
        }
        offset += padded;
    }
    (*out).atom.size = (header_size + offset) as u32;
}

// Dead-import quiet: keep c_void referenced so future extension code
// compiles without edits.
const _: Option<*mut c_void> = None;
