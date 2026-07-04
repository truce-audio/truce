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
/// 16/32-bit values take their high 7/14 bits; a `NoteOn2` downconverts
/// with velocity at least 1 (the spec's 2.0 -> 1.0 translation rule) -
/// 2.0's velocity 0 is a real note-on, while a 1.0 velocity-0 `NoteOn`
/// is a note-off. Per-note
/// richness (`PerNoteCC` / `PerNotePitchBend`) collapses onto the note's
/// channel - the note identity is lost but the controller stays visible
/// (MPE-style degradation); registered per-note controllers only, since
/// assignable indices are manufacturer-defined and correspond to no CC.
/// `ProgramChange2`'s bank pair is dropped: MIDI 1.0 spells it as a
/// separate CC 0 / CC 32 pair *before* the program change, and this
/// one-in-one-out API can't emit three events for one - callers that
/// need the bank must expand it themselves.
/// Returns `None` for bodies that are already MIDI 1.0, aren't channel
/// voice, or have no 1.0 form (per-note management, assignable per-note
/// controllers, (N)RPN controllers).
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
            velocity: hi7_16(velocity).max(1),
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
        // Only *registered* per-note controllers mirror the MIDI 1.0
        // CC numbering (index 7 = volume, 74 = brightness, ...);
        // assignable indices are manufacturer-defined, so collapsing
        // one onto the same-numbered channel CC would alias it onto an
        // unrelated standard controller. Drop assignables instead.
        EventBody::PerNoteCC {
            registered: false, ..
        } => return None,
        // A per-note CC index >= 128 (MIDI 2.0 carries an 8-bit index)
        // has no MIDI 1.0 CC number - a 1.0 CC data byte is 7-bit.
        // Emitting it verbatim sets bit 7, which a 1.0 parser reads as
        // a status byte and desyncs the stream. Drop it rather than
        // alias it onto a real controller by masking.
        EventBody::ControlChange2 { cc, .. } | EventBody::PerNoteCC { cc, .. } if cc >= 128 => {
            return None;
        }
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

/// MIDI 2.0 min-center-max up-scaling (M2-104 §2.2): shift into the
/// wider domain, then bit-repeat the low source bits above the source
/// center. Center and max map exactly (a plain shift would leave max
/// short of full-scale), and [`downconvert_to_midi1`]'s high-bit take
/// recovers the original value.
fn upscale(src: u32, src_bits: u32, dst_bits: u32) -> u32 {
    let scale_bits = dst_bits - src_bits;
    let shifted = src << scale_bits;
    let center = 1u32 << (src_bits - 1);
    if src <= center {
        return shifted;
    }
    let repeat_bits = src_bits - 1;
    let mut repeat = src & ((1 << repeat_bits) - 1);
    if scale_bits > repeat_bits {
        repeat <<= scale_bits - repeat_bits;
    } else {
        repeat >>= repeat_bits - scale_bits;
    }
    let mut out = shifted;
    while repeat != 0 {
        out |= repeat;
        repeat >>= repeat_bits;
    }
    out
}

/// 7-bit -> 16-bit spec up-scaling: exact center (`64 -> 0x8000`) and
/// exact max (`127 -> 0xFFFF`), unlike a plain shift or linear rescale.
#[must_use]
pub fn upscale_7_to_16(v: u8) -> u16 {
    // Bounded by the 16-bit target width.
    #[allow(clippy::cast_possible_truncation)]
    let out = upscale(u32::from(v), 7, 16) as u16;
    out
}

/// 7-bit -> 32-bit spec up-scaling (see [`upscale_7_to_16`]).
#[must_use]
pub fn upscale_7_to_32(v: u8) -> u32 {
    upscale(u32::from(v), 7, 32)
}

/// 14-bit -> 32-bit spec up-scaling: `0x2000 -> 0x8000_0000` exactly,
/// so a centered 1.0 pitch bend stays a centered 2.0 bend.
#[must_use]
pub fn upscale_14_to_32(v: u16) -> u32 {
    upscale(u32::from(v), 14, 32)
}

/// Up-convert a MIDI 1.0 channel-voice [`EventBody`] to its MIDI 2.0
/// equivalent. Used when a wrapper emits into a MIDI 2.0 protocol
/// stream (AU v3's `MIDIEventList`): the UMP spec forbids mixing 1.0
/// channel-voice packets into a 2.0-protocol stream, so 1.0 bodies
/// widen first. A `NoteOn` with velocity 0 becomes a `NoteOff2` at
/// center velocity, mirroring the spec's 1.0 -> 2.0 translation rule.
/// Returns `None` for bodies that are already 2.0 or aren't channel
/// voice.
#[must_use]
pub fn upconvert_to_midi2(body: &EventBody) -> Option<EventBody> {
    Some(match *body {
        EventBody::NoteOn {
            group,
            channel,
            note,
            velocity: 0,
        } => EventBody::NoteOff2 {
            group,
            channel,
            note,
            velocity: 0x8000,
            attribute_type: 0,
            attribute: 0,
        },
        EventBody::NoteOn {
            group,
            channel,
            note,
            velocity,
        } => EventBody::NoteOn2 {
            group,
            channel,
            note,
            velocity: upscale_7_to_16(velocity),
            attribute_type: 0,
            attribute: 0,
        },
        EventBody::NoteOff {
            group,
            channel,
            note,
            velocity,
        } => EventBody::NoteOff2 {
            group,
            channel,
            note,
            velocity: upscale_7_to_16(velocity),
            attribute_type: 0,
            attribute: 0,
        },
        EventBody::Aftertouch {
            group,
            channel,
            note,
            pressure,
        } => EventBody::PolyPressure2 {
            group,
            channel,
            note,
            pressure: upscale_7_to_32(pressure),
        },
        EventBody::ControlChange {
            group,
            channel,
            cc,
            value,
        } => EventBody::ControlChange2 {
            group,
            channel,
            cc,
            value: upscale_7_to_32(value),
        },
        EventBody::ChannelPressure {
            group,
            channel,
            pressure,
        } => EventBody::ChannelPressure2 {
            group,
            channel,
            pressure: upscale_7_to_32(pressure),
        },
        EventBody::PitchBend {
            group,
            channel,
            value,
        } => EventBody::PitchBend2 {
            group,
            channel,
            value: upscale_14_to_32(value),
        },
        EventBody::ProgramChange {
            group,
            channel,
            program,
        } => EventBody::ProgramChange2 {
            group,
            channel,
            program,
            bank: None,
        },
        _ => return None,
    })
}

/// Route a MIDI port index onto a plugin's declared port count: an
/// in-range port passes through, anything else lands on port 0 - the
/// default/main port, not the arbitrary last one. Shared by every
/// wrapper's port stamping/routing so out-of-range handling can't
/// drift per format (the VST3 shim implements the same rule in C++).
#[must_use]
pub fn route_midi_port(port: u8, count: u8) -> u8 {
    if port < count { port } else { 0 }
}

/// Centre of a 32-bit per-note pitch bend (`0x8000_0000`), as `f64`.
const PER_NOTE_BEND_CENTER: f64 = 2_147_483_648.0;

/// Per-note pitch-bend full-scale, in semitones each way (the MPE
/// convention). Every wrapper that maps [`EventBody::PerNotePitchBend`]
/// onto a semitone-denominated host domain (CLAP's `TUNING`
/// expression, VST3's tuning type) scales through this constant, so
/// the same event bends identically across formats.
pub const PER_NOTE_TUNING_SEMITONES: f64 = 48.0;

/// Per-note volume full-scale, in linear gain. CLAP's `VOLUME`
/// expression (plain gain `0..=4`) and VST3's volume type (normalized
/// `0..=1` with `plain = 20 * log(4 * norm)`) define the same physical
/// domain; the wire's quarter point is unity gain (0 dB).
pub const PER_NOTE_VOLUME_MAX_GAIN: f64 = 4.0;

/// 32-bit per-note pitch bend (centre `0x8000_0000`) -> semitones,
/// full-scale [`PER_NOTE_TUNING_SEMITONES`] each way.
#[must_use]
pub fn per_note_bend_semitones(v: u32) -> f64 {
    ((f64::from(v) - PER_NOTE_BEND_CENTER) / PER_NOTE_BEND_CENTER) * PER_NOTE_TUNING_SEMITONES
}

/// Semitones -> 32-bit per-note pitch bend (centre `0x8000_0000`),
/// saturating at full-scale - the wire can't express a wider bend.
#[must_use]
pub fn per_note_bend_from_semitones(semis: f64) -> u32 {
    // Clamped to `0..=u32::MAX` before the cast, so no truncation or
    // sign loss is possible (NaN saturates to 0 under `as`).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let v = (PER_NOTE_BEND_CENTER + (semis / PER_NOTE_TUNING_SEMITONES) * PER_NOTE_BEND_CENTER)
        .clamp(0.0, f64::from(u32::MAX)) as u32;
    v
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
        // Any NoteOn2 - a tiny 16-bit velocity, and 2.0's genuine
        // velocity-0 note-on alike - must downconvert with velocity
        // at least 1 (the spec's translation rule): a 1.0 velocity-0
        // NoteOn is a note-off, which would silently release the note
        // instead of sounding it.
        for velocity2 in [0u16, 1] {
            let Some(EventBody::NoteOn { velocity, .. }) =
                downconvert_to_midi1(&EventBody::NoteOn2 {
                    group: 0,
                    channel: 0,
                    note: 60,
                    velocity: velocity2,
                    attribute_type: 0,
                    attribute: 0,
                })
            else {
                panic!("expected NoteOn");
            };
            assert_eq!(velocity, 1);
        }
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
    fn downconvert_drops_out_of_range_per_note_cc() {
        // A per-note CC index >= 128 has no MIDI 1.0 CC number; emitting
        // it would put bit 7 in a data byte and desync the 1.0 stream.
        for cc in [128u8, 200, 255] {
            assert!(
                downconvert_to_midi1(&EventBody::PerNoteCC {
                    group: 0,
                    channel: 3,
                    note: 60,
                    cc,
                    value: u32::MAX,
                    registered: true,
                })
                .is_none(),
                "per-note CC index {cc} must drop, not emit a corrupt data byte"
            );
        }
        // A valid 7-bit index still down-converts.
        assert!(matches!(
            downconvert_to_midi1(&EventBody::PerNoteCC {
                group: 0,
                channel: 3,
                note: 60,
                cc: 127,
                value: 0,
                registered: true,
            }),
            Some(EventBody::ControlChange { cc: 127, .. })
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

    #[test]
    fn upconvert_upscales_center_and_max_exactly() {
        // Spec min-center-max scaling: 7-bit max fills the wide domain,
        // 7-bit center lands on the wide center (a plain shift does
        // neither).
        let up = |velocity| match upconvert_to_midi2(&EventBody::NoteOn {
            group: 0,
            channel: 0,
            note: 60,
            velocity,
        }) {
            Some(EventBody::NoteOn2 { velocity, .. }) => velocity,
            other => panic!("expected NoteOn2, got {other:?}"),
        };
        assert_eq!(up(127), u16::MAX);
        assert_eq!(up(64), 0x8000);
        assert_eq!(up(1), 1 << 9);

        // 14-bit pitch bend: max and center map exactly too.
        match upconvert_to_midi2(&EventBody::PitchBend {
            group: 0,
            channel: 0,
            value: 0x3FFF,
        }) {
            Some(EventBody::PitchBend2 { value, .. }) => assert_eq!(value, u32::MAX),
            other => panic!("expected PitchBend2, got {other:?}"),
        }
        match upconvert_to_midi2(&EventBody::PitchBend {
            group: 0,
            channel: 0,
            value: 0x2000,
        }) {
            Some(EventBody::PitchBend2 { value, .. }) => assert_eq!(value, 0x8000_0000),
            other => panic!("expected PitchBend2, got {other:?}"),
        }
    }

    #[test]
    fn upconvert_round_trips_through_downconvert() {
        let bodies = [
            EventBody::NoteOn {
                group: 0,
                channel: 3,
                note: 60,
                velocity: 100,
            },
            EventBody::NoteOff {
                group: 0,
                channel: 3,
                note: 60,
                velocity: 40,
            },
            EventBody::ControlChange {
                group: 0,
                channel: 1,
                cc: 74,
                value: 99,
            },
            EventBody::PitchBend {
                group: 0,
                channel: 0,
                value: 12345,
            },
            EventBody::ChannelPressure {
                group: 0,
                channel: 9,
                pressure: 77,
            },
            EventBody::Aftertouch {
                group: 0,
                channel: 2,
                note: 61,
                pressure: 5,
            },
        ];
        for body in bodies {
            let wide = upconvert_to_midi2(&body).expect("channel voice widens");
            assert_eq!(downconvert_to_midi1(&wide), Some(body));
        }
    }

    #[test]
    fn upconvert_maps_velocity_zero_note_on_to_note_off() {
        // The 1.0 -> 2.0 translation rule: a running-status-style
        // velocity-0 NoteOn is a release and must not survive as a
        // 2.0 NoteOn (whose velocity 0 is a real, audible note-on).
        match upconvert_to_midi2(&EventBody::NoteOn {
            group: 0,
            channel: 0,
            note: 60,
            velocity: 0,
        }) {
            Some(EventBody::NoteOff2 { velocity, .. }) => assert_eq!(velocity, 0x8000),
            other => panic!("expected NoteOff2, got {other:?}"),
        }
        // Already-2.0 and non-channel-voice bodies pass through as None.
        assert!(
            upconvert_to_midi2(&EventBody::NoteOn2 {
                group: 0,
                channel: 0,
                note: 60,
                velocity: 1,
                attribute_type: 0,
                attribute: 0,
            })
            .is_none()
        );
    }

    #[test]
    fn out_of_range_port_routes_to_zero() {
        assert_eq!(route_midi_port(1, 2), 1); // in range passes through
        assert_eq!(route_midi_port(2, 2), 0); // past the count -> default port
        assert_eq!(route_midi_port(0, 0), 0); // portless plugin
    }

    #[test]
    fn upscale_preserves_center_and_endpoints() {
        assert_eq!(upscale_14_to_32(0x2000), 0x8000_0000); // centered bend stays centered
        assert_eq!(upscale_14_to_32(0x3FFF), u32::MAX);
        assert_eq!(upscale_14_to_32(0), 0);
        assert_eq!(upscale_7_to_16(64), 0x8000);
        assert_eq!(upscale_7_to_32(127), u32::MAX);
    }

    #[test]
    fn assignable_per_note_cc_has_no_midi1_form() {
        // Assignable (registered: false) per-note controller indices
        // are manufacturer-defined - collapsing index 7 onto channel
        // CC 7 would alias it onto Volume.
        let assignable = EventBody::PerNoteCC {
            group: 0,
            channel: 0,
            note: 60,
            cc: 7,
            value: u32::MAX,
            registered: false,
        };
        assert_eq!(downconvert_to_midi1(&assignable), None);
        // The registered twin still degrades to the channel CC.
        let registered = EventBody::PerNoteCC {
            group: 0,
            channel: 0,
            note: 60,
            cc: 7,
            value: u32::MAX,
            registered: true,
        };
        assert!(matches!(
            downconvert_to_midi1(&registered),
            Some(EventBody::ControlChange { cc: 7, .. })
        ));
    }

    #[test]
    fn per_note_bend_semitone_round_trip() {
        // Centre is exactly no detune.
        assert!(per_note_bend_semitones(0x8000_0000).abs() < 1e-9);
        assert_eq!(per_note_bend_from_semitones(0.0), 0x8000_0000);
        // Full-scale each way.
        assert_eq!(per_note_bend_from_semitones(-PER_NOTE_TUNING_SEMITONES), 0);
        assert_eq!(
            per_note_bend_from_semitones(PER_NOTE_TUNING_SEMITONES),
            u32::MAX
        );
        assert!((per_note_bend_semitones(u32::MAX) - PER_NOTE_TUNING_SEMITONES).abs() < 1e-6);
        // Beyond full-scale saturates - the wire can't express it.
        assert_eq!(per_note_bend_from_semitones(-120.0), 0);
        assert_eq!(per_note_bend_from_semitones(120.0), u32::MAX);
        // Mid-range values survive the round trip.
        let wire = per_note_bend_from_semitones(12.0);
        assert!((per_note_bend_semitones(wire) - 12.0).abs() < 1e-6);
    }
}
