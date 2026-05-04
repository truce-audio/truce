//! Numeric-cast helpers for the audio-plugin → host FFI boundary.
//!
//! Audio-plugin code routinely casts at four points where Rust's
//! type system can't help:
//!
//! - **MIDI 7-bit normalize:** velocity / CC / pressure stored as
//!   `f32 ∈ [0.0, 1.0]` re-encodes to `u8 ∈ [0, 127]`.
//! - **Pitch bend (14-bit):** `f32 ∈ [-1.0, 1.0]` re-encodes to
//!   `u14` packed into the low 7 bits of two MIDI bytes.
//! - **FFI struct sizes / element counts:** `usize` (Rust) vs `u32`
//!   (every C ABI we ship to).
//! - **Discrete-index ↔ normalized:** GUI selector / dropdown
//!   widgets bridge an integer "which option" to a normalized
//!   `f64 ∈ [0.0, 1.0]` parameter value. The full audit lives in
//!   `internal/float-misuse-audit.md`.
//!
//! Each helper is `#[inline]`, debug-asserts the input range so a
//! NaN-bearing or overflowing caller fails loud in tests, and is
//! the *only* place in the workspace that's allowed to reach for
//! `as` on its specific shape. The lints
//! `cast_possible_truncation`, `cast_sign_loss`, and
//! `cast_precision_loss` are allowed at the module level so the
//! helpers can do their job without per-site annotations.
//!
//! Adding new helpers: target shapes that show up at ≥ 5 sites
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

/// Narrow a host-facing `f64` parameter / scale value to the `f32`
/// the DSP loop and renderer expect.
///
/// The host parameter API surface is `f64` (CLAP, VST3, AU all
/// deliver `double`); per-sample DSP and tiny-skia rendering work in
/// `f32`. This helper hides that bridge so call sites don't repeat
/// `as f32` with a `#[allow]`.
///
/// Truncation is invisible in practice — parameter values stay in
/// `[-1e10, 1e10]` and `f32` carries 7 decimals of precision. NaN
/// debug-asserts; in release it round-trips through `as f32` (which
/// preserves NaN). Inf passes through unchanged.
#[inline]
#[must_use]
pub fn param_f32(v: f64) -> f32 {
    debug_assert!(
        !v.is_nan(),
        "param_f32: NaN input — caller's f64 parameter value is uninitialized?",
    );
    v as f32
}

/// Convert a host-supplied sample-position `f64` to the `i64` truce's
/// `TransportInfo::position_samples` carries.
///
/// Hosts deliver play-cursor position as `double samplePosition`
/// (CLAP / VST3 / AU all do). Truce stores it as `i64`, large enough
/// for ~3000 years at 48 kHz. Non-finite inputs saturate at
/// `i64::MIN` / `i64::MAX` rather than producing the unspecified
/// integer the bare cast historically did.
#[inline]
#[must_use]
pub fn sample_pos_i64(v: f64) -> i64 {
    if v.is_nan() {
        debug_assert!(false, "sample_pos_i64: NaN host sample position");
        return 0;
    }
    if v >= i64::MAX as f64 {
        return i64::MAX;
    }
    if v <= i64::MIN as f64 {
        return i64::MIN;
    }
    v as i64
}

/// Convert a sample-count expressed as `f64` (e.g. `seconds *
/// sample_rate`) to `usize`, saturating on overflow / negative /
/// non-finite inputs.
///
/// Mirrors the "is_finite && >= 0" guard pattern that `truce-driver`
/// open-coded across its offline-render path. NaN and negative
/// inputs collapse to `0`; positive infinity and any value past
/// `usize::MAX` clamp to `usize::MAX`.
#[inline]
#[must_use]
pub fn sample_count_usize(v: f64) -> usize {
    if v.is_nan() || v <= 0.0 {
        return 0;
    }
    if v >= usize::MAX as f64 {
        return usize::MAX;
    }
    v as usize
}

/// Map a discrete index in `[0, count - 1]` to a normalized value
/// in `[0.0, 1.0]`. Returns `0.0` when `count <= 1` — there's only
/// one valid index, so any input collapses to the bottom of the
/// range.
///
/// `idx` is clamped to `count - 1` before scaling so an off-by-one
/// caller can't produce a normalized value above `1.0`. The output
/// is `f64` because the host-facing param surface
/// (`Params::set_normalized`) is `f64`; widget code that needs
/// `f32` should cast at the call site.
///
/// Inverse of [`discrete_index`]. Together they are the canonical
/// place selector / dropdown widgets bridge integer option indices
/// to normalized parameter values.
#[inline]
#[must_use]
pub fn discrete_norm(idx: usize, count: usize) -> f64 {
    if count <= 1 {
        return 0.0;
    }
    let max_idx = count - 1;
    idx.min(max_idx) as f64 / max_idx as f64
}

/// Map a normalized value in `[0.0, 1.0]` to a discrete index in
/// `[0, count - 1]`. Returns `0` when `count <= 1` — the index is
/// pinned to the only valid slot.
///
/// `norm` is clamped to `[0.0, 1.0]` before scaling so an
/// out-of-range host (e.g. a VST3 host that sends `1.0001`) can't
/// produce an out-of-range index. Rounding is half-to-even via
/// `f64::round`, the same rule applied across the param taper code
/// in `truce_params::range`.
///
/// Inverse of [`discrete_norm`]; round-trips for every `idx ∈
/// [0, count - 1]` whenever `count - 1` is exactly representable
/// in `f64` (i.e. always, for any sane widget).
#[inline]
#[must_use]
pub fn discrete_index(norm: f64, count: usize) -> usize {
    if count <= 1 {
        return 0;
    }
    let n = norm.clamp(0.0, 1.0);
    let max_idx = count - 1;
    (n * max_idx as f64).round() as usize
}

#[cfg(test)]
mod tests {
    // Tests compare exactly-representable float results (0.0, 1.0,
    // 1/3, etc.) where bit-equality is the contract.
    #![allow(clippy::float_cmp)]

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

    #[test]
    fn discrete_norm_endpoints() {
        // 4-option selector: indices 0..=3 map to 0, 1/3, 2/3, 1.
        assert_eq!(discrete_norm(0, 4), 0.0);
        assert!((discrete_norm(1, 4) - 1.0 / 3.0).abs() < 1e-12);
        assert!((discrete_norm(2, 4) - 2.0 / 3.0).abs() < 1e-12);
        assert_eq!(discrete_norm(3, 4), 1.0);
    }

    #[test]
    fn discrete_norm_degenerate_collapses_to_zero() {
        assert_eq!(discrete_norm(0, 0), 0.0);
        assert_eq!(discrete_norm(0, 1), 0.0);
        assert_eq!(discrete_norm(99, 1), 0.0);
    }

    #[test]
    fn discrete_norm_clamps_oob_idx() {
        // idx past count-1 must not produce a normalized > 1.0
        assert_eq!(discrete_norm(99, 4), 1.0);
    }

    #[test]
    fn discrete_index_endpoints() {
        assert_eq!(discrete_index(0.0, 4), 0);
        assert_eq!(discrete_index(1.0, 4), 3);
        // Quarter of the way → first non-zero step.
        assert_eq!(discrete_index(1.0 / 3.0, 4), 1);
        assert_eq!(discrete_index(2.0 / 3.0, 4), 2);
    }

    #[test]
    fn discrete_index_degenerate_returns_zero() {
        assert_eq!(discrete_index(0.5, 0), 0);
        assert_eq!(discrete_index(0.5, 1), 0);
        assert_eq!(discrete_index(1.0, 1), 0);
    }

    #[test]
    fn discrete_index_clamps_oob_norm() {
        assert_eq!(discrete_index(-0.5, 4), 0);
        assert_eq!(discrete_index(2.0, 4), 3);
    }

    #[test]
    fn param_f32_basic() {
        assert_eq!(param_f32(0.0), 0.0_f32);
        assert_eq!(param_f32(1.0), 1.0_f32);
        assert_eq!(param_f32(-1.0), -1.0_f32);
        assert_eq!(param_f32(0.5), 0.5_f32);
        assert!(param_f32(f64::INFINITY).is_infinite());
        assert!(param_f32(f64::NEG_INFINITY).is_infinite());
    }

    #[test]
    fn sample_pos_i64_basic() {
        assert_eq!(sample_pos_i64(0.0), 0);
        assert_eq!(sample_pos_i64(48_000.0), 48_000);
        assert_eq!(sample_pos_i64(-1.0), -1);
    }

    #[test]
    fn sample_pos_i64_saturates_on_non_finite() {
        assert_eq!(sample_pos_i64(f64::INFINITY), i64::MAX);
        assert_eq!(sample_pos_i64(f64::NEG_INFINITY), i64::MIN);
    }

    #[test]
    fn sample_count_usize_basic() {
        assert_eq!(sample_count_usize(0.0), 0);
        assert_eq!(sample_count_usize(48_000.0), 48_000);
    }

    #[test]
    fn sample_count_usize_collapses_invalid() {
        assert_eq!(sample_count_usize(-1.0), 0);
        assert_eq!(sample_count_usize(f64::NAN), 0);
        assert_eq!(sample_count_usize(f64::INFINITY), usize::MAX);
        assert_eq!(sample_count_usize(f64::NEG_INFINITY), 0);
    }

    #[test]
    fn discrete_norm_index_round_trip() {
        for count in [2usize, 3, 4, 7, 16, 128] {
            for idx in 0..count {
                let norm = discrete_norm(idx, count);
                let back = discrete_index(norm, count);
                assert_eq!(back, idx, "count={count}, idx={idx}, norm={norm}");
            }
        }
    }
}
