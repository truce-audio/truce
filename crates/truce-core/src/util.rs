use std::f64::consts::LN_10;

/// Convert decibels to linear gain.
#[inline]
#[must_use] 
pub fn db_to_linear(db: f64) -> f64 {
    (db * LN_10 / 20.0).exp()
}

/// Convert linear gain to decibels.
#[inline]
#[must_use] 
pub fn linear_to_db(linear: f64) -> f64 {
    20.0 * linear.log10()
}

/// Convert a MIDI note number to frequency in Hz (A4 = 440 Hz).
#[inline]
#[must_use] 
pub fn midi_note_to_freq(note: u8) -> f64 {
    440.0 * 2.0f64.powf((f64::from(note) - 69.0) / 12.0)
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

/// Slug a plugin's display name into a lowercase, hyphenated,
/// ASCII-safe identifier suitable for filesystem paths, LV2 bundle
/// names, and IRI components.
///
/// Rules: ASCII alphanumerics pass through lowercased; every other
/// character (including runs of them) collapses to a single `-`;
/// leading and trailing dashes are trimmed.
#[must_use] 
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
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
    fn slugify_basic() {
        assert_eq!(slugify("My Plugin"), "my-plugin");
        assert_eq!(slugify("Hello!! World"), "hello-world");
        assert_eq!(slugify("--leading and trailing--"), "leading-and-trailing");
        assert_eq!(slugify("ABC123"), "abc123");
        assert_eq!(slugify(""), "");
    }
}
