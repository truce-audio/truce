//! QWERTY keyboard to MIDI note mapping.
//!
//! Two rows of keys map to a piano keyboard starting at C3 (MIDI 48):
//!
//! ```text
//!  Upper: W E   T Y U   O P
//!        C#D#  F#G#A#  C#D#
//! Lower: A S D F G H J K L ;
//!        C D E F G A B C D E
//! ```
//!
//! Keys are matched by physical `keyboard_types::Code` so the mapping
//! is keyboard-layout-independent. AZERTY / Dvorak / etc. all hit the
//! same physical piano layout.

#[cfg(feature = "gui")]
use keyboard_types::Code;

/// Map a physical QWERTY key (by `keyboard_types::Code`) to a MIDI
/// note number, shifted by `octave_offset` octaves. Returns `None`
/// for keys not on the piano layout.
#[cfg(feature = "gui")]
pub fn code_to_midi_note(code: Code, octave_offset: i8) -> Option<u8> {
    let base: i16 = 48 + (octave_offset as i16 * 12); // C3 default

    let offset: i16 = match code {
        // Lower row: white keys C D E F G A B C D E
        Code::KeyA => 0,
        Code::KeyS => 2,
        Code::KeyD => 4,
        Code::KeyF => 5,
        Code::KeyG => 7,
        Code::KeyH => 9,
        Code::KeyJ => 11,
        Code::KeyK => 12,
        Code::KeyL => 14,
        Code::Semicolon => 16,

        // Upper row: black keys
        Code::KeyW => 1,
        Code::KeyE => 3,
        Code::KeyT => 6,
        Code::KeyY => 8,
        Code::KeyU => 10,
        Code::KeyO => 13,
        Code::KeyP => 15,

        _ => return None,
    };

    let note = base + offset;
    if (0..=127).contains(&note) {
        Some(note as u8)
    } else {
        None
    }
}

/// Map `Z` / `X` to `-1` / `+1` octave shift.
#[cfg(feature = "gui")]
pub fn code_to_octave_shift(code: Code) -> Option<i8> {
    match code {
        Code::KeyZ => Some(-1),
        Code::KeyX => Some(1),
        _ => None,
    }
}
