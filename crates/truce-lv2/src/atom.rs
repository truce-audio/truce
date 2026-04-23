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
        // LV2_Atom_Object_Body per lv2/atom/atom.h:
        //   { uint32_t id; uint32_t otype; }
        // id is a per-object instance identifier (0 for blank); otype is
        // the class URID we key on.
        let header_size = core::mem::size_of::<Urid>() * 2;
        if body_bytes < header_size {
            return false;
        }
        let otype = *(body_ptr.add(core::mem::size_of::<Urid>()) as *const Urid);
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
                } else if key == self.urid.time_bar {
                    bar = Some(v);
                } else if key == self.urid.time_bar_beat {
                    bar_beat = Some(v);
                } else if key == self.urid.time_beats_per_bar {
                    beats_per_bar = Some(v);
                    info.time_sig_num = v.round().clamp(0.0, u8::MAX as f64) as u8;
                } else if key == self.urid.time_beat {
                    // Non-standard but some hosts still emit it — treat
                    // it as the absolute beat position directly.
                    info.position_beats = v;
                } else if key == self.urid.time_frame {
                    info.position_samples = v as i64;
                } else if key == self.urid.time_speed {
                    info.playing = v.abs() > 1e-9;
                } else if key == self.urid.time_beat_unit {
                    info.time_sig_den = v.round().clamp(0.0, u8::MAX as f64) as u8;
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
            .or_else(|| {
                if info.time_sig_num > 0 {
                    Some(info.time_sig_num as f64)
                } else {
                    None
                }
            })
            .unwrap_or(4.0);
        if let Some(b) = bar {
            info.bar_start_beats = b * bpb;
            if let Some(bb) = bar_beat {
                info.position_beats = info.bar_start_beats + bb;
            }
        } else if let Some(bb) = bar_beat {
            // No bar field — best we can do is surface the intra-bar
            // offset as the position, matching our previous behavior
            // for hosts that only emit `time:barBeat`.
            info.position_beats = bb;
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

/// Write a single `time:Position` atom object into a notify-out sequence.
/// Called each run() block so the UI's `port_event` receives the latest
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
    if out.is_null() || urid.time_position == 0 || urid.atom_object == 0 {
        return;
    }
    let capacity = (*out).atom.size as usize;
    let atom_size = core::mem::size_of::<Atom>();
    let body_header = core::mem::size_of::<AtomSequenceBody>();
    let body_start = (out as *mut u8).add(atom_size + body_header);

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
    let ev_ptr = body_start as *mut AtomEventHeader;
    if ev_header_size + obj_header_size > capacity {
        (*out).atom.size = body_header as u32;
        return;
    }
    (*ev_ptr).time_frames = 0;
    (*ev_ptr).body.type_ = urid.atom_object;
    let obj_body_start = body_start.add(ev_header_size);
    // Per lv2/atom/atom.h: `id` first, then `otype`.
    *(obj_body_start as *mut Urid) = 0; // id = blank
    *(obj_body_start.add(core::mem::size_of::<Urid>()) as *mut Urid) = urid.time_position;

    let mut prop_offset = obj_header_size;
    let prop_header_size = core::mem::size_of::<Urid>() * 2 + core::mem::size_of::<Atom>();

    let mut write_double = |key: Urid, value: f64| -> bool {
        if key == 0 || urid.atom_double == 0 {
            return false;
        }
        let value_size = core::mem::size_of::<f64>();
        let total = prop_header_size + value_size;
        let padded = (total + 7) & !7;
        if ev_header_size + prop_offset + padded > capacity {
            return false;
        }
        let entry = obj_body_start.add(prop_offset);
        *(entry as *mut Urid) = key;
        *(entry.add(core::mem::size_of::<Urid>()) as *mut Urid) = 0; // context
        let atom_hdr = entry.add(core::mem::size_of::<Urid>() * 2) as *mut Atom;
        (*atom_hdr).size = value_size as u32;
        (*atom_hdr).type_ = urid.atom_double;
        let value_ptr = entry.add(prop_header_size);
        *(value_ptr as *mut f64) = value;
        prop_offset += padded;
        true
    };

    write_double(urid.time_speed, if info.playing { 1.0 } else { 0.0 });
    write_double(urid.time_beats_per_minute, info.tempo);
    write_double(urid.time_beat, info.position_beats);
    write_double(urid.time_bar_beat, info.position_beats);
    write_double(urid.time_bar, info.bar_start_beats);
    write_double(urid.time_frame, info.position_samples as f64);
    write_double(urid.time_beats_per_bar, info.time_sig_num as f64);
    write_double(urid.time_beat_unit, info.time_sig_den as f64);

    // `prop_offset` already includes `obj_header_size` — it started at
    // that value and advanced for each property written.
    (*ev_ptr).body.size = prop_offset as u32;
    // Sequence size = body header + event header + event body size.
    let event_total = ev_header_size + prop_offset;
    (*out).atom.size = (body_header + event_total) as u32;
}

// Dead-import quiet: keep c_void referenced so future extension code
// compiles without edits.
const _: Option<*mut c_void> = None;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::urid::UridMap;

    /// Build a UridMap by hand so tests don't need a real LV2 host.
    /// Every URI gets a unique deterministic id.
    fn test_urid_map() -> UridMap {
        let mut u = UridMap::default();
        // Any non-zero values will do — the codec only compares for equality.
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
        let seq = buf.as_mut_ptr() as *mut AtomSequence;
        // Caller contract: atom.size = capacity on entry.
        unsafe {
            (*seq).atom.size =
                (buf.len() - core::mem::size_of::<Atom>()) as u32;
        }

        let source = TransportInfo {
            playing: true,
            recording: false,
            tempo: 132.5,
            time_sig_num: 7,
            time_sig_den: 8,
            position_samples: 48000,
            position_seconds: 0.0,
            position_beats: 16.25,
            bar_start_beats: 16.0,
            loop_active: false,
            loop_start_beats: 0.0,
            loop_end_beats: 0.0,
        };

        unsafe {
            write_time_position_sequence(seq, &source, &urid);
        }

        let mut decoded = TransportInfo::default();
        let reader = AtomSequenceReader::new(seq as *const AtomSequence, &urid);
        assert!(reader.apply_time_position(&mut decoded));

        assert!(decoded.playing, "playing flag round-tripped");
        assert!((decoded.tempo - source.tempo).abs() < 1e-9);
        assert!((decoded.position_beats - source.position_beats).abs() < 1e-9);
        assert!((decoded.bar_start_beats - source.bar_start_beats).abs() < 1e-9);
        assert_eq!(decoded.position_samples, source.position_samples);
        assert_eq!(decoded.time_sig_num, source.time_sig_num);
        assert_eq!(decoded.time_sig_den, source.time_sig_den);
    }
}
