use crate::events::EventBody;

/// Upconvert MIDI 1.0 bytes to our event representation.
pub fn parse_midi1(bytes: &[u8]) -> Option<EventBody> {
    if bytes.is_empty() {
        return None;
    }

    let status = bytes[0] & 0xF0;
    let channel = bytes[0] & 0x0F;

    match status {
        0x90 if bytes.len() >= 3 && bytes[2] > 0 => Some(EventBody::NoteOn {
            channel,
            note: bytes[1],
            velocity: bytes[2] as f32 / 127.0,
        }),
        0x90 if bytes.len() >= 3 => Some(EventBody::NoteOff {
            channel,
            note: bytes[1],
            velocity: 0.0,
        }),
        0x80 if bytes.len() >= 3 => Some(EventBody::NoteOff {
            channel,
            note: bytes[1],
            velocity: bytes[2] as f32 / 127.0,
        }),
        0xA0 if bytes.len() >= 3 => Some(EventBody::Aftertouch {
            channel,
            note: bytes[1],
            pressure: bytes[2] as f32 / 127.0,
        }),
        0xB0 if bytes.len() >= 3 => Some(EventBody::ControlChange {
            channel,
            cc: bytes[1],
            value: bytes[2] as f32 / 127.0,
        }),
        0xD0 if bytes.len() >= 2 => Some(EventBody::ChannelPressure {
            channel,
            pressure: bytes[1] as f32 / 127.0,
        }),
        0xE0 if bytes.len() >= 3 => {
            let raw = ((bytes[2] as u16) << 7) | (bytes[1] as u16);
            let normalized = (raw as f32 - 8192.0) / 8192.0;
            Some(EventBody::PitchBend {
                channel,
                value: normalized,
            })
        }
        0xC0 if bytes.len() >= 2 => Some(EventBody::ProgramChange {
            channel,
            program: bytes[1],
        }),
        _ => None,
    }
}

/// Downconvert our events to MIDI 1.0 bytes.
pub fn event_to_midi1(event: &EventBody) -> Option<[u8; 3]> {
    match event {
        EventBody::NoteOn {
            channel,
            note,
            velocity,
        } => Some([0x90 | channel, *note, (*velocity * 127.0).round() as u8]),
        EventBody::NoteOff {
            channel,
            note,
            velocity,
        } => Some([0x80 | channel, *note, (*velocity * 127.0).round() as u8]),
        EventBody::ControlChange { channel, cc, value } => {
            Some([0xB0 | channel, *cc, (*value * 127.0).round() as u8])
        }
        EventBody::PitchBend { channel, value } => {
            let raw = ((*value + 1.0) * 8192.0).round() as u16;
            let raw = raw.min(16383);
            Some([
                0xE0 | channel,
                (raw & 0x7F) as u8,
                ((raw >> 7) & 0x7F) as u8,
            ])
        }
        EventBody::ChannelPressure { channel, pressure } => {
            Some([0xD0 | channel, (*pressure * 127.0).round() as u8, 0])
        }
        EventBody::ProgramChange { channel, program } => Some([0xC0 | channel, *program, 0]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_note_on() {
        let bytes = [0x90, 60, 100];
        let event = parse_midi1(&bytes).unwrap();
        let back = event_to_midi1(&event).unwrap();
        assert_eq!(back[0], 0x90);
        assert_eq!(back[1], 60);
        assert_eq!(back[2], 100);
    }

    #[test]
    fn round_trip_pitch_bend() {
        let bytes = [0xE0, 0x00, 0x40]; // center
        let event = parse_midi1(&bytes).unwrap();
        if let EventBody::PitchBend { value, .. } = event {
            assert!((value).abs() < 0.01);
        } else {
            panic!("expected PitchBend");
        }
    }

    #[test]
    fn note_on_zero_velocity_is_note_off() {
        let bytes = [0x90, 60, 0];
        let event = parse_midi1(&bytes).unwrap();
        assert!(matches!(event, EventBody::NoteOff { .. }));
    }
}
