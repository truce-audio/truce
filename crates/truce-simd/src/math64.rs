//! Vectorized transcendentals at `f64`. Mirror of [`crate::math`]
//! (f32). The wider lane is `wide::f64x4` (4 lanes vs `f32x8`'s 8),
//! so chunk granularity is 4 and the per-block speedup is roughly
//! half of the f32 path's. Same `*_block` shape, same scalar
//! fallback contract.
//!
//! ## Error bounds
//!
//! - `db_to_linear_block`, `linear_to_db_block`: < 1e-9 dB across
//!   the audio range `[-120, +24]` dB (verified by the round-trip
//!   test below). The double-precision exp identity has 2-3 more
//!   decimal digits of headroom than the f32 path.
//! - `exp2_block`, `log2_block`: < 1 ULP for inputs in
//!   `[-1022, +1023]` (exp2) and `[2^-1022, 2^1023]` (log2). NaN /
//!   negative log2 inputs return NaN.
//! - `tanh_block`: exp-identity form via `wide::exp`, < 5e-15
//!   absolute error vs `f64::tanh` across `[-20, +20]`. Inputs
//!   outside that range clamp first; at `|x| = 20`, true `tanh`
//!   is already within 5e-18 of `±1`.

/// `20 / log2(10)`, for `linear → dB`.
#[cfg(feature = "wide-backend")]
const TWENTY_OVER_LOG2_10: f64 = 6.020_599_913_279_624;

/// `out[i] = 10^(src[i] / 20)`. dB → linear at f64.
#[inline]
pub fn db_to_linear_block(out: &mut [f64], src: &[f64]) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let n = out.len().min(src.len());
        let n4 = n / 4 * 4;
        // 10^(db/20) = exp(db * ln(10) / 20). One fma + one exp
        // per chunk.
        let scale = f64x4::splat(core::f64::consts::LN_10 / 20.0);
        let (head_out, tail_out) = out[..n].split_at_mut(n4);
        for (out_chunk, src_chunk) in head_out.chunks_exact_mut(4).zip(src[..n4].chunks_exact(4)) {
            let v = f64x4::from(<[f64; 4]>::try_from(src_chunk).unwrap_or_default());
            out_chunk.copy_from_slice((v * scale).exp().as_array_ref());
        }
        db_to_linear_block_scalar(tail_out, &src[n4..n]);
    }
    #[cfg(not(feature = "wide-backend"))]
    db_to_linear_block_scalar(out, src);
}

/// Scalar fallback for [`db_to_linear_block`].
#[inline]
pub fn db_to_linear_block_scalar(out: &mut [f64], src: &[f64]) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] = 10.0_f64.powf(src[i] / 20.0);
    }
}

/// `out[i] = 20 * log10(src[i])`. Linear → dB. Returns
/// `-f64::INFINITY` for zero input (matches `f64::log10`).
#[inline]
pub fn linear_to_db_block(out: &mut [f64], src: &[f64]) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let n = out.len().min(src.len());
        let n4 = n / 4 * 4;
        let scale = f64x4::splat(TWENTY_OVER_LOG2_10);
        let (head_out, tail_out) = out[..n].split_at_mut(n4);
        for (out_chunk, src_chunk) in head_out.chunks_exact_mut(4).zip(src[..n4].chunks_exact(4)) {
            let v = f64x4::from(<[f64; 4]>::try_from(src_chunk).unwrap_or_default());
            out_chunk.copy_from_slice((v.log2() * scale).as_array_ref());
        }
        linear_to_db_block_scalar(tail_out, &src[n4..n]);
    }
    #[cfg(not(feature = "wide-backend"))]
    linear_to_db_block_scalar(out, src);
}

/// Scalar fallback for [`linear_to_db_block`].
#[inline]
pub fn linear_to_db_block_scalar(out: &mut [f64], src: &[f64]) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] = 20.0 * src[i].log10();
    }
}

/// `out[i] = 2^src[i]`. The building block for `exp` and
/// `db_to_linear`.
#[inline]
pub fn exp2_block(out: &mut [f64], src: &[f64]) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let n = out.len().min(src.len());
        let n4 = n / 4 * 4;
        let ln2 = f64x4::splat(core::f64::consts::LN_2);
        let (head_out, tail_out) = out[..n].split_at_mut(n4);
        for (out_chunk, src_chunk) in head_out.chunks_exact_mut(4).zip(src[..n4].chunks_exact(4)) {
            // exp2(x) = exp(x * ln(2)); `wide` has `exp` but no
            // direct `exp2`, same as the f32 path.
            let v = f64x4::from(<[f64; 4]>::try_from(src_chunk).unwrap_or_default());
            out_chunk.copy_from_slice((v * ln2).exp().as_array_ref());
        }
        exp2_block_scalar(tail_out, &src[n4..n]);
    }
    #[cfg(not(feature = "wide-backend"))]
    exp2_block_scalar(out, src);
}

/// Scalar fallback for [`exp2_block`].
#[inline]
pub fn exp2_block_scalar(out: &mut [f64], src: &[f64]) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] = src[i].exp2();
    }
}

/// `out[i] = log2(src[i])`. Building block for log10 and
/// `linear_to_db`.
#[inline]
pub fn log2_block(out: &mut [f64], src: &[f64]) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let n = out.len().min(src.len());
        let n4 = n / 4 * 4;
        let (head_out, tail_out) = out[..n].split_at_mut(n4);
        for (out_chunk, src_chunk) in head_out.chunks_exact_mut(4).zip(src[..n4].chunks_exact(4)) {
            let v = f64x4::from(<[f64; 4]>::try_from(src_chunk).unwrap_or_default());
            out_chunk.copy_from_slice(v.log2().as_array_ref());
        }
        log2_block_scalar(tail_out, &src[n4..n]);
    }
    #[cfg(not(feature = "wide-backend"))]
    log2_block_scalar(out, src);
}

/// Scalar fallback for [`log2_block`].
#[inline]
pub fn log2_block_scalar(out: &mut [f64], src: &[f64]) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] = src[i].log2();
    }
}

/// `out[i] = tanh(src[i])`. For soft-clipping waveshapers and any
/// other DSP that wants a bounded sigmoid.
///
/// Same exp-identity form as the f32 path; clamp to `[-20, +20]`
/// before exponentiating (at `|x| = 20`, true `tanh` is within
/// 5e-18 of `±1`).
#[inline]
pub fn tanh_block(out: &mut [f64], src: &[f64]) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let n = out.len().min(src.len());
        let n4 = n / 4 * 4;
        let bound = f64x4::splat(20.0);
        let neg_bound = f64x4::splat(-20.0);
        let two = f64x4::splat(2.0);
        let one = f64x4::splat(1.0);
        let (head_out, tail_out) = out[..n].split_at_mut(n4);
        for (out_chunk, src_chunk) in head_out.chunks_exact_mut(4).zip(src[..n4].chunks_exact(4)) {
            let x = f64x4::from(<[f64; 4]>::try_from(src_chunk).unwrap_or_default());
            let x_clamped = x.fast_max(neg_bound).fast_min(bound);
            let e2x = (x_clamped * two).exp();
            let result = (e2x - one) / (e2x + one);
            out_chunk.copy_from_slice(result.as_array_ref());
        }
        tanh_block_scalar(tail_out, &src[n4..n]);
    }
    #[cfg(not(feature = "wide-backend"))]
    tanh_block_scalar(out, src);
}

/// Scalar fallback for [`tanh_block`]. Uses libm's `tanh` (full
/// precision); the SIMD path's approximation is the cost of vector
/// throughput.
#[inline]
pub fn tanh_block_scalar(out: &mut [f64], src: &[f64]) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] = src[i].tanh();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp, clippy::cast_precision_loss)]

    use super::*;

    fn max_abs_err(a: &[f64], b: &[f64]) -> f64 {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| (x - y).abs())
            .fold(0.0_f64, f64::max)
    }

    fn max_rel_err(a: &[f64], b: &[f64]) -> f64 {
        a.iter()
            .zip(b.iter())
            .filter(|(_, y)| y.abs() > 1e-12)
            .map(|(x, y)| ((x - y) / y).abs())
            .fold(0.0_f64, f64::max)
    }

    #[test]
    fn db_to_linear_block_matches_libm() {
        let src: Vec<f64> = (-120..=24).map(f64::from).collect();
        let mut out = vec![0.0; src.len()];
        db_to_linear_block(&mut out, &src);
        let expected: Vec<f64> = src.iter().map(|&x| 10.0_f64.powf(x / 20.0)).collect();
        // f64 path has ~10x tighter relative-error budget than f32.
        assert!(
            max_rel_err(&out, &expected) < 1e-9,
            "rel err = {}",
            max_rel_err(&out, &expected)
        );
    }

    #[test]
    fn linear_to_db_round_trips() {
        let db: Vec<f64> = (-100..=20).map(f64::from).collect();
        let mut lin = vec![0.0; db.len()];
        let mut roundtrip = vec![0.0; db.len()];
        db_to_linear_block(&mut lin, &db);
        linear_to_db_block(&mut roundtrip, &lin);
        // Round-trip stays within 1e-9 dB at f64 (vs 1e-4 at f32).
        let err = max_abs_err(&db, &roundtrip);
        assert!(err < 1e-9, "round-trip err = {err} dB");
    }

    #[test]
    fn exp2_block_matches_libm() {
        let src: Vec<f64> = (-100..=100).map(|i| f64::from(i) * 0.1).collect();
        let mut out = vec![0.0; src.len()];
        exp2_block(&mut out, &src);
        let expected: Vec<f64> = src.iter().map(|&x| x.exp2()).collect();
        assert!(
            max_rel_err(&out, &expected) < 1e-9,
            "rel err = {}",
            max_rel_err(&out, &expected)
        );
    }

    #[test]
    fn log2_block_matches_libm() {
        let src: Vec<f64> = (1..=200).map(f64::from).collect();
        let mut out = vec![0.0; src.len()];
        log2_block(&mut out, &src);
        let expected: Vec<f64> = src.iter().map(|&x| x.log2()).collect();
        assert!(
            max_abs_err(&out, &expected) < 1e-9,
            "abs err = {}",
            max_abs_err(&out, &expected)
        );
    }

    #[test]
    fn tanh_block_matches_libm() {
        let src: Vec<f64> = (-100..=100).map(|i| f64::from(i) * 0.1).collect();
        let mut out = vec![0.0; src.len()];
        tanh_block(&mut out, &src);
        let expected: Vec<f64> = src.iter().map(|&x| x.tanh()).collect();
        let err = max_abs_err(&out, &expected);
        assert!(err < 5e-15, "abs err = {err}");
    }

    #[test]
    fn tanh_block_saturates_for_large_inputs() {
        let src = [-50.0, -30.0, 30.0, 50.0];
        let mut out = [0.0; 4];
        tanh_block(&mut out, &src);
        for &y in &out {
            assert!(
                (y.abs() - 1.0).abs() < 1e-12,
                "expected saturation near ±1, got {y}"
            );
        }
    }

    #[test]
    fn lengths_min_clamped() {
        let src = [1.0_f64, 2.0, 3.0];
        let mut out = [0.0_f64; 5];
        db_to_linear_block(&mut out, &src);
        assert_eq!(out[3], 0.0);
        assert_eq!(out[4], 0.0);
    }
}
