use truce_params::sample::Float;

/// Convert decibels to linear gain.
///
/// Generic over `f32` and `f64` via [`Float`]. The bound is `Float`
/// rather than `Sample` because a gain value isn't an audio sample
/// - it's a scaling coefficient.
#[inline]
#[must_use]
pub fn db_to_linear<F: Float>(db: F) -> F {
    (db * F::from_f64(std::f64::consts::LN_10 / 20.0)).exp()
}

/// Convert linear gain to decibels.
///
/// Generic over `f32` and `f64` - see [`db_to_linear`].
#[inline]
#[must_use]
pub fn linear_to_db<F: Float>(linear: F) -> F {
    F::from_f64(20.0) * linear.log10()
}

/// Convert a MIDI note number to frequency in Hz (A4 = 440 Hz).
///
/// Generic over `f32` and `f64` - see [`db_to_linear`]. A frequency
/// isn't an audio sample either; bound on [`Float`].
#[inline]
#[must_use]
pub fn midi_note_to_freq<F: Float>(note: u8) -> F {
    let semitones = F::from_f64(f64::from(note) - 69.0);
    F::from_f64(440.0) * F::from_f64(2.0).powf(semitones / F::from_f64(12.0))
}

/// Convert a linear peak level to a smoothed 0.0–1.0 display value for meters.
///
/// Maps -60 dB → 0.0, 0 dB → 1.0 (linear scale in dB domain).
/// Values above 0 dB clamp to 1.0. Silence (< -60 dB) maps to 0.0.
/// Apply smoothing externally (e.g., exponential decay per frame).
#[inline]
#[must_use]
pub fn meter_display(linear_peak: f32) -> f32 {
    if linear_peak < 1e-6 {
        return 0.0;
    }
    let db = 20.0 * linear_peak.log10();
    // Map -60..0 dB → 0.0..1.0
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

// `slugify` lives in `truce-utils` (dependency-free) so it can be
// shared with `cargo-truce` without forcing the `truce-core` →
// `truce-params` chain into the CLI's publish dependencies.
// Re-exported at the crate root via `pub use truce_utils::slugify`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_linear_round_trip_f64() {
        let db = -6.0_f64;
        let linear = db_to_linear(db);
        let back = linear_to_db(linear);
        assert!((back - db).abs() < 1e-10);
    }

    #[test]
    fn db_linear_round_trip_f32() {
        // f32 carries ~7 decimal digits; the round-trip survives
        // well under audible thresholds (1e-5 dB ≈ 200 dB below
        // unity).
        let db = -6.0_f32;
        let linear = db_to_linear(db);
        let back = linear_to_db(linear);
        assert!((back - db).abs() < 1e-5);
    }

    #[test]
    fn zero_db_is_unity_f64() {
        let linear: f64 = db_to_linear(0.0_f64);
        assert!((linear - 1.0).abs() < 1e-10);
    }

    #[test]
    fn zero_db_is_unity_f32() {
        let linear: f32 = db_to_linear(0.0_f32);
        assert!((linear - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a4_is_440_f64() {
        let freq: f64 = midi_note_to_freq(69);
        assert!((freq - 440.0).abs() < 1e-10);
    }

    #[test]
    fn a4_is_440_f32() {
        let freq: f32 = midi_note_to_freq(69);
        assert!((freq - 440.0).abs() < 1e-3);
    }
}
