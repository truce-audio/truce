//! Numeric-cast helpers for the audio-plugin → host FFI boundary.
//!
//! Audio-plugin code routinely casts at three points where Rust's
//! type system can't help:
//!
//! - **MIDI 7-bit normalize:** velocity / CC / pressure stored as
//!   `f32 ∈ [0.0, 1.0]` re-encodes to `u8 ∈ [0, 127]`.
//! - **Pitch bend (14-bit):** `f32 ∈ [-1.0, 1.0]` re-encodes to
//!   `u14` packed into the low 7 bits of two MIDI bytes.
//! - **FFI struct sizes / element counts:** `usize` (Rust) vs `u32`
//!   (every C ABI we ship to).
//!
//! Each helper is `#[inline]`, debug-asserts the input range so a
//! NaN-bearing or overflowing caller fails loud in tests, and is
//! the *only* place in the workspace that's allowed to reach for
//! `as` on its specific shape. The lints
//! `cast_possible_truncation`, `cast_sign_loss`, and
//! `cast_precision_loss` are allowed at the module level so the
//! helpers can do their job without per-site annotations.
//!
//! Adding new helpers: target shapes that show up at ≥ 10 sites
//! and have a uniform body. Single-site casts belong with a
//! per-site `#[allow]` and a sentence of `reason` text, not in
//! this module.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

/// Encode a normalized `f32` (in `[0.0, 1.0]`) to a 7-bit MIDI
/// byte (in `[0, 127]`).
///
/// Used for note velocity, CC value, channel pressure, polyphonic
/// pressure — everything MIDI 1.0 represents as a single byte
/// value with the high bit reserved.
///
/// The caller's value is clamped before scaling and rounded to
/// the nearest integer. Negative inputs land on `0`; inputs ≥ 1.0
/// land on `127`. NaN debug-asserts; in release it lands on `0`
/// because `clamp(0.0, 1.0)` returns the lower bound when the
/// input compares-unordered against both bounds.
#[inline]
#[must_use]
pub fn midi_7bit(v: f32) -> u8 {
    debug_assert!(
        !v.is_nan(),
        "midi_7bit: NaN input — caller's normalized value is uninitialized?",
    );
    (v.clamp(0.0, 1.0) * 127.0).round() as u8
}

/// Encode a bipolar `f32` (in `[-1.0, 1.0]`) to a 14-bit MIDI
/// pitch-bend value packed into the low 14 bits of a `u16`.
///
/// Caller is responsible for splitting the result into the LSB /
/// MSB bytes of the pitch-bend message:
///
/// ```ignore
/// let n = midi_14bit_pb(value);
/// let lsb = (n & 0x7F) as u8;
/// let msb = ((n >> 7) & 0x7F) as u8;
/// ```
///
/// `0` encodes the maximum-negative bend, `8192` is center,
/// `16383` is the maximum-positive bend. Inputs outside `[-1, 1]`
/// clamp to the endpoints. NaN debug-asserts.
#[inline]
#[must_use]
pub fn midi_14bit_pb(v: f32) -> u16 {
    debug_assert!(
        !v.is_nan(),
        "midi_14bit_pb: NaN input — caller's normalized value is uninitialized?",
    );
    ((v.clamp(-1.0, 1.0) + 1.0) * 8191.5).round() as u16
}

/// Cast a `usize` element count (`Vec::len()`, iterator count) to
/// `u32` for an FFI field.
///
/// Debug-asserts the value fits — a 5GB+ `Vec<u8>` would silently
/// truncate without this guard. Release builds wrap; callers that
/// can produce values past `u32::MAX` should use `try_into` and
/// surface a typed error instead.
#[inline]
#[must_use]
pub fn len_u32(n: usize) -> u32 {
    debug_assert!(
        u32::try_from(n).is_ok(),
        "len_u32: count {n} overflows u32; FFI field would silently truncate",
    );
    n as u32
}

/// Cast `core::mem::size_of::<T>()` to `u32` for an FFI struct's
/// `size` field.
///
/// `const` so the call disappears at codegen. The compile-time
/// `assert!` catches the (unrealistic) case where `T` is more than
/// 4GB at instantiation rather than panicking at run time.
///
/// # Panics
///
/// Panics at compile time (via `const` evaluation) if `T`'s size
/// exceeds `u32::MAX`. No real Rust type hits this, but the assert
/// is kept so callers can rely on the cast being lossless.
#[inline]
#[must_use]
pub const fn size_of_u32<T>() -> u32 {
    let n = core::mem::size_of::<T>();
    assert!(
        n <= u32::MAX as usize,
        "size_of_u32: T's size overflows u32",
    );
    n as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_7bit_endpoints() {
        assert_eq!(midi_7bit(0.0), 0);
        assert_eq!(midi_7bit(1.0), 127);
        assert_eq!(midi_7bit(0.5), 64); // round-half-to-even via .round()
    }

    #[test]
    fn midi_7bit_out_of_range_clamps() {
        assert_eq!(midi_7bit(-0.5), 0);
        assert_eq!(midi_7bit(2.0), 127);
        assert_eq!(midi_7bit(f32::INFINITY), 127);
        assert_eq!(midi_7bit(f32::NEG_INFINITY), 0);
    }

    #[test]
    fn midi_14bit_pb_endpoints() {
        assert_eq!(midi_14bit_pb(-1.0), 0);
        assert_eq!(midi_14bit_pb(0.0), 8192);
        assert_eq!(midi_14bit_pb(1.0), 16383);
    }

    #[test]
    fn midi_14bit_pb_clamps() {
        assert_eq!(midi_14bit_pb(-2.0), 0);
        assert_eq!(midi_14bit_pb(2.0), 16383);
    }

    #[test]
    fn len_u32_basic() {
        assert_eq!(len_u32(0), 0);
        assert_eq!(len_u32(127), 127);
        assert_eq!(len_u32(u32::MAX as usize), u32::MAX);
    }

    #[test]
    #[should_panic(expected = "overflows u32")]
    #[cfg(target_pointer_width = "64")]
    fn len_u32_overflow_panics_in_debug() {
        let _ = len_u32(u32::MAX as usize + 1);
    }

    #[test]
    fn size_of_u32_basic() {
        assert_eq!(size_of_u32::<u8>(), 1);
        assert_eq!(size_of_u32::<u32>(), 4);
        assert_eq!(size_of_u32::<u64>(), 8);
        #[repr(C)]
        struct AbiStruct {
            _a: u32,
            _b: u64,
        }
        assert_eq!(size_of_u32::<AbiStruct>(), 16);
    }
}
