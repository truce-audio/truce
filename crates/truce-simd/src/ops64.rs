//! Block-rate audio ops at `f64`. Mirror of [`crate::ops`] (f32);
//! see that module for the per-op contracts. The wider lane width
//! means `f64x4` SIMD lanes for the same `wide` backend; chunk
//! granularity is 4 instead of 8.

#[inline]
pub fn gain_block_scalar(buf: &mut [f64], gain: f64) {
    for s in buf {
        *s *= gain;
    }
}

#[inline]
pub fn gain_block(buf: &mut [f64], gain: f64) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let g = f64x4::splat(gain);
        let (chunks, tail) = buf.as_chunks_mut::<4>();
        for chunk in chunks {
            let v = f64x4::from(*chunk);
            *chunk = (v * g).to_array();
        }
        gain_block_scalar(tail, gain);
    }
    #[cfg(not(feature = "wide-backend"))]
    gain_block_scalar(buf, gain);
}

#[inline]
pub fn scale_block_scalar(out: &mut [f64], src: &[f64], scale: f64) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] = src[i] * scale;
    }
}

#[inline]
pub fn scale_block(out: &mut [f64], src: &[f64], scale: f64) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let n = out.len().min(src.len());
        let g = f64x4::splat(scale);
        let (out_chunks, out_tail) = out[..n].as_chunks_mut::<4>();
        let (src_chunks, src_tail) = src[..n].as_chunks::<4>();
        for (out_chunk, src_chunk) in out_chunks.iter_mut().zip(src_chunks) {
            let v = f64x4::from(*src_chunk);
            *out_chunk = (v * g).to_array();
        }
        scale_block_scalar(out_tail, src_tail, scale);
    }
    #[cfg(not(feature = "wide-backend"))]
    scale_block_scalar(out, src, scale);
}

#[inline]
pub fn mul_block_scalar(out: &mut [f64], a: &[f64], b: &[f64]) {
    let n = out.len().min(a.len()).min(b.len());
    for i in 0..n {
        out[i] = a[i] * b[i];
    }
}

#[inline]
pub fn mul_block(out: &mut [f64], a: &[f64], b: &[f64]) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let n = out.len().min(a.len()).min(b.len());
        let (out_chunks, out_tail) = out[..n].as_chunks_mut::<4>();
        let (a_chunks, a_tail) = a[..n].as_chunks::<4>();
        let (b_chunks, b_tail) = b[..n].as_chunks::<4>();
        for ((out_chunk, a_chunk), b_chunk) in out_chunks.iter_mut().zip(a_chunks).zip(b_chunks) {
            let av = f64x4::from(*a_chunk);
            let bv = f64x4::from(*b_chunk);
            *out_chunk = (av * bv).to_array();
        }
        mul_block_scalar(out_tail, a_tail, b_tail);
    }
    #[cfg(not(feature = "wide-backend"))]
    mul_block_scalar(out, a, b);
}

#[inline]
pub fn mac_block_scalar(out: &mut [f64], src: &[f64], scale: f64) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] += src[i] * scale;
    }
}

#[inline]
pub fn mac_block(out: &mut [f64], src: &[f64], scale: f64) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let n = out.len().min(src.len());
        let s = f64x4::splat(scale);
        let (out_chunks, out_tail) = out[..n].as_chunks_mut::<4>();
        let (src_chunks, src_tail) = src[..n].as_chunks::<4>();
        for (out_chunk, src_chunk) in out_chunks.iter_mut().zip(src_chunks) {
            let ov = f64x4::from(*out_chunk);
            let sv = f64x4::from(*src_chunk);
            *out_chunk = (ov + sv * s).to_array();
        }
        mac_block_scalar(out_tail, src_tail, scale);
    }
    #[cfg(not(feature = "wide-backend"))]
    mac_block_scalar(out, src, scale);
}

#[inline]
pub fn mix_block_scalar(out: &mut [f64], a: &[f64], gain_a: f64, b: &[f64], gain_b: f64) {
    let n = out.len().min(a.len()).min(b.len());
    for i in 0..n {
        out[i] = a[i] * gain_a + b[i] * gain_b;
    }
}

#[inline]
pub fn mix_block(out: &mut [f64], a: &[f64], gain_a: f64, b: &[f64], gain_b: f64) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let n = out.len().min(a.len()).min(b.len());
        let ga = f64x4::splat(gain_a);
        let gb = f64x4::splat(gain_b);
        let (out_chunks, out_tail) = out[..n].as_chunks_mut::<4>();
        let (a_chunks, a_tail) = a[..n].as_chunks::<4>();
        let (b_chunks, b_tail) = b[..n].as_chunks::<4>();
        for ((out_chunk, a_chunk), b_chunk) in out_chunks.iter_mut().zip(a_chunks).zip(b_chunks) {
            let av = f64x4::from(*a_chunk);
            let bv = f64x4::from(*b_chunk);
            *out_chunk = (av * ga + bv * gb).to_array();
        }
        mix_block_scalar(out_tail, a_tail, gain_a, b_tail, gain_b);
    }
    #[cfg(not(feature = "wide-backend"))]
    mix_block_scalar(out, a, gain_a, b, gain_b);
}

#[inline]
pub fn copy_block(out: &mut [f64], src: &[f64]) {
    let n = out.len().min(src.len());
    out[..n].copy_from_slice(&src[..n]);
}

#[inline]
pub fn zero_block(buf: &mut [f64]) {
    buf.fill(0.0);
}

#[inline]
#[must_use]
pub fn abs_max_block(buf: &[f64]) -> f64 {
    let mut peak = 0.0_f64;
    for &v in buf {
        if v.is_nan() {
            return f64::NAN;
        }
        let a = v.abs();
        if a > peak {
            peak = a;
        }
    }
    peak
}
