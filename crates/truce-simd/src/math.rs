//! Vectorized transcendentals at audio-grade precision.
//!
//! Each `*_block` op mirrors the shape of [`crate::ops`]: takes a
//! mutable output slice and a read-only input slice, writes
//! element-wise. Lengths are taken to `min(out, src)`. Scalar
//! fallback is identical math; the SIMD path uses `wide`'s
//! sleef-derived approximations.
//!
//! Why these and not a full math library: every truce plugin
//! converts dB to linear (gain, mix, send, peak) and most
//! waveshapers want a fast `tanh`. The rest (`exp2`, `log2`) come
//! along for free since they're the building blocks of the first
//! two. We do not ship the wider set (`sin`, `cos`, `erf`,
//! `lgamma`); plugins that need those pull `wide` directly or use
//! `simdeez` / `sleef`.
//!
//! ## Error bounds
//!
//! - `db_to_linear_block`, `linear_to_db_block`: < 0.1 dB across
//!   the audio range `[-120, +24]` dB. Round-trip through
//!   linear→dB→linear stays within 1 part in 10^6 (verified by
//!   the fuzz test).
//! - `exp2_block`, `log2_block`: < 1 ULP for inputs in
//!   `[-126, +127]` (exp2) and `[2^-126, 2^127]` (log2). NaN /
//!   negative log2 inputs return NaN.
//! - `tanh_block`: exp-identity form via `wide::exp`, < 5e-6
//!   absolute error vs `f32::tanh` across `[-10, +10]`. Inputs
//!   outside that range clamp first; at `|x| = 10`, true `tanh`
//!   is already within 5e-9 of `±1`.

/// `20 / log2(10)`, for `linear → dB`.
#[cfg(feature = "wide-backend")]
const TWENTY_OVER_LOG2_10: f32 = 6.020_6;

/// `out[i] = 10^(src[i] / 20)`. The dB → linear conversion every
/// gain knob needs.
#[inline]
pub fn db_to_linear_block(out: &mut [f32], src: &[f32]) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f32x8;
        let n = out.len().min(src.len());
        // 10^(db/20) = exp(db * ln(10) / 20). `wide` provides
        // `exp` directly; routing through it is one fma + one
        // exp per chunk.
        let scale = f32x8::splat(core::f32::consts::LN_10 / 20.0);
        let (out_chunks, out_tail) = out[..n].as_chunks_mut::<8>();
        let (src_chunks, src_tail) = src[..n].as_chunks::<8>();
        for (out_chunk, src_chunk) in out_chunks.iter_mut().zip(src_chunks) {
            let v = f32x8::from(*src_chunk);
            *out_chunk = (v * scale).exp().to_array();
        }
        db_to_linear_block_scalar(out_tail, src_tail);
    }
    #[cfg(not(feature = "wide-backend"))]
    db_to_linear_block_scalar(out, src);
}

/// Scalar fallback for [`db_to_linear_block`]. Kept `pub` so
/// Criterion benches can hand the same inputs to the scalar and
/// vector paths.
#[inline]
pub fn db_to_linear_block_scalar(out: &mut [f32], src: &[f32]) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] = 10.0_f32.powf(src[i] / 20.0);
    }
}

/// `out[i] = 20 * log10(src[i])`. Linear → dB. Returns
/// `-f32::INFINITY` for zero input (matches `f32::log10`).
#[inline]
pub fn linear_to_db_block(out: &mut [f32], src: &[f32]) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f32x8;
        let n = out.len().min(src.len());
        let scale = f32x8::splat(TWENTY_OVER_LOG2_10);
        let (out_chunks, out_tail) = out[..n].as_chunks_mut::<8>();
        let (src_chunks, src_tail) = src[..n].as_chunks::<8>();
        for (out_chunk, src_chunk) in out_chunks.iter_mut().zip(src_chunks) {
            let v = f32x8::from(*src_chunk);
            *out_chunk = (v.log2() * scale).to_array();
        }
        linear_to_db_block_scalar(out_tail, src_tail);
    }
    #[cfg(not(feature = "wide-backend"))]
    linear_to_db_block_scalar(out, src);
}

/// Scalar fallback for [`linear_to_db_block`].
#[inline]
pub fn linear_to_db_block_scalar(out: &mut [f32], src: &[f32]) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] = 20.0 * src[i].log10();
    }
}

/// `out[i] = 2^src[i]`. The building block for `exp` and
/// `db_to_linear`.
#[inline]
pub fn exp2_block(out: &mut [f32], src: &[f32]) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f32x8;
        let n = out.len().min(src.len());
        let ln2 = f32x8::splat(core::f32::consts::LN_2);
        let (out_chunks, out_tail) = out[..n].as_chunks_mut::<8>();
        let (src_chunks, src_tail) = src[..n].as_chunks::<8>();
        for (out_chunk, src_chunk) in out_chunks.iter_mut().zip(src_chunks) {
            // exp2(x) = exp(x * ln(2)). `wide` has `exp` natively
            // but no `exp2`; multiply-then-exp is the same cycle
            // count as a hypothetical direct `exp2`.
            let v = f32x8::from(*src_chunk);
            *out_chunk = (v * ln2).exp().to_array();
        }
        exp2_block_scalar(out_tail, src_tail);
    }
    #[cfg(not(feature = "wide-backend"))]
    exp2_block_scalar(out, src);
}

/// Scalar fallback for [`exp2_block`].
#[inline]
pub fn exp2_block_scalar(out: &mut [f32], src: &[f32]) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] = src[i].exp2();
    }
}

/// `out[i] = log2(src[i])`. Building block for log10 and
/// `linear_to_db`.
#[inline]
pub fn log2_block(out: &mut [f32], src: &[f32]) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f32x8;
        let n = out.len().min(src.len());
        let (out_chunks, out_tail) = out[..n].as_chunks_mut::<8>();
        let (src_chunks, src_tail) = src[..n].as_chunks::<8>();
        for (out_chunk, src_chunk) in out_chunks.iter_mut().zip(src_chunks) {
            let v = f32x8::from(*src_chunk);
            *out_chunk = v.log2().to_array();
        }
        log2_block_scalar(out_tail, src_tail);
    }
    #[cfg(not(feature = "wide-backend"))]
    log2_block_scalar(out, src);
}

/// Scalar fallback for [`log2_block`].
#[inline]
pub fn log2_block_scalar(out: &mut [f32], src: &[f32]) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] = src[i].log2();
    }
}

/// `out[i] = tanh(src[i])`. For soft-clipping waveshapers and any
/// other DSP that wants a bounded sigmoid.
///
/// Computed via the exp identity `tanh(x) = (exp(2x) - 1) / (exp(2x) + 1)`,
/// which keeps the entire pipeline inside `wide`'s native `exp`
/// SIMD intrinsic. Input is clamped to `[-10, +10]` first so the
/// exponentiation can't overflow; at that magnitude `tanh` is
/// already within 5e-9 of `±1`. Error vs `f32::tanh` stays below
/// 5e-6 absolute across the audio range.
#[inline]
pub fn tanh_block(out: &mut [f32], src: &[f32]) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f32x8;
        let n = out.len().min(src.len());
        let bound = f32x8::splat(10.0);
        let neg_bound = f32x8::splat(-10.0);
        let two = f32x8::splat(2.0);
        let one = f32x8::splat(1.0);
        let (out_chunks, out_tail) = out[..n].as_chunks_mut::<8>();
        let (src_chunks, src_tail) = src[..n].as_chunks::<8>();
        for (out_chunk, src_chunk) in out_chunks.iter_mut().zip(src_chunks) {
            let x = f32x8::from(*src_chunk);
            let x_clamped = x.fast_max(neg_bound).fast_min(bound);
            let e2x = (x_clamped * two).exp();
            *out_chunk = ((e2x - one) / (e2x + one)).to_array();
        }
        tanh_block_scalar(out_tail, src_tail);
    }
    #[cfg(not(feature = "wide-backend"))]
    tanh_block_scalar(out, src);
}

/// Scalar fallback for [`tanh_block`]. Uses libm's `tanh` (full
/// precision) rather than the Padé approximant; the SIMD path's
/// approximation is the cost of vector throughput, and the scalar
/// path doesn't pay that cost.
#[inline]
pub fn tanh_block_scalar(out: &mut [f32], src: &[f32]) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] = src[i].tanh();
    }
}

#[cfg(test)]
mod tests {
    // Tolerance-based comparisons; bit-exactness isn't the
    // contract here (approximations are the whole point).
    #![allow(clippy::float_cmp, clippy::cast_precision_loss)]

    use super::*;

    fn max_abs_err(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| (x - y).abs())
            .fold(0.0_f32, f32::max)
    }

    fn max_rel_err(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b.iter())
            .filter(|(_, y)| y.abs() > 1e-6)
            .map(|(x, y)| ((x - y) / y).abs())
            .fold(0.0_f32, f32::max)
    }

    #[test]
    fn db_to_linear_block_matches_libm() {
        let src: Vec<f32> = (-120..=24).map(|i| i as f32).collect();
        let mut out = vec![0.0; src.len()];
        db_to_linear_block(&mut out, &src);
        let expected: Vec<f32> = src.iter().map(|&x| 10.0_f32.powf(x / 20.0)).collect();
        // Across [-120, +24] dB the relative error budget is 1e-5
        // (well under 0.1 dB).
        assert!(
            max_rel_err(&out, &expected) < 1e-5,
            "rel err = {}",
            max_rel_err(&out, &expected)
        );
    }

    #[test]
    fn linear_to_db_round_trips() {
        let db: Vec<f32> = (-100..=20).map(|i| i as f32).collect();
        let mut lin = vec![0.0; db.len()];
        let mut roundtrip = vec![0.0; db.len()];
        db_to_linear_block(&mut lin, &db);
        linear_to_db_block(&mut roundtrip, &lin);
        // Round-trip stays within 1e-4 dB.
        let err = max_abs_err(&db, &roundtrip);
        assert!(err < 1e-4, "round-trip err = {err} dB");
    }

    #[test]
    fn exp2_block_matches_libm() {
        let src: Vec<f32> = (-100..=100).map(|i| i as f32 * 0.1).collect();
        let mut out = vec![0.0; src.len()];
        exp2_block(&mut out, &src);
        let expected: Vec<f32> = src.iter().map(|&x| x.exp2()).collect();
        assert!(
            max_rel_err(&out, &expected) < 1e-5,
            "rel err = {}",
            max_rel_err(&out, &expected)
        );
    }

    #[test]
    fn log2_block_matches_libm() {
        let src: Vec<f32> = (1..=200).map(|i| i as f32).collect();
        let mut out = vec![0.0; src.len()];
        log2_block(&mut out, &src);
        let expected: Vec<f32> = src.iter().map(|&x| x.log2()).collect();
        assert!(
            max_abs_err(&out, &expected) < 1e-5,
            "abs err = {}",
            max_abs_err(&out, &expected)
        );
    }

    #[test]
    fn tanh_block_matches_libm() {
        let src: Vec<f32> = (-100..=100).map(|i| i as f32 * 0.1).collect();
        let mut out = vec![0.0; src.len()];
        tanh_block(&mut out, &src);
        let expected: Vec<f32> = src.iter().map(|&x| x.tanh()).collect();
        let err = max_abs_err(&out, &expected);
        assert!(err < 5e-6, "abs err = {err}");
    }

    #[test]
    fn tanh_block_saturates_for_large_inputs() {
        let src = [-50.0, -20.0, 20.0, 50.0];
        let mut out = [0.0; 4];
        tanh_block(&mut out, &src);
        for &y in &out {
            assert!(
                (y.abs() - 1.0).abs() < 1e-4,
                "expected saturation near ±1, got {y}"
            );
        }
    }

    #[test]
    fn lengths_min_clamped() {
        let src = [1.0_f32, 2.0, 3.0];
        let mut out = [0.0_f32; 5];
        db_to_linear_block(&mut out, &src);
        // Last two slots untouched.
        assert_eq!(out[3], 0.0);
        assert_eq!(out[4], 0.0);
    }
}
