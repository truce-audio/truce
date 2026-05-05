//! MIDI 1.0 byte-stream codec.
//!
//! `parse_midi1` decodes a slice of MIDI 1.0 bytes (channel-voice
//! status `0x80..=0xEF`) into an [`EventBody`]; `event_to_midi1`
//! does the inverse. Both work on wire-native integers — see
//! [`crate::events`] for the value-domain rationale, and the
//! [`midi`](crate::midi_helpers) module's helpers for normalize /
//! denormalize when plugin code wants `f32`.
//!
//! System common (`0xF1..=0xF7`), system real-time (`0xF8..=0xFF`),
//! and `SysEx` (`0xF0`) return `None` from [`parse_midi1`] — the
//! framework's [`EventBody`] doesn't model them. Format wrappers
//! that care must inspect the raw bytes themselves.
//!
//! Re-exports [`truce_utils::midi`]'s helpers so plugin code that
//! reaches for `truce_core::midi` finds both the codec and the
//! value-domain helpers in one module.

pub use truce_utils::midi::*;

use crate::events::EventBody;

/// Decode a MIDI 1.0 channel-voice byte stream into an
/// [`EventBody`].
///
/// `group` is the UMP group index the host delivered the bytes
/// under (0..=15); legacy MIDI 1.0 byte streams that don't carry a
/// group field pass `0`. Wrappers that demux UMP-Type-2 packets
/// fill the actual group.
///
/// Returns `None` for status bytes outside `0x80..=0xEF`, for
/// truncated buffers, and for malformed encodings.
#[must_use]
pub fn parse_midi1(group: u8, bytes: &[u8]) -> Option<EventBody> {
    if bytes.is_empty() {
        return None;
    }

    let status = bytes[0] & 0xF0;
    let channel = bytes[0] & 0x0F;

    match status {
        0x90 if bytes.len() >= 3 && bytes[2] > 0 => Some(EventBody::NoteOn {
            group,
            channel,
            note: bytes[1],
            velocity: bytes[2],
        }),
        // MIDI 1.0 quirk: NoteOn with velocity 0 is a NoteOff.
        0x90 if bytes.len() >= 3 => Some(EventBody::NoteOff {
            group,
            channel,
            note: bytes[1],
            velocity: 0,
        }),
        0x80 if bytes.len() >= 3 => Some(EventBody::NoteOff {
            group,
            channel,
            note: bytes[1],
            velocity: bytes[2],
        }),
        0xA0 if bytes.len() >= 3 => Some(EventBody::Aftertouch {
            group,
            channel,
            note: bytes[1],
            pressure: bytes[2],
        }),
        0xB0 if bytes.len() >= 3 => Some(EventBody::ControlChange {
            group,
            channel,
            cc: bytes[1],
            value: bytes[2],
        }),
        0xD0 if bytes.len() >= 2 => Some(EventBody::ChannelPressure {
            group,
            channel,
            pressure: bytes[1],
        }),
        0xE0 if bytes.len() >= 3 => Some(EventBody::PitchBend {
            group,
            channel,
            value: pitch_bend_from_bytes(bytes[1], bytes[2]),
        }),
        0xC0 if bytes.len() >= 2 => Some(EventBody::ProgramChange {
            group,
            channel,
            program: bytes[1],
        }),
        _ => None,
    }
}

/// Encode an [`EventBody`] into a MIDI 1.0 byte stream.
///
/// Returns `(length, bytes)` — `length` is `2` for `ChannelPressure`
/// and `ProgramChange`, `3` for everything else. Sinks must respect
/// the length: emitting all 3 bytes for a 2-byte status produces a
/// spurious trailing zero that a downstream parser interprets as a
/// running-status `NoteOff`.
///
/// Returns `None` for events that don't fit MIDI 1.0 (every MIDI
/// 2.0 variant, `ParamChange`, `ParamMod`, `Transport`). Callers
/// that want lossy down-conversion should explicitly call
/// `downconvert_*` helpers first.
///
/// All status bytes mask `channel & 0x0F` so an out-of-range
/// channel value can't corrupt the status byte itself.
#[must_use]
pub fn event_to_midi1(event: &EventBody) -> Option<(usize, [u8; 3])> {
    match event {
        EventBody::NoteOn {
            channel,
            note,
            velocity,
            ..
        } => Some((3, [0x90 | (channel & 0x0F), *note, *velocity])),
        EventBody::NoteOff {
            channel,
            note,
            velocity,
            ..
        } => Some((3, [0x80 | (channel & 0x0F), *note, *velocity])),
        EventBody::Aftertouch {
            channel,
            note,
            pressure,
            ..
        } => Some((3, [0xA0 | (channel & 0x0F), *note, *pressure])),
        EventBody::ControlChange {
            channel, cc, value, ..
        } => Some((3, [0xB0 | (channel & 0x0F), *cc, *value])),
        EventBody::PitchBend { channel, value, .. } => {
            let (lsb, msb) = pitch_bend_to_bytes(*value);
            Some((3, [0xE0 | (channel & 0x0F), lsb, msb]))
        }
        EventBody::ChannelPressure {
            channel, pressure, ..
        } => Some((2, [0xD0 | (channel & 0x0F), *pressure, 0])),
        EventBody::ProgramChange {
            channel, program, ..
        } => Some((2, [0xC0 | (channel & 0x0F), *program, 0])),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_note_on() {
        let bytes = [0x90, 60, 100];
        let event = parse_midi1(0, &bytes).unwrap();
        let (len, back) = event_to_midi1(&event).unwrap();
        assert_eq!(len, 3);
        assert_eq!(back, [0x90, 60, 100]);
    }

    #[test]
    fn round_trip_pitch_bend_center() {
        let bytes = [0xE0, 0x00, 0x40]; // center
        let event = parse_midi1(0, &bytes).unwrap();
        if let EventBody::PitchBend { value, .. } = event {
            assert_eq!(value, 8192);
        } else {
            panic!("expected PitchBend");
        }
    }

    #[test]
    fn note_on_zero_velocity_is_note_off() {
        let bytes = [0x90, 60, 0];
        let event = parse_midi1(0, &bytes).unwrap();
        assert!(matches!(event, EventBody::NoteOff { .. }));
    }

    #[test]
    fn channel_masked_on_encode() {
        // Out-of-range channel (the `EventBody` field is `u8` so
        // 16+ is reachable through user code) must not corrupt the
        // status byte.
        let event = EventBody::NoteOn {
            group: 0,
            channel: 64, // 0x40 — high bit would flip 0x90 → 0xD0
            note: 60,
            velocity: 100,
        };
        let (_len, bytes) = event_to_midi1(&event).unwrap();
        // Channel masked to 0, status byte is clean 0x90.
        assert_eq!(bytes[0], 0x90);
    }

    #[test]
    fn group_propagated_through_parse() {
        let event = parse_midi1(7, &[0x90, 60, 100]).unwrap();
        if let EventBody::NoteOn { group, .. } = event {
            assert_eq!(group, 7);
        } else {
            panic!("expected NoteOn");
        }
    }
}
