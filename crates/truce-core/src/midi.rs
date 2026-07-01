//! MIDI 1.0 byte-stream codec.
//!
//! `parse_midi1` decodes a slice of MIDI 1.0 bytes (channel-voice
//! status `0x80..=0xEF`) into an [`EventBody`]; `event_to_midi1`
//! does the inverse. Both work on wire-native integers - see
//! [`crate::events`] for the value-domain rationale, and the
//! re-exported [`truce_utils::midi`] helpers (below) for normalize /
//! denormalize when plugin code wants `f32`.
//!
//! System common (`0xF1..=0xF7`), system real-time (`0xF8..=0xFF`),
//! and `SysEx` (`0xF0`) return `None` from [`parse_midi1`] - the
//! framework's [`EventBody`] doesn't model them. Format wrappers
//! that care must inspect the raw bytes themselves.
//!
//! Re-exports [`truce_utils::midi`]'s helpers so plugin code that
//! reaches for `truce_core::midi` finds both the codec and the
//! value-domain helpers in one module.

pub use truce_utils::midi::*;

use crate::events::EventBody;

/// Decode one MIDI 1.0 channel-voice short message into an
/// [`EventBody`].
///
/// Takes the three wire bytes as scalars rather than a slice so
/// format wrappers (CLAP / VST3 / VST2 / AU / AAX) can hand the
/// host's per-event `status` / `data1` / `data2` fields directly
/// without copying into a buffer. The `group` field on the
/// returned event is `0`; callers demuxing UMP-Type-2 packets that
/// carry a real group index should write it on the returned event.
///
/// Two-byte messages (`ProgramChange`, `ChannelPressure`) ignore
/// `data2`. `data1` and `data2` are masked to the 7-bit MIDI 1.0
/// data range before use so an out-of-spec high bit on either byte
/// can't corrupt the decoded value.
///
/// Returns `None` for status bytes outside `0x80..=0xEF`
/// (system-common, system-real-time, and `SysEx` are not modeled
/// by [`EventBody`]; wrappers that care must inspect raw bytes).
#[must_use]
pub fn decode_short_message(status: u8, data1: u8, data2: u8) -> Option<EventBody> {
    let channel = status & 0x0F;
    let d1 = data1 & 0x7F;
    let d2 = data2 & 0x7F;
    match status & 0xF0 {
        0x90 if d2 > 0 => Some(EventBody::NoteOn {
            group: 0,
            channel,
            note: d1,
            velocity: d2,
        }),
        // MIDI 1.0 quirk: NoteOn with velocity 0 is a NoteOff.
        0x90 => Some(EventBody::NoteOff {
            group: 0,
            channel,
            note: d1,
            velocity: 0,
        }),
        0x80 => Some(EventBody::NoteOff {
            group: 0,
            channel,
            note: d1,
            velocity: d2,
        }),
        0xA0 => Some(EventBody::Aftertouch {
            group: 0,
            channel,
            note: d1,
            pressure: d2,
        }),
        0xB0 => Some(EventBody::ControlChange {
            group: 0,
            channel,
            cc: d1,
            value: d2,
        }),
        0xC0 => Some(EventBody::ProgramChange {
            group: 0,
            channel,
            program: d1,
        }),
        0xD0 => Some(EventBody::ChannelPressure {
            group: 0,
            channel,
            pressure: d1,
        }),
        0xE0 => Some(EventBody::PitchBend {
            group: 0,
            channel,
            value: pitch_bend_from_bytes(d1, d2),
        }),
        _ => None,
    }
}

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
    let status = bytes[0];
    // Two-byte messages (`ProgramChange`, `ChannelPressure`) need
    // only `bytes[1]`; three-byte messages need both. `data2` is
    // unread for two-byte forms, so a zero fill is sound.
    let (data1, data2) = match status & 0xF0 {
        0xC0 | 0xD0 if bytes.len() >= 2 => (bytes[1], 0),
        0x80..=0xB0 | 0xE0 if bytes.len() >= 3 => (bytes[1], bytes[2]),
        _ => return None,
    };
    let mut event = decode_short_message(status, data1, data2)?;
    // `decode_short_message` always fills `group = 0`; rewrite if
    // the caller supplied a UMP group.
    rewrite_group(&mut event, group);
    Some(event)
}

fn rewrite_group(event: &mut EventBody, new_group: u8) {
    match event {
        EventBody::NoteOn { group, .. }
        | EventBody::NoteOff { group, .. }
        | EventBody::Aftertouch { group, .. }
        | EventBody::ChannelPressure { group, .. }
        | EventBody::ControlChange { group, .. }
        | EventBody::PitchBend { group, .. }
        | EventBody::ProgramChange { group, .. } => *group = new_group,
        _ => {}
    }
}

/// Encode an [`EventBody`] into a MIDI 1.0 byte stream.
///
/// Returns `(length, bytes)` - `length` is `2` for `ChannelPressure`
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

/// Down-convert a MIDI 2.0 channel-voice [`EventBody`] to its nearest
/// MIDI 1.0 equivalent (lossy). Used two ways: when a plug-in *emits*
/// 2.0 but the wrapper has no UMP transport (VST3 / VST2 / AAX / LV2),
/// and when a plug-in that did **not** opt into MIDI 2.0
/// (`midi_input_dialect == Midi1`) *receives* 2.0 - it should see 1.0
/// rather than have the event dropped.
///
/// 16/32-bit values take their high 7/14 bits; a live `NoteOn2` keeps a
/// non-zero velocity so it can't collapse into a note-off. Per-note
/// richness (`PerNoteCC` / `PerNotePitchBend`) collapses onto the note's
/// channel - the note identity is lost but the controller stays visible
/// (MPE-style degradation). Returns `None` for bodies that are already
/// MIDI 1.0, aren't channel voice, or have no 1.0 form (per-note
/// management, (N)RPN controllers).
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn downconvert_to_midi1(body: &EventBody) -> Option<EventBody> {
    let hi7_16 = |v: u16| (v >> 9) as u8;
    let hi7_32 = |v: u32| (v >> 25) as u8;
    let hi14_32 = |v: u32| (v >> 18) as u16 & 0x3FFF;
    Some(match *body {
        EventBody::NoteOn2 {
            group,
            channel,
            note,
            velocity,
            ..
        } => EventBody::NoteOn {
            group,
            channel,
            note,
            velocity: hi7_16(velocity).max(u8::from(velocity > 0)),
        },
        EventBody::NoteOff2 {
            group,
            channel,
            note,
            velocity,
            ..
        } => EventBody::NoteOff {
            group,
            channel,
            note,
            velocity: hi7_16(velocity),
        },
        EventBody::PolyPressure2 {
            group,
            channel,
            note,
            pressure,
        } => EventBody::Aftertouch {
            group,
            channel,
            note,
            pressure: hi7_32(pressure),
        },
        EventBody::ControlChange2 {
            group,
            channel,
            cc,
            value,
        }
        | EventBody::PerNoteCC {
            group,
            channel,
            cc,
            value,
            ..
        } => EventBody::ControlChange {
            group,
            channel,
            cc,
            value: hi7_32(value),
        },
        EventBody::ChannelPressure2 {
            group,
            channel,
            pressure,
        } => EventBody::ChannelPressure {
            group,
            channel,
            pressure: hi7_32(pressure),
        },
        EventBody::PitchBend2 {
            group,
            channel,
            value,
        }
        | EventBody::PerNotePitchBend {
            group,
            channel,
            value,
            ..
        } => EventBody::PitchBend {
            group,
            channel,
            value: hi14_32(value),
        },
        EventBody::ProgramChange2 {
            group,
            channel,
            program,
            ..
        } => EventBody::ProgramChange {
            group,
            channel,
            program,
        },
        _ => return None,
    })
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
    fn downconvert_note_on_2_keeps_high_bits() {
        let out = downconvert_to_midi1(&EventBody::NoteOn2 {
            group: 2,
            channel: 3,
            note: 60,
            velocity: 0xFFFF,
            attribute_type: 0,
            attribute: 0,
        });
        assert!(matches!(
            out,
            Some(EventBody::NoteOn {
                group: 2,
                channel: 3,
                note: 60,
                velocity: 127,
            })
        ));
    }

    #[test]
    fn downconvert_keeps_live_note_off_the_note_off_boundary() {
        // A tiny non-zero 16-bit velocity must not become a note-off.
        let Some(EventBody::NoteOn { velocity, .. }) = downconvert_to_midi1(&EventBody::NoteOn2 {
            group: 0,
            channel: 0,
            note: 60,
            velocity: 1,
            attribute_type: 0,
            attribute: 0,
        }) else {
            panic!("expected NoteOn");
        };
        assert_eq!(velocity, 1);
    }

    #[test]
    fn downconvert_per_note_collapses_to_channel_cc() {
        let out = downconvert_to_midi1(&EventBody::PerNoteCC {
            group: 0,
            channel: 4,
            note: 60,
            cc: 74,
            value: u32::MAX,
            registered: true,
        });
        assert!(matches!(
            out,
            Some(EventBody::ControlChange {
                channel: 4,
                cc: 74,
                value: 127,
                ..
            })
        ));
    }

    #[test]
    fn downconvert_returns_none_for_1_0_and_non_cv() {
        assert!(
            downconvert_to_midi1(&EventBody::NoteOn {
                group: 0,
                channel: 0,
                note: 60,
                velocity: 100,
            })
            .is_none()
        );
        assert!(downconvert_to_midi1(&EventBody::ParamChange { id: 0, value: 0.0 }).is_none());
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
            channel: 64, // 0x40 - high bit would flip 0x90 → 0xD0
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

    #[test]
    fn decode_program_change() {
        let event = decode_short_message(0xC3, 42, 0).unwrap();
        if let EventBody::ProgramChange {
            channel, program, ..
        } = event
        {
            assert_eq!(channel, 3);
            assert_eq!(program, 42);
        } else {
            panic!("expected ProgramChange, got {event:?}");
        }
    }

    #[test]
    fn decode_channel_pressure() {
        let event = decode_short_message(0xD5, 96, 0).unwrap();
        if let EventBody::ChannelPressure {
            channel, pressure, ..
        } = event
        {
            assert_eq!(channel, 5);
            assert_eq!(pressure, 96);
        } else {
            panic!("expected ChannelPressure, got {event:?}");
        }
    }

    #[test]
    fn decode_short_message_unknown_status_returns_none() {
        // System common / real-time / SysEx aren't modeled.
        assert!(decode_short_message(0xF0, 0, 0).is_none());
        assert!(decode_short_message(0xF8, 0, 0).is_none());
    }

    #[test]
    fn decode_short_message_strips_data_high_bit() {
        // Hosts shouldn't, but if they did, the helper masks the
        // 7-bit MIDI 1.0 data range so the decoded value stays in
        // domain.
        let event = decode_short_message(0xB0, 0xFF, 0xFF).unwrap();
        if let EventBody::ControlChange { cc, value, .. } = event {
            assert_eq!(cc, 0x7F);
            assert_eq!(value, 0x7F);
        } else {
            panic!("expected ControlChange, got {event:?}");
        }
    }
}
