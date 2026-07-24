//! Halfband-cascade oversampling for waveshapers: upsample ->
//! nonlinearity -> downsample, fixed 2x/4x/8x. Not a polyphase
//! resampler. See [`filter`] for the stage design.
//!
//! <https://en.wikipedia.org/wiki/Oversampling>,
//! <https://dspguru.com/dsp/faqs/multirate/resampling/>.

// f64 filter state <-> f32 audio, index/delay casts: load-bearing.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

pub mod filter;

pub use filter::HalfbandFilter;

/// Upsample-by-2 one stage: zero-stuff `src`, filter, writing `dst`
/// (length `2 * src.len()`). `2.0` scale offsets zero-stuffing's
/// half amplitude.
///
/// <https://www.earlevel.com/main/2010/12/11/a-closer-look-at-upsampling-filters/>
fn upsample_stage(filter: &mut HalfbandFilter, src: &[f32], dst: &mut [f32]) {
    debug_assert_eq!(dst.len(), src.len() * 2);
    for (i, &x) in src.iter().enumerate() {
        dst[2 * i] = filter.process_sample(f64::from(x) * 2.0) as f32;
        dst[2 * i + 1] = filter.process_sample(0.0) as f32;
    }
}

/// Downsample-by-2 one stage: push `src` (the high-rate signal)
/// through `filter` to attenuate the image band above the new
/// Nyquist, then decimate by keeping every other filtered sample.
/// Writes `dst` (length `src.len() / 2`).
fn downsample_stage(filter: &mut HalfbandFilter, src: &[f32], dst: &mut [f32]) {
    debug_assert_eq!(src.len(), dst.len() * 2);
    for (i, out) in dst.iter_mut().enumerate() {
        let keep = filter.process_sample(f64::from(src[2 * i]));
        let _discard = filter.process_sample(f64::from(src[2 * i + 1]));
        *out = keep as f32;
    }
}

/// Upsample -> nonlinearity -> downsample at a fixed factor.
///
/// Not `PluginLogic`-aware by design (see crate docs). A plugin
/// wires this in by calling [`Self::prepare`] from its `reset` (the
/// only place allocation is allowed) and [`Self::process_block`]
/// from `process`, and reports [`Self::latency_samples`] from its
/// `latency` hook.
pub struct Oversampler {
    factor: usize,
    up_stages: Vec<HalfbandFilter>,
    down_stages: Vec<HalfbandFilter>,
    /// Ping-pong scratch buffers for the cascade. Both sized to
    /// `max_block_size * factor` in [`Self::prepare`], never
    /// resized in [`Self::process_block`].
    buf_a: Vec<f32>,
    buf_b: Vec<f32>,
}

impl Oversampler {
    /// `factor` in {2, 4, 8}, `taps_per_stage` taps per halfband
    /// filter (up and down stages alike). Not real-time safe;
    /// construct off the audio thread (`init`, not `process`).
    ///
    /// For exact (non-rounded) [`Self::latency_samples`], pick
    /// `taps_per_stage` so `(taps_per_stage - 1) / 2` is divisible by
    /// `factor` (e.g. 17 taps -> delay 8). Otherwise latency rounds
    /// to the nearest sample.
    ///
    /// # Panics
    /// Panics if `factor` is not 2, 4, or 8.
    #[must_use]
    pub fn new(factor: usize, taps_per_stage: usize) -> Self {
        assert!(
            matches!(factor, 2 | 4 | 8),
            "oversampling factor must be 2, 4, or 8, got {factor}"
        );
        let num_stages = factor.trailing_zeros() as usize;
        let up_stages = (0..num_stages)
            .map(|_| HalfbandFilter::design(taps_per_stage))
            .collect();
        let down_stages = (0..num_stages)
            .map(|_| HalfbandFilter::design(taps_per_stage))
            .collect();
        Self {
            factor,
            up_stages,
            down_stages,
            buf_a: Vec::new(),
            buf_b: Vec::new(),
        }
    }

    /// The configured oversampling factor (2, 4, or 8).
    #[must_use]
    pub fn factor(&self) -> usize {
        self.factor
    }

    /// Sizes the scratch buffers for blocks up to `max_block_size`
    /// and clears every stage's filter history. Allocates, so call
    /// from a plugin's `reset` (which receives the host's max block
    /// size for exactly this purpose), never from `process`.
    pub fn prepare(&mut self, max_block_size: usize) {
        let cap = max_block_size * self.factor;
        self.buf_a.clear();
        self.buf_a.resize(cap, 0.0);
        self.buf_b.clear();
        self.buf_b.resize(cap, 0.0);
        for stage in self.up_stages.iter_mut().chain(self.down_stages.iter_mut()) {
            stage.reset();
        }
    }

    /// Sum of every stage's group delay, scaled to the 1x rate,
    /// rounded to the nearest sample. Feed to `PluginLogic::latency`;
    /// see [`Self::new`] for making it exact.
    ///
    /// <https://en.wikipedia.org/wiki/Group_delay_and_phase_delay>
    #[must_use]
    pub fn latency_samples(&self) -> u32 {
        let num_stages = self.up_stages.len() as u32;
        let mut total = 0.0_f64;
        for (s, stage) in self.up_stages.iter().enumerate() {
            let divisor = f64::from(2u32.pow(s as u32 + 1));
            total += f64::from(stage.group_delay()) / divisor;
        }
        for (s, stage) in self.down_stages.iter().enumerate() {
            let divisor = f64::from(2u32.pow(num_stages - s as u32));
            total += f64::from(stage.group_delay()) / divisor;
        }
        total.round() as u32
    }

    /// Runs `block` through upsample -> `nonlinearity` -> downsample
    /// in place. `nonlinearity` receives the block at `factor`x the
    /// rate and length, and can be a no-op (`|_| {}`) to exercise
    /// just the oversampling plumbing, which is exactly what this
    /// crate's own transparency tests do.
    ///
    /// # Panics
    /// Panics if `block.len() * factor()` exceeds the capacity
    /// [`Self::prepare`] was last called with.
    pub fn process_block(&mut self, block: &mut [f32], mut nonlinearity: impl FnMut(&mut [f32])) {
        let n = block.len();
        let total = n * self.factor;
        assert!(
            total <= self.buf_a.len(),
            "block of {n} samples * factor {} exceeds the capacity prepare() was called with \
             ({} samples). Call prepare(max_block_size) with a large enough size first",
            self.factor,
            self.buf_a.len() / self.factor.max(1),
        );

        let Self {
            up_stages,
            down_stages,
            buf_a,
            buf_b,
            ..
        } = self;

        // Upsample cascade, ping-ponging through buf_a/buf_b.
        let mut cur_len = n * 2;
        upsample_stage(&mut up_stages[0], block, &mut buf_a[..cur_len]);
        let mut in_a = true;
        for stage in up_stages.iter_mut().skip(1) {
            let next_len = cur_len * 2;
            if in_a {
                let (src, dst) = (&buf_a[..cur_len], &mut buf_b[..next_len]);
                upsample_stage(stage, src, dst);
            } else {
                let (src, dst) = (&buf_b[..cur_len], &mut buf_a[..next_len]);
                upsample_stage(stage, src, dst);
            }
            in_a = !in_a;
            cur_len = next_len;
        }
        debug_assert_eq!(cur_len, total);

        // Nonlinearity at the oversampled rate.
        if in_a {
            nonlinearity(&mut buf_a[..total]);
        } else {
            nonlinearity(&mut buf_b[..total]);
        }

        // Downsample cascade, mirroring the upsample order.
        for stage in down_stages.iter_mut() {
            let next_len = cur_len / 2;
            if in_a {
                let (src, dst) = (&buf_a[..cur_len], &mut buf_b[..next_len]);
                downsample_stage(stage, src, dst);
            } else {
                let (src, dst) = (&buf_b[..cur_len], &mut buf_a[..next_len]);
                downsample_stage(stage, src, dst);
            }
            in_a = !in_a;
            cur_len = next_len;
        }
        debug_assert_eq!(cur_len, n);

        if in_a {
            block.copy_from_slice(&buf_a[..n]);
        } else {
            block.copy_from_slice(&buf_b[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 17 taps -> group delay 8, divisible by 2/4/8, so
    /// `latency_samples()` is exact rather than rounded (see
    /// [`Oversampler::new`] docs)
    const TEST_TAPS: usize = 17;

    #[test]
    fn factor_accessor_matches_construction() {
        for &factor in &[2usize, 4, 8] {
            assert_eq!(Oversampler::new(factor, TEST_TAPS).factor(), factor);
        }
    }

    #[test]
    #[should_panic(expected = "factor must be 2, 4, or 8")]
    fn invalid_factor_panics() {
        let _ = Oversampler::new(3, TEST_TAPS);
    }

    #[test]
    fn stage_count_matches_log2_factor() {
        assert_eq!(Oversampler::new(2, TEST_TAPS).up_stages.len(), 1);
        assert_eq!(Oversampler::new(4, TEST_TAPS).up_stages.len(), 2);
        assert_eq!(Oversampler::new(8, TEST_TAPS).up_stages.len(), 3);
    }

    /// Impulse in -> single peak at reported latency.
    #[test]
    fn impulse_response_peak_at_reported_latency() {
        for &factor in &[2usize, 4, 8] {
            let mut os = Oversampler::new(factor, TEST_TAPS);
            os.prepare(64);
            let mut block = vec![0.0_f32; 64];
            block[0] = 1.0;
            os.process_block(&mut block, |_| {});

            let latency = os.latency_samples() as usize;
            let (peak_idx, &peak_val) = block
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
                .expect("non-empty block");

            assert_eq!(
                peak_idx, latency,
                "factor {factor}: impulse peak at {peak_idx}, expected latency {latency}"
            );
            assert!(
                peak_val > 0.1,
                "factor {factor}: expected a clear positive peak, got {peak_val}"
            );
        }
    }

    /// Identity closure: sub-Nyquist sine round-trips shifted by
    /// `latency_samples()`.
    #[test]
    fn sine_round_trip_is_transparent() {
        let sample_rate = 48_000.0_f64;
        let freq = 1_000.0_f64; // well below Nyquist at every factor
        let n = 2000;

        for &factor in &[2usize, 4, 8] {
            let mut os = Oversampler::new(factor, TEST_TAPS);
            os.prepare(n);

            let mut block: Vec<f32> = (0..n)
                .map(|i| {
                    (2.0 * std::f64::consts::PI * freq * (i as f64) / sample_rate).sin() as f32
                })
                .collect();
            os.process_block(&mut block, |_| {});

            let latency = os.latency_samples() as usize;
            // Skip the filter cascade's startup transient (state
            // starts at zero, not phase-matched to the sine).
            let warmup = latency + 8 * TEST_TAPS * (factor.trailing_zeros() as usize);

            let mut max_err = 0.0_f32;
            for (i, &sample) in block.iter().enumerate().skip(warmup.min(n)) {
                let src_idx = i - latency;
                let expected = (2.0 * std::f64::consts::PI * freq * (src_idx as f64) / sample_rate)
                    .sin() as f32;
                max_err = max_err.max((sample - expected).abs());
            }
            assert!(
                max_err < 0.02,
                "factor {factor}: max round-trip error {max_err} exceeds tolerance"
            );
        }
    }
}
