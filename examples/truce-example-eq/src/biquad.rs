//! Local biquad shim for the EQ example.
//!
//! Wraps the upstream [`biquad`] crate (0.6+) with two pieces of
//! glue this plugin needs:
//!
//! - [`StereoSample`], a newtype over `wide::f64x2` that
//!   implements the minimal trait set the biquad crate's
//!   generic-T form requires (`Copy + Add + Sub + Zero` plus
//!   `Mul<StereoSample, Output=StereoSample> for f64`). With it,
//!   `DirectForm2Transposed::<f64, StereoSample>` runs both
//!   channels through one SIMD register pair, ~2x faster than
//!   two scalar biquads.
//! - Cookbook helpers ([`peaking`], [`low_shelf`], [`high_shelf`])
//!   that turn the EQ's `(freq, gain_db, q, sr)` knob shape into
//!   the biquad crate's [`Coefficients`].
//!
//! Lives in the example rather than a framework crate because no
//! other plugin in the tree currently wants biquads; the framework
//! stays neutral on which DSP library plugins reach for.

use std::ops::{Add, Mul, Sub};

use biquad::{Coefficients, Hertz, Type};
use num_traits::{ConstZero, Zero};
use wide::f64x2;

/// Two-channel sample packed into one `f64x2` SIMD register
/// (lane 0 = L, lane 1 = R). Implements just enough numeric
/// traits to plug into `biquad::Biquad<f64, StereoSample>`.
#[derive(Clone, Copy, Debug)]
pub struct StereoSample(pub f64x2);

impl StereoSample {
    #[inline]
    #[must_use]
    pub fn from_lr(l: f64, r: f64) -> Self {
        Self(f64x2::from([l, r]))
    }

    #[inline]
    #[must_use]
    pub fn to_lr(self) -> (f64, f64) {
        let a = self.0.to_array();
        (a[0], a[1])
    }
}

impl Add for StereoSample {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl Sub for StereoSample {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}

// f64 * StereoSample -> StereoSample. The biquad crate's
// `C: Mul<T, Output=T>` bound applies with C=f64, T=StereoSample;
// the orphan rule permits this impl because StereoSample (the
// generic argument) is local.
impl Mul<StereoSample> for f64 {
    type Output = StereoSample;
    #[inline]
    fn mul(self, rhs: StereoSample) -> StereoSample {
        StereoSample(f64x2::splat(self) * rhs.0)
    }
}

impl Zero for StereoSample {
    #[inline]
    fn zero() -> Self {
        <Self as ConstZero>::ZERO
    }
    #[inline]
    fn is_zero(&self) -> bool {
        self.0.to_array() == [0.0, 0.0]
    }
}

impl ConstZero for StereoSample {
    const ZERO: Self = Self(f64x2::ZERO);
}

/// Cookbook peaking-EQ coefficients. Mirrors the RBJ formulae the
/// `biquad` crate ships under `Type::PeakingEQ(gain_db)`; collapsed
/// to a single helper so the EQ's `process()` reads as
/// `bands[i].update_coefficients(peaking(freq, gain, q, sr))`.
///
/// # Panics
///
/// Panics if `q <= 0` or `sr <= 0`. `freq` is first clamped to just
/// below Nyquist (`sr / 2`), so a band whose center sits above Nyquist
/// at a low sample rate pins to the top of the usable range instead of
/// tripping the upstream `from_params` Nyquist check.
pub fn peaking(freq: f64, gain_db: f64, q: f64, sr: f64) -> Coefficients<f64> {
    let freq = freq.min(sr * 0.5 - 1.0);
    Coefficients::<f64>::from_params(
        Type::PeakingEQ(gain_db),
        Hertz::<f64>::from_hz(sr).expect("sample rate > 0"),
        Hertz::<f64>::from_hz(freq).expect("freq in (0, sr/2)"),
        q,
    )
    .expect("RBJ peaking coeffs")
}

pub fn low_shelf(freq: f64, gain_db: f64, q: f64, sr: f64) -> Coefficients<f64> {
    let freq = freq.min(sr * 0.5 - 1.0);
    Coefficients::<f64>::from_params(
        Type::LowShelf(gain_db),
        Hertz::<f64>::from_hz(sr).expect("sample rate > 0"),
        Hertz::<f64>::from_hz(freq).expect("freq in (0, sr/2)"),
        q,
    )
    .expect("RBJ low-shelf coeffs")
}

pub fn high_shelf(freq: f64, gain_db: f64, q: f64, sr: f64) -> Coefficients<f64> {
    let freq = freq.min(sr * 0.5 - 1.0);
    Coefficients::<f64>::from_params(
        Type::HighShelf(gain_db),
        Hertz::<f64>::from_hz(sr).expect("sample rate > 0"),
        Hertz::<f64>::from_hz(freq).expect("freq in (0, sr/2)"),
        q,
    )
    .expect("RBJ high-shelf coeffs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use biquad::{Biquad, DirectForm2Transposed};

    #[test]
    fn stereo_sample_arithmetic() {
        let a = StereoSample::from_lr(1.0, 2.0);
        let b = StereoSample::from_lr(0.5, 0.25);
        let sum = a + b;
        assert_eq!(sum.to_lr(), (1.5, 2.25));
        let diff = a - b;
        assert_eq!(diff.to_lr(), (0.5, 1.75));
        let scaled = 2.0_f64 * a;
        assert_eq!(scaled.to_lr(), (2.0, 4.0));
        let zero = StereoSample::zero();
        assert_eq!(zero.to_lr(), (0.0, 0.0));
        assert!(zero.is_zero());
        assert!(!a.is_zero());
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn stereo_biquad_matches_two_scalar_biquads() {
        // Same coefficients fed to a scalar pair and a packed
        // stereo instance; outputs should agree to bit precision
        // (NEON / AVX2 f64x2 ops are IEEE-754 across-lane).
        let coeffs = peaking(1000.0, 6.0, 0.7, 48_000.0);
        let mut scalar_l = DirectForm2Transposed::<f64>::new(coeffs);
        let mut scalar_r = DirectForm2Transposed::<f64>::new(coeffs);
        let mut stereo = DirectForm2Transposed::<f64, StereoSample>::new(coeffs);

        for i in 0..64 {
            let xl = f64::from(i) * 0.1 - 1.0;
            let xr = (f64::from(i) * 0.13 + 0.7).sin();
            let yl = scalar_l.run(xl);
            let yr = scalar_r.run(xr);
            let (sl, sr) = stereo.run(StereoSample::from_lr(xl, xr)).to_lr();
            assert_eq!(yl, sl, "L mismatch at i={i}");
            assert_eq!(yr, sr, "R mismatch at i={i}");
        }
    }
}
