//! Halfband lowpass FIR, cutoff Fs/4: even taps exactly 0, center 0.5;
//! one design serves both up and downsample. Windowed-sinc, built once
//! at construction.
//!
//! <https://ccrma.stanford.edu/~jos/sasp/Window_Method_FIR_Filter.html>

// This module converts between taps and samples. Tap counts and delays
// never approach 2^32, so the casts are deliberate and bounded.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

/// Approximated kaiser window beta for ~80 dB stopband attenuation
/// (`beta = 0.1102 * (A - 8.7)` for `A > 50`). Affects filter *design*
/// time, not `process_sample`.
///
/// See <https://en.wikipedia.org/wiki/Kaiser_window>.
const KAISER_BETA: f64 = 7.857;

/// Zeroth-order modified Bessel function of the first kind, via its
/// power series. Used by the Kaiser window formula; converges fast for
/// the `x` range Kaiser windows need (`x <= beta <~ 12`), and the
/// loop's early-exit bounds it regardless.
///
/// See <https://en.wikipedia.org/wiki/Bessel_function>.
fn bessel_i0(x: f64) -> f64 {
    let y = x / 2.0;
    let mut term = 1.0_f64;
    let mut sum = 1.0_f64;
    for k in 1..64 {
        term *= (y / f64::from(k)).powi(2);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// One halfband stage's taps + delay line; up and down directions
/// share the same design, so [`Oversampler`](crate::Oversampler)
/// builds one instance per direction per stage.
pub struct HalfbandFilter {
    /// Symmetric FIR taps, length `num_taps` (odd). Every even-offset
    /// tap around the center is exactly `0.0` by construction (see
    /// module docs), stored dense rather than compacted, so
    /// `process_sample` stays a plain convolution instead of a
    /// sparse-tap special case.
    taps: Vec<f64>,
    /// Ring buffer of the last `num_taps` input samples.
    delay: Vec<f64>,
    /// Index the *next* sample gets written to in `delay`.
    pos: usize,
}

impl HalfbandFilter {
    /// Designs a halfband lowpass with `num_taps` taps (must be odd so
    /// the impulse response has an exact integer-sample center, i.e. an
    /// exact integer group delay). Not real-time safe and should only
    /// be called from a plugin's construction path, never from
    /// `process`.
    ///
    /// # Panics
    /// Panics if `num_taps` is even or less than 3.
    #[must_use]
    pub fn design(num_taps: usize) -> Self {
        assert!(
            num_taps >= 3 && num_taps % 2 == 1,
            "halfband filter needs an odd tap count >= 3, got {num_taps}"
        );
        let center = (num_taps - 1) / 2;
        let alpha = center as f64;

        let mut taps = vec![0.0_f64; num_taps];
        for (n, tap) in taps.iter_mut().enumerate() {
            let k = n as isize - center as isize;
            // h[n] = 0.5 * sinc(k/2). sin(pi*k/2) is 0 for even k,
            // +-1 for odd k mod 4 - integer case analysis avoids
            // f64::sin rounding breaking the exact-zero taps.
            let ideal = if k == 0 {
                0.5
            } else if k % 2 == 0 {
                0.0
            } else {
                let sign = if k.rem_euclid(4) == 1 { 1.0 } else { -1.0 };
                sign / (std::f64::consts::PI * k as f64)
            };

            // Kaiser window.
            let ratio = (n as f64 - alpha) / alpha;
            let inside = (1.0 - ratio * ratio).max(0.0);
            let window = bessel_i0(KAISER_BETA * inside.sqrt()) / bessel_i0(KAISER_BETA);

            *tap = ideal * window;
        }

        Self {
            taps,
            delay: vec![0.0_f64; num_taps],
            pos: 0,
        }
    }

    /// Group delay in samples: exact (not approximate) for a
    /// symmetric linear-phase FIR with an odd tap count: it's the
    /// center tap's index.
    ///
    /// See <https://en.wikipedia.org/wiki/Group_delay_and_phase_delay> and
    /// <https://ccrma.stanford.edu/~jos/filters/Group_Delay.html>.
    #[must_use]
    pub fn group_delay(&self) -> u32 {
        ((self.taps.len() - 1) / 2) as u32
    }

    /// Push one input sample through the filter, returning the
    /// corresponding output sample (delayed by [`Self::group_delay`]
    /// samples relative to the input).
    pub fn process_sample(&mut self, x: f64) -> f64 {
        let len = self.delay.len();
        self.delay[self.pos] = x;

        let mut acc = 0.0_f64;
        for (i, &tap) in self.taps.iter().enumerate() {
            let idx = (self.pos + len - i) % len;
            acc += tap * self.delay[idx];
        }

        self.pos = (self.pos + 1) % len;
        acc
    }

    /// Clears the delay line and resets the write position, so the
    /// filter starts a fresh activation with no leftover history
    /// from a prior sample rate / block size.
    pub fn reset(&mut self) {
        self.delay.fill(0.0);
        self.pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::float_cmp)] // exact zero is the property under test, not a tolerance check
    fn even_offset_taps_are_exactly_zero() {
        let f = HalfbandFilter::design(15);
        let center = 7;
        for (n, &tap) in f.taps.iter().enumerate() {
            let k = n as isize - center as isize;
            if k != 0 && k % 2 == 0 {
                assert_eq!(tap, 0.0, "tap at offset {k} (n={n}) should be exactly zero");
            }
        }
    }

    #[test]
    fn center_tap_is_close_to_half() {
        // Windowed, so not exactly 0.5, but the window is 1.0 at
        // its own center by construction.
        let f = HalfbandFilter::design(15);
        assert!((f.taps[7] - 0.5).abs() < 1e-12);
    }

    #[test]
    fn taps_are_symmetric() {
        let f = HalfbandFilter::design(31);
        let n = f.taps.len();
        for i in 0..n {
            assert!(
                (f.taps[i] - f.taps[n - 1 - i]).abs() < 1e-15,
                "tap {i} and {} should be mirror-symmetric",
                n - 1 - i
            );
        }
    }

    #[test]
    fn group_delay_matches_center_index() {
        assert_eq!(HalfbandFilter::design(15).group_delay(), 7);
        assert_eq!(HalfbandFilter::design(31).group_delay(), 15);
        assert_eq!(HalfbandFilter::design(63).group_delay(), 31);
    }

    #[test]
    fn dc_gain_is_close_to_unity() {
        // A lowpass should pass DC (0 Hz) with ~unity gain: sum of
        // taps ~= 1.0.
        let f = HalfbandFilter::design(31);
        let sum: f64 = f.taps.iter().sum();
        assert!((sum - 1.0).abs() < 1e-3, "DC gain sum = {sum}");
    }

    #[test]
    #[should_panic(expected = "odd tap count")]
    fn even_tap_count_panics() {
        let _ = HalfbandFilter::design(16);
    }

    #[test]
    fn impulse_response_matches_taps() {
        // Feeding a unit impulse through the filter should read
        // back the (time-reversed via convolution, but this filter
        // is symmetric so it doesn't matter) tap coefficients.
        let num_taps = 15;
        let mut f = HalfbandFilter::design(num_taps);
        let mut out = Vec::with_capacity(num_taps);
        out.push(f.process_sample(1.0));
        for _ in 1..num_taps {
            out.push(f.process_sample(0.0));
        }
        for (i, (&out_i, &tap_i)) in out.iter().zip(f.taps.iter()).enumerate() {
            assert!(
                (out_i - tap_i).abs() < 1e-12,
                "impulse output[{i}] = {out_i}, expected tap {tap_i}"
            );
        }
    }
}
