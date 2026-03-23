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

use crossterm::event::KeyCode;

/// Map a QWERTY key to a MIDI note number, or None if unmapped.
pub fn key_to_midi_note(key: KeyCode, octave_offset: i8) -> Option<u8> {
    let base: i16 = 48 + (octave_offset as i16 * 12); // C3 default

    let offset: i16 = match key {
        // Lower row: white keys C D E F G A B C D E
        KeyCode::Char('a') => 0,  // C
        KeyCode::Char('s') => 2,  // D
        KeyCode::Char('d') => 4,  // E
        KeyCode::Char('f') => 5,  // F
        KeyCode::Char('g') => 7,  // G
        KeyCode::Char('h') => 9,  // A
        KeyCode::Char('j') => 11, // B
        KeyCode::Char('k') => 12, // C (next octave)
        KeyCode::Char('l') => 14, // D
        KeyCode::Char(';') => 16, // E

        // Upper row: black keys
        KeyCode::Char('w') => 1,  // C#
        KeyCode::Char('e') => 3,  // D#
        KeyCode::Char('t') => 6,  // F#
        KeyCode::Char('y') => 8,  // G#
        KeyCode::Char('u') => 10, // A#
        KeyCode::Char('o') => 13, // C# (next octave)
        KeyCode::Char('p') => 15, // D# (next octave)

        _ => return None,
    };

    let note = base + offset;
    if (0..=127).contains(&note) {
        Some(note as u8)
    } else {
        None
    }
}

/// Map key for octave shift.
pub fn key_to_octave_shift(key: KeyCode) -> Option<i8> {
    match key {
        KeyCode::Char('z') => Some(-1), // octave down
        KeyCode::Char('x') => Some(1),  // octave up
        _ => None,
    }
}
