//! Turn a captured [`LogEntry`] into human-readable text.
//!
//! Every event yields a `raw` line (its `Debug`, or a hex dump for
//! `SysEx`) so nothing is ever invisible - that's the "show what we
//! can't interpret yet" guarantee. Where truce understands the
//! message, a friendlier `summary` is filled in too. Adding a richer
//! interpretation later is a single match arm; the raw line keeps the
//! event legible until then.

use truce_core::events::EventBody;
use truce_core::midi::{norm_7bit, norm_pitch_bend};

use crate::ring::{LogEntry, SYSEX_INLINE};

/// Default pitch-bend range assumed for the semitone readout (the MIDI
/// default of +/-2 semitones). Purely cosmetic - the raw 14-bit code
/// is always shown alongside.
const PITCH_BEND_RANGE_ST: f32 = 2.0;

/// Coarse grouping used for the editor's filter toggles. MIDI 2.0
/// messages fold into the matching semantic group rather than a bucket
/// of their own; the `kind` string carries the "(MIDI 2.0)" marker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    Note,
    Cc,
    PitchBend,
    Pressure,
    Program,
    SysEx,
    Param,
    Transport,
    Other,
}

impl Category {
    /// All categories, in display order, with their toggle labels.
    pub const ALL: [(Category, &'static str); 9] = [
        (Category::Note, "Note"),
        (Category::Cc, "CC"),
        (Category::PitchBend, "Bend"),
        (Category::Pressure, "Press"),
        (Category::Program, "Prog"),
        (Category::SysEx, "SysEx"),
        (Category::Param, "Param"),
        (Category::Transport, "Transp"),
        (Category::Other, "Other"),
    ];
}

/// A fully interpreted event ready to render.
#[derive(Clone, Debug)]
pub struct Interpreted {
    pub category: Category,
    pub kind: &'static str,
    /// Wire channel `0..=15`, when the message is channel-addressed.
    pub channel: Option<u8>,
    pub summary: String,
    pub raw: String,
}

/// Note name for a MIDI note number, e.g. `60 -> "C4"`.
#[must_use]
pub fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let octave = i32::from(note) / 12 - 1;
    format!("{}{}", NAMES[(note % 12) as usize], octave)
}

/// Friendly name for the common control-change numbers; `None` for the
/// rest (the number alone is shown).
#[must_use]
pub fn cc_name(cc: u8) -> Option<&'static str> {
    Some(match cc {
        0 => "Bank Select MSB",
        1 => "Mod Wheel",
        2 => "Breath",
        4 => "Foot",
        5 => "Portamento Time",
        6 => "Data Entry MSB",
        7 => "Volume",
        8 => "Balance",
        10 => "Pan",
        11 => "Expression",
        32 => "Bank Select LSB",
        64 => "Sustain",
        65 => "Portamento",
        66 => "Sostenuto",
        67 => "Soft Pedal",
        71 => "Resonance",
        74 => "Brightness",
        91 => "Reverb",
        93 => "Chorus",
        120 => "All Sound Off",
        121 => "Reset All Controllers",
        123 => "All Notes Off",
        _ => return None,
    })
}

/// Manufacturer / universal label for a `SysEx` payload from its
/// leading id byte(s).
#[must_use]
pub fn sysex_source(bytes: &[u8]) -> &'static str {
    match bytes.first() {
        None => "empty",
        Some(0x7E) => "Universal Non-Realtime",
        Some(0x7F) => "Universal Realtime",
        Some(0x7D) => "Non-Commercial / Educational",
        Some(0x00) => "Extended id (00 xx xx)",
        Some(0x41) => "Roland",
        Some(0x42) => "Korg",
        Some(0x43) => "Yamaha",
        Some(0x40) => "Kawai",
        Some(0x44) => "Casio",
        Some(0x47) => "Akai",
        Some(_) => "Manufacturer",
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Format a 7-bit value with its normalized form, e.g. `100 (0.79)`.
fn v7(value: u8) -> String {
    format!("{value} ({:.2})", norm_7bit(value))
}

/// Interpret a captured entry. Pure - no GUI or plugin state, so it's
/// straightforward to unit test.
//
// One arm per `EventBody` variant - long by nature, and the
// exhaustive listing doubles as documentation of the taxonomy.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn interpret(entry: &LogEntry) -> Interpreted {
    // Raw fallback: the Debug form for everything except SysEx (whose
    // Debug is just pool indices - useless to a human), which gets a
    // hex dump built from the inlined bytes.
    let raw = match &entry.body {
        EventBody::SysEx { .. } => {
            let n = (entry.sysex_len as usize).min(SYSEX_INLINE);
            let tail = if entry.sysex_len as usize > SYSEX_INLINE {
                format!(" … +{} more", entry.sysex_len as usize - SYSEX_INLINE)
            } else {
                String::new()
            };
            format!("SysEx [{}]{}", hex(&entry.sysex[..n]), tail)
        }
        body => format!("{body:?}"),
    };

    let (category, kind, channel, summary) = match entry.body {
        // -- MIDI 1.0 channel voice --
        EventBody::NoteOn {
            channel,
            note,
            velocity,
            ..
        } => (
            Category::Note,
            "Note On",
            Some(channel),
            format!("{} vel {}", note_name(note), v7(velocity)),
        ),
        EventBody::NoteOff {
            channel,
            note,
            velocity,
            ..
        } => (
            Category::Note,
            "Note Off",
            Some(channel),
            format!("{} vel {}", note_name(note), v7(velocity)),
        ),
        EventBody::Aftertouch {
            channel,
            note,
            pressure,
            ..
        } => (
            Category::Pressure,
            "Poly Aftertouch",
            Some(channel),
            format!("{} {}", note_name(note), v7(pressure)),
        ),
        EventBody::ChannelPressure {
            channel, pressure, ..
        } => (
            Category::Pressure,
            "Channel Pressure",
            Some(channel),
            v7(pressure),
        ),
        EventBody::ControlChange {
            channel, cc, value, ..
        } => {
            let label = cc_name(cc).map_or_else(|| format!("CC{cc}"), |n| format!("CC{cc} {n}"));
            (
                Category::Cc,
                "Control Change",
                Some(channel),
                format!("{label} = {}", v7(value)),
            )
        }
        EventBody::PitchBend { channel, value, .. } => {
            let norm = norm_pitch_bend(value);
            (
                Category::PitchBend,
                "Pitch Bend",
                Some(channel),
                format!(
                    "raw {value} -> {norm:+.3} ({:+.2} st)",
                    norm * PITCH_BEND_RANGE_ST
                ),
            )
        }
        EventBody::ProgramChange {
            channel, program, ..
        } => (
            Category::Program,
            "Program Change",
            Some(channel),
            format!("program {program}"),
        ),

        // -- MIDI 2.0 channel voice (16/32-bit) --
        EventBody::NoteOn2 {
            channel,
            note,
            velocity,
            ..
        } => (
            Category::Note,
            "Note On (MIDI 2.0)",
            Some(channel),
            format!(
                "{} vel {velocity} ({:.2})",
                note_name(note),
                f32::from(velocity) / f32::from(u16::MAX)
            ),
        ),
        EventBody::NoteOff2 {
            channel,
            note,
            velocity,
            ..
        } => (
            Category::Note,
            "Note Off (MIDI 2.0)",
            Some(channel),
            format!(
                "{} vel {velocity} ({:.2})",
                note_name(note),
                f32::from(velocity) / f32::from(u16::MAX)
            ),
        ),
        EventBody::PolyPressure2 {
            channel,
            note,
            pressure,
            ..
        } => (
            Category::Pressure,
            "Poly Pressure (MIDI 2.0)",
            Some(channel),
            format!("{} {pressure}", note_name(note)),
        ),
        EventBody::PerNoteCC {
            channel,
            note,
            cc,
            value,
            registered,
            ..
        } => (
            Category::Cc,
            "Per-Note CC (MIDI 2.0)",
            Some(channel),
            format!(
                "{} {} CC{cc} = {value}",
                note_name(note),
                if registered { "(reg)" } else { "(asgn)" }
            ),
        ),
        EventBody::PerNotePitchBend {
            channel,
            note,
            value,
            ..
        } => (
            Category::PitchBend,
            "Per-Note Bend (MIDI 2.0)",
            Some(channel),
            format!("{} raw {value} (center 0x80000000)", note_name(note)),
        ),
        EventBody::PerNoteManagement {
            channel,
            note,
            flags,
            ..
        } => (
            Category::Other,
            "Per-Note Mgmt (MIDI 2.0)",
            Some(channel),
            format!("{} flags 0x{flags:02X}", note_name(note)),
        ),
        EventBody::ControlChange2 {
            channel, cc, value, ..
        } => {
            let label = cc_name(cc).map_or_else(|| format!("CC{cc}"), |n| format!("CC{cc} {n}"));
            (
                Category::Cc,
                "Control Change (MIDI 2.0)",
                Some(channel),
                format!("{label} = {value}"),
            )
        }
        EventBody::ChannelPressure2 {
            channel, pressure, ..
        } => (
            Category::Pressure,
            "Channel Pressure (MIDI 2.0)",
            Some(channel),
            format!("{pressure}"),
        ),
        EventBody::PitchBend2 { channel, value, .. } => (
            Category::PitchBend,
            "Pitch Bend (MIDI 2.0)",
            Some(channel),
            format!("raw {value} (center 0x80000000)"),
        ),
        EventBody::ProgramChange2 {
            channel,
            program,
            bank,
            ..
        } => {
            let bank = bank.map_or_else(
                || "no bank".to_string(),
                |(msb, lsb)| format!("bank {msb}/{lsb}"),
            );
            (
                Category::Program,
                "Program Change (MIDI 2.0)",
                Some(channel),
                format!("program {program} ({bank})"),
            )
        }
        EventBody::RegisteredController {
            channel,
            bank,
            index,
            value,
            ..
        } => (
            Category::Cc,
            "Registered Controller (MIDI 2.0)",
            Some(channel),
            format!("RPN {bank}/{index} = {value}"),
        ),
        EventBody::AssignableController {
            channel,
            bank,
            index,
            value,
            ..
        } => (
            Category::Cc,
            "Assignable Controller (MIDI 2.0)",
            Some(channel),
            format!("NRPN {bank}/{index} = {value}"),
        ),

        // -- truce-internal automation --
        EventBody::ParamChange { id, value } => (
            Category::Param,
            "Param Change",
            None,
            format!("param #{id} = {value:.4}"),
        ),
        EventBody::ParamMod { id, note_id, value } => (
            Category::Param,
            "Param Modulation",
            None,
            format!("param #{id} note {note_id} += {value:.4}"),
        ),

        // -- transport --
        EventBody::Transport(t) => (
            Category::Transport,
            "Transport",
            None,
            format!(
                "{:.2} bpm {}/{} {} bar@{:.2}",
                t.tempo,
                t.time_sig_num,
                t.time_sig_den,
                if t.playing { "play" } else { "stop" },
                t.position_beats,
            ),
        ),

        // -- system --
        EventBody::SysEx { .. } => {
            let n = (entry.sysex_len as usize).min(SYSEX_INLINE);
            (
                Category::SysEx,
                "SysEx",
                None,
                format!(
                    "{} bytes · {}",
                    entry.sysex_len,
                    sysex_source(&entry.sysex[..n])
                ),
            )
        }
    };

    Interpreted {
        category,
        kind,
        channel,
        summary,
        raw,
    }
}
