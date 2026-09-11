//! A separate step from decode, so the caller controls when, and
//! whether, resampling runs. Linear interpolation only: quality is
//! out of scope for this crate.

use std::sync::Arc;

use crate::decode::DecodedAudio;

/// Resample every channel to `target_sample_rate`.
///
/// Returns `decoded` unchanged if `decoded.sample_rate` already
/// matches `target_sample_rate`.
#[must_use]
// `target_sample_rate` is a fixed host value, not a computed float,
// so an exact match means no resampling is needed.
#[allow(clippy::float_cmp)]
pub fn resample(decoded: DecodedAudio, target_sample_rate: f64) -> DecodedAudio {
    if decoded.sample_rate == target_sample_rate {
        return decoded;
    }
    let ratio = target_sample_rate / decoded.sample_rate;
    let channels = decoded
        .channels
        .iter()
        .map(|ch| Arc::from(resample_linear(ch, ratio)))
        .collect();
    DecodedAudio {
        channels,
        sample_rate: target_sample_rate,
    }
}

/// Resample `input` by `ratio`, using linear interpolation between samples.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn resample_linear(input: &[f32], ratio: f64) -> Vec<f32> {
    if input.is_empty() {
        return Vec::new();
    }
    let out_len = ((input.len() as f64) * ratio).round().max(1.0) as usize;
    let last = input.len() - 1;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 / ratio;
        let idx = (src_pos.floor() as usize).min(last);
        let frac = (src_pos - idx as f64) as f32;
        let a = input[idx];
        let b = input[(idx + 1).min(last)];
        out.push(a + (b - a) * frac);
    }
    out
}
