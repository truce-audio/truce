//! Block-rate audio ops at `f32`. See [`crate::ops64`] for the
//! `f64` mirror.
//!
//! Two implementations per op when `wide-backend` is enabled:
//! a scalar fallback (the `*_scalar` variant, also the body when
//! the feature is off) and a vector path that processes `f32x8`
//! chunks. The scalar variants are kept `pub` so the Criterion
//! benches can compare pre-/post-vectorization on exactly the
//! same operation.

/// `buf[i] *= gain` for every element. Scalar.
#[inline]
pub fn gain_block_scalar(buf: &mut [f32], gain: f32) {
    for s in buf {
        *s *= gain;
    }
}

/// `buf[i] *= gain` for every element. Vectorized when
/// `wide-backend` is on, otherwise identical to the scalar path.
#[inline]
pub fn gain_block(buf: &mut [f32], gain: f32) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f32x8;
        let g = f32x8::splat(gain);
        let (chunks, tail) = buf.as_chunks_mut::<8>();
        for chunk in chunks {
            let v = f32x8::from(*chunk);
            *chunk = (v * g).to_array();
        }
        gain_block_scalar(tail, gain);
    }
    #[cfg(not(feature = "wide-backend"))]
    gain_block_scalar(buf, gain);
}

/// `out[i] = src[i] * scale` (length: `min(out, src)`). Scalar.
/// The non-aliased counterpart to [`gain_block`] - fills the
/// "copy then gain" hole in the API so a single line covers what
/// would otherwise need two calls and a discarded intermediate.
#[inline]
pub fn scale_block_scalar(out: &mut [f32], src: &[f32], scale: f32) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] = src[i] * scale;
    }
}

/// `out[i] = src[i] * scale`. Vectorized when `wide-backend` is on.
#[inline]
pub fn scale_block(out: &mut [f32], src: &[f32], scale: f32) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f32x8;
        let n = out.len().min(src.len());
        let g = f32x8::splat(scale);
        let (out_chunks, out_tail) = out[..n].as_chunks_mut::<8>();
        let (src_chunks, src_tail) = src[..n].as_chunks::<8>();
        for (out_chunk, src_chunk) in out_chunks.iter_mut().zip(src_chunks) {
            let v = f32x8::from(*src_chunk);
            *out_chunk = (v * g).to_array();
        }
        scale_block_scalar(out_tail, src_tail, scale);
    }
    #[cfg(not(feature = "wide-backend"))]
    scale_block_scalar(out, src, scale);
}

/// `out[i] = a[i] * b[i]` (length: `min(out, a, b)`). Scalar.
#[inline]
pub fn mul_block_scalar(out: &mut [f32], a: &[f32], b: &[f32]) {
    let n = out.len().min(a.len()).min(b.len());
    for i in 0..n {
        out[i] = a[i] * b[i];
    }
}

/// `out[i] = a[i] * b[i]`. Vectorized when `wide-backend` is on.
#[inline]
pub fn mul_block(out: &mut [f32], a: &[f32], b: &[f32]) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f32x8;
        let n = out.len().min(a.len()).min(b.len());
        let (out_chunks, out_tail) = out[..n].as_chunks_mut::<8>();
        let (a_chunks, a_tail) = a[..n].as_chunks::<8>();
        let (b_chunks, b_tail) = b[..n].as_chunks::<8>();
        for ((out_chunk, a_chunk), b_chunk) in out_chunks.iter_mut().zip(a_chunks).zip(b_chunks) {
            let av = f32x8::from(*a_chunk);
            let bv = f32x8::from(*b_chunk);
            *out_chunk = (av * bv).to_array();
        }
        mul_block_scalar(out_tail, a_tail, b_tail);
    }
    #[cfg(not(feature = "wide-backend"))]
    mul_block_scalar(out, a, b);
}

/// Multiply-accumulate: `out[i] += src[i] * scale`. Scalar.
#[inline]
pub fn mac_block_scalar(out: &mut [f32], src: &[f32], scale: f32) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] += src[i] * scale;
    }
}

/// `out[i] += src[i] * scale`. Vectorized when `wide-backend` is on.
#[inline]
pub fn mac_block(out: &mut [f32], src: &[f32], scale: f32) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f32x8;
        let n = out.len().min(src.len());
        let s = f32x8::splat(scale);
        let (out_chunks, out_tail) = out[..n].as_chunks_mut::<8>();
        let (src_chunks, src_tail) = src[..n].as_chunks::<8>();
        for (out_chunk, src_chunk) in out_chunks.iter_mut().zip(src_chunks) {
            let ov = f32x8::from(*out_chunk);
            let sv = f32x8::from(*src_chunk);
            *out_chunk = (ov + sv * s).to_array();
        }
        mac_block_scalar(out_tail, src_tail, scale);
    }
    #[cfg(not(feature = "wide-backend"))]
    mac_block_scalar(out, src, scale);
}

/// `out[i] = a[i] * gain_a + b[i] * gain_b`. Scalar.
#[inline]
pub fn mix_block_scalar(out: &mut [f32], a: &[f32], gain_a: f32, b: &[f32], gain_b: f32) {
    let n = out.len().min(a.len()).min(b.len());
    for i in 0..n {
        out[i] = a[i] * gain_a + b[i] * gain_b;
    }
}

/// `out[i] = a[i] * gain_a + b[i] * gain_b`. Vectorized when
/// `wide-backend` is on.
#[inline]
pub fn mix_block(out: &mut [f32], a: &[f32], gain_a: f32, b: &[f32], gain_b: f32) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f32x8;
        let n = out.len().min(a.len()).min(b.len());
        let ga = f32x8::splat(gain_a);
        let gb = f32x8::splat(gain_b);
        let (out_chunks, out_tail) = out[..n].as_chunks_mut::<8>();
        let (a_chunks, a_tail) = a[..n].as_chunks::<8>();
        let (b_chunks, b_tail) = b[..n].as_chunks::<8>();
        for ((out_chunk, a_chunk), b_chunk) in out_chunks.iter_mut().zip(a_chunks).zip(b_chunks) {
            let av = f32x8::from(*a_chunk);
            let bv = f32x8::from(*b_chunk);
            *out_chunk = (av * ga + bv * gb).to_array();
        }
        mix_block_scalar(out_tail, a_tail, gain_a, b_tail, gain_b);
    }
    #[cfg(not(feature = "wide-backend"))]
    mix_block_scalar(out, a, gain_a, b, gain_b);
}

/// `out[i] = src[i]`. Equivalent to `copy_from_slice` but exposed
/// in the same surface as the other ops for code that wires up its
/// inner loops from this module.
#[inline]
pub fn copy_block(out: &mut [f32], src: &[f32]) {
    let n = out.len().min(src.len());
    out[..n].copy_from_slice(&src[..n]);
}

/// `out[i] = 0.0` for all `i`.
#[inline]
pub fn zero_block(buf: &mut [f32]) {
    buf.fill(0.0);
}

/// `max(buf[i].abs())`. Returns `0.0` for an empty slice; returns
/// `f32::NAN` on first NaN (so meters can flag a runaway plugin
/// instead of silently reporting in-range peaks).
#[inline]
#[must_use]
pub fn abs_max_block(buf: &[f32]) -> f32 {
    let mut peak = 0.0_f32;
    for &v in buf {
        if v.is_nan() {
            return f32::NAN;
        }
        let a = v.abs();
        if a > peak {
            peak = a;
        }
    }
    peak
}

#[cfg(test)]
mod tests {
    // SIMD outputs are bit-identical to the scalar ones for the
    // ops we ship here (no transcendentals, no fused multiply-add
    // discrepancies on the targets we care about).
    #![allow(clippy::float_cmp, clippy::cast_precision_loss)]

    use super::*;

    #[test]
    fn gain_block_matches_scalar() {
        for n in [0, 1, 7, 8, 9, 16, 31, 32, 33, 128] {
            let init: Vec<f32> = (0..n).map(|i| i as f32 * 0.5 - 1.0).collect();
            let mut a = init.clone();
            let mut b = init.clone();
            gain_block_scalar(&mut a, 0.75);
            gain_block(&mut b, 0.75);
            assert_eq!(a, b, "mismatch at n={n}");
        }
    }

    #[test]
    fn scale_block_matches_scalar() {
        for n in [0, 1, 7, 8, 9, 16, 33] {
            let src: Vec<f32> = (0..n).map(|i| i as f32 * 0.3 - 1.0).collect();
            let mut out_s = vec![0.0; n];
            let mut out_v = vec![0.0; n];
            scale_block_scalar(&mut out_s, &src, 0.5);
            scale_block(&mut out_v, &src, 0.5);
            assert_eq!(out_s, out_v, "mismatch at n={n}");
        }
    }

    #[test]
    fn mul_block_matches_scalar() {
        for n in [0, 1, 7, 8, 33] {
            let a: Vec<f32> = (0..n).map(|i| i as f32 * 0.1).collect();
            let b: Vec<f32> = (0..n).map(|i| i as f32 * -0.2).collect();
            let mut out_s = vec![0.0; n];
            let mut out_v = vec![0.0; n];
            mul_block_scalar(&mut out_s, &a, &b);
            mul_block(&mut out_v, &a, &b);
            assert_eq!(out_s, out_v, "mismatch at n={n}");
        }
    }

    #[test]
    fn mac_block_matches_scalar() {
        for n in [0, 7, 16, 65] {
            let src: Vec<f32> = (0..n).map(|i| i as f32).collect();
            let mut a = vec![1.0; n];
            let mut b = vec![1.0; n];
            mac_block_scalar(&mut a, &src, 0.25);
            mac_block(&mut b, &src, 0.25);
            assert_eq!(a, b);
        }
    }

    #[test]
    fn mix_block_matches_scalar() {
        let a: Vec<f32> = (0..32).map(|i| i as f32).collect();
        let b: Vec<f32> = (0..32).map(|i| (i as f32) * 2.0).collect();
        let mut out_s = vec![0.0; 32];
        let mut out_v = vec![0.0; 32];
        mix_block_scalar(&mut out_s, &a, 0.5, &b, 0.25);
        mix_block(&mut out_v, &a, 0.5, &b, 0.25);
        assert_eq!(out_s, out_v);
    }

    #[test]
    fn abs_max_block_finds_peak() {
        let buf = [-0.1, 0.7, -0.9, 0.3];
        assert!((abs_max_block(&buf) - 0.9).abs() < 1e-6);
        assert_eq!(abs_max_block(&[]), 0.0);
        assert!(abs_max_block(&[1.0, f32::NAN, 2.0]).is_nan());
    }
}
