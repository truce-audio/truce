//! MIDI value-domain helpers: normalize / denormalize between
//! wire-native integers and `f32` ranges.
//!
//! truce's `EventBody` carries MIDI events as wire-native integers
//! (7-bit `u8`, 14-bit `u16`, 16-bit `u16`, 32-bit `u32`) so the
//! framework's representation round-trips exactly with the wire.
//! Plugin code that wants to multiply by a parameter, accumulate
//! into a phase, or otherwise use the value as a float reaches for
//! the helpers below.
//!
//! Each pair (`norm_*` / `denorm_*`) round-trips for every
//! representable wire input. See the per-helper docs for endpoint
//! semantics - pitch-bend is asymmetric on both MIDI 1.0 and MIDI
//! 2.0 because the spec's center value sits one code closer to the
//! negative end than the positive.
//!
//! Lints: the helpers do `as`-casts at well-defined widening or
//! lossless points (`u8 → f32`, `u16 → f32`, `f64 → f32` after
//! a clamped multiply), so the `cast_*` lints are allowed at the
//! module level rather than per call.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

// ---------------------------------------------------------------------------
// 7-bit (MIDI 1.0 velocity / CC / aftertouch / channel pressure / program)
// ---------------------------------------------------------------------------

/// MIDI 1.0 7-bit unsigned (`0..=127`) → `f32 ∈ [0.0, 1.0]`.
///
/// `norm_7bit(0) == 0.0`, `norm_7bit(127) == 1.0`. Inputs above 127
/// debug-assert: the high bit is reserved as the MIDI status flag,
/// so a value here is a sign of caller bug (the wrapper-level demux
/// already strips the status bit).
#[inline]
#[must_use]
pub fn norm_7bit(v: u8) -> f32 {
    debug_assert!(
        v <= 127,
        "norm_7bit: {v} > 127 (high bit is the MIDI status flag)",
    );
    f32::from(v) / 127.0
}

/// `f32 ∈ [0.0, 1.0]` → MIDI 1.0 7-bit unsigned (`0..=127`).
///
/// Clamps and rounds half-to-even. Negative inputs land on `0`;
/// inputs ≥ 1.0 land on `127`. NaN debug-asserts; release builds
/// land on `0` (clamp returns the lower bound for unordered input).
#[inline]
#[must_use]
pub fn denorm_7bit(v: f32) -> u8 {
    debug_assert!(
        !v.is_nan(),
        "denorm_7bit: NaN input - caller's normalized value is uninitialized?",
    );
    (v.clamp(0.0, 1.0) * 127.0).round() as u8
}

// ---------------------------------------------------------------------------
// 14-bit pitch bend (MIDI 1.0)
// ---------------------------------------------------------------------------

/// MIDI 1.0 14-bit pitch-bend code (`0..=16383`) → `f32 ∈ [-1.0,
/// ~0.99987]`.
///
/// Center is `8192`. The mapping is asymmetric (8192 negative
/// codes, 8191 positive codes) because that is the MIDI 1.0
/// convention: `0` decodes to exactly `-1.0`, but the positive
/// endpoint stops at `8191/8192`. Inputs above 16383 debug-assert.
///
/// Round-trips exactly with [`denorm_pitch_bend`] for every
/// `raw ∈ [0, 16383]`.
#[inline]
#[must_use]
pub fn norm_pitch_bend(raw: u16) -> f32 {
    debug_assert!(
        raw <= 16383,
        "norm_pitch_bend: raw {raw} > 16383 - caller didn't mask LSB|MSB<<7?",
    );
    (f32::from(raw) - 8192.0) / 8192.0
}

/// `f32 ∈ [-1.0, 1.0]` → MIDI 1.0 14-bit pitch-bend code
/// (`0..=16383`).
///
/// Inverse of [`norm_pitch_bend`]. `-1.0` → `0`, `0.0` → `8192`,
/// `1.0` → `16383` (clamped - the perfectly symmetric `+1.0`
/// would be `16384`). NaN debug-asserts.
#[inline]
#[must_use]
pub fn denorm_pitch_bend(v: f32) -> u16 {
    debug_assert!(
        !v.is_nan(),
        "denorm_pitch_bend: NaN input - caller's normalized value is uninitialized?",
    );
    let raw = (v.clamp(-1.0, 1.0) * 8192.0 + 8192.0).round();
    (raw as u16).min(16383)
}

/// Split a 14-bit pitch-bend code into the (LSB, MSB) byte pair the
/// wire format carries. Each output byte has the high bit clear.
///
/// Used by every format wrapper's MIDI 1.0 output path. Unifies the
/// `(raw & 0x7F) as u8` / `((raw >> 7) & 0x7F) as u8` magic-constant
/// split that previously lived in six places.
#[inline]
#[must_use]
pub fn pitch_bend_to_bytes(raw: u16) -> (u8, u8) {
    debug_assert!(raw <= 16383, "pitch_bend_to_bytes: raw {raw} > 16383");
    let lsb = (raw & 0x7F) as u8;
    let msb = ((raw >> 7) & 0x7F) as u8;
    (lsb, msb)
}

/// Combine two MIDI bytes (LSB first) into a 14-bit pitch-bend code.
/// Each input byte's high bit is masked off before combining.
///
/// Inverse of [`pitch_bend_to_bytes`]. The masking matters: a
/// running-status parser may hand bytes that include the status
/// flag, and `(msb << 7) | lsb` without masking would corrupt the
/// result on out-of-domain input.
#[inline]
#[must_use]
pub fn pitch_bend_from_bytes(lsb: u8, msb: u8) -> u16 {
    (u16::from(msb & 0x7F) << 7) | u16::from(lsb & 0x7F)
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    // ---------- 7-bit ----------

    #[test]
    fn norm_7bit_endpoints() {
        assert_eq!(norm_7bit(0), 0.0);
        assert_eq!(norm_7bit(127), 1.0);
        assert!((norm_7bit(64) - (64.0 / 127.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn denorm_7bit_endpoints() {
        assert_eq!(denorm_7bit(0.0), 0);
        assert_eq!(denorm_7bit(1.0), 127);
        assert_eq!(denorm_7bit(0.5), 64); // round-half-to-even via .round()
    }

    #[test]
    fn denorm_7bit_clamps() {
        assert_eq!(denorm_7bit(-0.5), 0);
        assert_eq!(denorm_7bit(2.0), 127);
        assert_eq!(denorm_7bit(f32::INFINITY), 127);
        assert_eq!(denorm_7bit(f32::NEG_INFINITY), 0);
    }

    #[test]
    fn round_trip_7bit_all_codes() {
        // Every representable 7-bit value normalizes and denormalizes
        // back to itself.
        for raw in 0u8..=127 {
            assert_eq!(denorm_7bit(norm_7bit(raw)), raw);
        }
    }

    // ---------- 14-bit pitch bend ----------

    #[test]
    fn norm_pitch_bend_endpoints() {
        assert_eq!(norm_pitch_bend(0), -1.0);
        assert_eq!(norm_pitch_bend(8192), 0.0);
        // Asymmetric positive endpoint: 8191 / 8192 ≈ 0.99987.
        let max_pos = norm_pitch_bend(16383);
        assert!((max_pos - 8191.0_f32 / 8192.0_f32).abs() < f32::EPSILON);
    }

    #[test]
    fn denorm_pitch_bend_endpoints() {
        assert_eq!(denorm_pitch_bend(-1.0), 0);
        assert_eq!(denorm_pitch_bend(0.0), 8192);
        assert_eq!(denorm_pitch_bend(1.0), 16383);
    }

    #[test]
    fn round_trip_pitch_bend_all_codes() {
        for raw in 0u16..=16383 {
            let v = norm_pitch_bend(raw);
            let back = denorm_pitch_bend(v);
            assert_eq!(back, raw, "raw={raw}, v={v}");
        }
    }

    #[test]
    fn pitch_bend_byte_split_round_trip() {
        for raw in 0u16..=16383 {
            let (lsb, msb) = pitch_bend_to_bytes(raw);
            assert!(lsb < 128 && msb < 128);
            assert_eq!(pitch_bend_from_bytes(lsb, msb), raw);
        }
    }

    #[test]
    fn pitch_bend_from_bytes_masks_high_bit() {
        // Status-flag bits in either byte must not corrupt the result.
        assert_eq!(pitch_bend_from_bytes(0xFF, 0xFF), 16383);
        assert_eq!(pitch_bend_from_bytes(0x80, 0x80), 0);
    }
}
