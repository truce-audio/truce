use std::f64::consts::LN_10;

/// Convert decibels to linear gain.
#[inline]
pub fn db_to_linear(db: f64) -> f64 {
    (db * LN_10 / 20.0).exp()
}

/// Convert linear gain to decibels.
#[inline]
pub fn linear_to_db(linear: f64) -> f64 {
    20.0 * linear.log10()
}

/// Convert a MIDI note number to frequency in Hz (A4 = 440 Hz).
#[inline]
pub fn midi_note_to_freq(note: u8) -> f64 {
    440.0 * 2.0f64.powf((note as f64 - 69.0) / 12.0)
}

/// Convert a frequency in Hz to a MIDI note number (fractional).
#[inline]
pub fn freq_to_midi_note(freq: f64) -> f64 {
    69.0 + 12.0 * (freq / 440.0).log2()
}

/// Convert a linear peak level to a smoothed 0.0–1.0 display value for meters.
///
/// Maps -60 dB → 0.0, 0 dB → 1.0 (linear scale in dB domain).
/// Values above 0 dB clamp to 1.0. Silence (< -60 dB) maps to 0.0.
/// Apply smoothing externally (e.g., exponential decay per frame).
#[inline]
pub fn meter_display(linear_peak: f32) -> f32 {
    if linear_peak < 1e-6 {
        return 0.0;
    }
    let db = 20.0 * linear_peak.log10();
    // Map -60..0 dB → 0.0..1.0
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_linear_round_trip() {
        let db = -6.0;
        let linear = db_to_linear(db);
        let back = linear_to_db(linear);
        assert!((back - db).abs() < 1e-10);
    }

    #[test]
    fn zero_db_is_unity() {
        let linear = db_to_linear(0.0);
        assert!((linear - 1.0).abs() < 1e-10);
    }

    #[test]
    fn a4_is_440() {
        let freq = midi_note_to_freq(69);
        assert!((freq - 440.0).abs() < 1e-10);
    }

    #[test]
    fn midi_freq_round_trip() {
        for note in 0..=127u8 {
            let freq = midi_note_to_freq(note);
            let back = freq_to_midi_note(freq);
            assert!((back - note as f64).abs() < 1e-10);
        }
    }
}
