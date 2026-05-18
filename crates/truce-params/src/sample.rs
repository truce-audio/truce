//! `Float` and `Sample` - the precision-routing traits that let
//! plugin code stay in one float type without per-call-site casts.
//!
//! Plugin authors don't usually name these traits directly. They
//! pick a precision via the prelude (`truce::prelude` /
//! `truce::prelude32` for `f32`, `truce::prelude64` for `f64`); the
//! prelude's `type Sample` alias resolves the bound at the call
//! sites. The traits surface only when DSP code wants to convert
//! between precisions per value:
//!
//! ```
//! use truce_params::sample::Float;
//! let v_f32: f32 = 0.5;
//! let v_f64: f64 = v_f32.to_f64();   // widen
//! let back:  f32 = f32::from_f64(v_f64); // narrow
//! ```
//!
//! Both traits are sealed at `f32` and `f64`. Downstream code can't
//! add new impls; numeric types beyond these two have never been
//! worth the complexity for audio.
//!
//! ## Two traits, why
//!
//! - [`Float`] is the **broad math bound**. Use it for utilities like
//!   `db_to_linear`, `midi_note_to_freq` - values that happen to be
//!   `f32` or `f64` but aren't audio samples. The bound carries the
//!   precision-routing methods (`from_f32`/`from_f64`/`to_f32`/`to_f64`)
//!   plus a handful of math primitives (`exp`, `log10`, `powf`).
//!   `Float::from_f64`'s NaN debug-assert is the same as `Sample`'s,
//!   because anywhere a NaN narrowing slips through is a bug
//!   regardless of whether the value is a sample or a gain coefficient.
//! - [`Sample`] is `Float` plus the marker bounds that buffer code
//!   needs (`Default + Send + Sync + 'static`) so the wrapper can
//!   default-construct scratch buffers and pass them across threads.
//!   This is the bound that goes on `AudioBuffer<S>`, `Plugin::Sample`,
//!   and the `FloatParamRead<S>` extension trait.

use std::ops::{Add, Div, Mul, Sub};

/// Broad numeric trait for code that operates on `f32` or `f64` but
/// isn't necessarily handling audio samples. Use this for math
/// utilities (gain conversions, frequency math, filter coefficients).
/// For audio-sample-typed surfaces (`AudioBuffer<S>`, smoother
/// reads), use [`Sample`] instead, which extends `Float` with the
/// marker bounds buffer code needs.
pub trait Float:
    sealed::Sealed
    + Copy
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
{
    /// Widen an `f32` to this precision. Lossless for `f64`; identity
    /// for `f32`.
    #[must_use]
    fn from_f32(v: f32) -> Self;

    /// Narrow an `f64` to this precision. Identity for `f64`. For
    /// `f32`, debug-asserts non-NaN - DSP code that produces a NaN
    /// here is always a bug, and silent NaN propagation through the
    /// audio path causes host-inconsistent behaviour. Release builds
    /// preserve NaN via the bare `as` cast so the upstream bug stays
    /// visible.
    #[must_use]
    fn from_f64(v: f64) -> Self;

    /// Narrow to `f32`. Identity for `f32`; for `f64`, same NaN
    /// debug-assert as [`Self::from_f64`].
    #[must_use]
    fn to_f32(self) -> f32;

    /// Widen to `f64`. Identity for `f64`; lossless for `f32`.
    #[must_use]
    fn to_f64(self) -> f64;

    /// Natural exponential. Forwards to the type's intrinsic.
    #[must_use]
    fn exp(self) -> Self;

    /// Base-10 logarithm. Forwards to the type's intrinsic.
    #[must_use]
    fn log10(self) -> Self;

    /// `self.powf(exp)`. Forwards to the type's intrinsic.
    #[must_use]
    fn powf(self, exp: Self) -> Self;
}

/// Audio-sample subtype of [`Float`]. Adds the
/// `Default + Send + Sync + 'static` marker bounds that buffer code,
/// scratch allocators, and the param-read extension trait need.
///
/// Bound at `f32` and `f64`. Plugin authors usually don't name this
/// directly; the prelude resolves the bound for them.
pub trait Sample: Float + Default + Send + Sync + 'static {}

impl Sample for f32 {}
impl Sample for f64 {}

mod sealed {
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
}

impl Float for f32 {
    #[inline]
    fn from_f32(v: f32) -> Self {
        v
    }

    // Plugins narrowing `f64 → f32` (param values, filter
    // coefficients, host-side display) get the NaN guard here.
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    fn from_f64(v: f64) -> Self {
        debug_assert!(
            !v.is_nan(),
            "Float::from_f64: NaN narrowed to f32 - DSP loop or coefficient \
             computation produced an undefined value?",
        );
        v as f32
    }

    #[inline]
    fn to_f32(self) -> f32 {
        self
    }

    #[inline]
    fn to_f64(self) -> f64 {
        f64::from(self)
    }

    #[inline]
    fn exp(self) -> Self {
        f32::exp(self)
    }
    #[inline]
    fn log10(self) -> Self {
        f32::log10(self)
    }
    #[inline]
    fn powf(self, exp: Self) -> Self {
        f32::powf(self, exp)
    }
}

impl Float for f64 {
    #[inline]
    fn from_f32(v: f32) -> Self {
        f64::from(v)
    }

    #[inline]
    fn from_f64(v: f64) -> Self {
        v
    }

    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    fn to_f32(self) -> f32 {
        debug_assert!(
            !self.is_nan(),
            "Float::to_f32: NaN narrowed to f32 - DSP loop or coefficient \
             computation produced an undefined value?",
        );
        self as f32
    }

    #[inline]
    fn to_f64(self) -> f64 {
        self
    }

    #[inline]
    fn exp(self) -> Self {
        f64::exp(self)
    }
    #[inline]
    fn log10(self) -> Self {
        f64::log10(self)
    }
    #[inline]
    fn powf(self, exp: Self) -> Self {
        f64::powf(self, exp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::float_cmp)]
    fn widen_narrow_round_trip_f32() {
        // Bit-exact round trip: f32 → f64 → f32 must be the identity
        // (f32 fits losslessly in f64), so a strict equality compare
        // is correct here, not a tolerance epsilon.
        let v: f32 = 0.123_456_7;
        assert_eq!(f32::from_f64(v.to_f64()), v);
    }

    #[test]
    fn widen_narrow_round_trip_f64_lossy() {
        // Narrowing a precise f64 to f32 and back loses bits but
        // stays bounded in audio range.
        let v: f64 = 0.123_456_789_012_345;
        let round_tripped = f32::from_f64(v).to_f64();
        assert!((round_tripped - v).abs() < 1e-7);
    }

    #[test]
    #[should_panic(expected = "NaN narrowed to f32")]
    #[cfg(debug_assertions)]
    fn nan_narrow_debug_panics() {
        let _ = f32::from_f64(f64::NAN);
    }
}
