//! 3-band parametric EQ using the upstream
//! [`biquad`](https://crates.io/crates/biquad) crate. Each band
//! has frequency, gain, and Q controls.
//!
//! Each filter is `DirectForm2Transposed::<f64, StereoSample>`,
//! where `StereoSample` is a local newtype over `wide::f64x2`
//! (the glue lives in this crate's `src/biquad.rs`). The biquad
//! crate's generic-T trait split lets us swap a scalar f64
//! sample for a packed f64x2 without forking the crate; the
//! filter inner loop then advances both channels through one
//! SIMD register pair, which on Apple Silicon NEON / x86 AVX2
//! is roughly half the cycles of two scalar biquads.

mod biquad;

use ::biquad::{Biquad, DirectForm2Transposed};
use biquad::{StereoSample, high_shelf, low_shelf, peaking};
use truce::prelude64::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, section, widgets};

type StereoBiquad = DirectForm2Transposed<f64, StereoSample>;

// --- Parameters ---

use EqParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct EqParams {
    #[param(
        name = "Low Freq",
        short_name = "LFreq",
        group = "Low",
        range = "log(20, 1000)",
        default = 200.0,
        unit = "Hz",
        smooth = "exp(10)"
    )]
    pub low_freq: FloatParam,

    #[param(
        name = "Low Gain",
        short_name = "LGain",
        group = "Low",
        range = "linear(-18, 18)",
        unit = "dB",
        smooth = "exp(10)"
    )]
    pub low_gain: FloatParam,

    #[param(
        name = "Low Q",
        short_name = "LQ",
        group = "Low",
        range = "log(0.1, 10)",
        default = 0.707,
        smooth = "exp(10)"
    )]
    pub low_q: FloatParam,

    #[param(
        name = "Mid Freq",
        short_name = "MFreq",
        group = "Mid",
        range = "log(200, 8000)",
        default = 1000.0,
        unit = "Hz",
        smooth = "exp(10)"
    )]
    pub mid_freq: FloatParam,

    #[param(
        name = "Mid Gain",
        short_name = "MGain",
        group = "Mid",
        range = "linear(-18, 18)",
        unit = "dB",
        smooth = "exp(10)"
    )]
    pub mid_gain: FloatParam,

    #[param(
        name = "Mid Q",
        short_name = "MQ",
        group = "Mid",
        range = "log(0.1, 10)",
        default = 0.707,
        smooth = "exp(10)"
    )]
    pub mid_q: FloatParam,

    #[param(
        name = "High Freq",
        short_name = "HFreq",
        group = "High",
        range = "log(1000, 20000)",
        default = 5000.0,
        unit = "Hz",
        smooth = "exp(10)"
    )]
    pub high_freq: FloatParam,

    #[param(
        name = "High Gain",
        short_name = "HGain",
        group = "High",
        range = "linear(-18, 18)",
        unit = "dB",
        smooth = "exp(10)"
    )]
    pub high_gain: FloatParam,

    #[param(
        name = "High Q",
        short_name = "HQ",
        group = "High",
        range = "log(0.1, 10)",
        default = 0.707,
        smooth = "exp(10)"
    )]
    pub high_q: FloatParam,

    #[param(
        name = "Output",
        short_name = "Out",
        range = "linear(-18, 18)",
        unit = "dB",
        smooth = "exp(5)"
    )]
    pub output: FloatParam,
}

// --- Plugin ---

const NUM_BANDS: usize = 3;

/// Upper bound on the audio block size we'll service. Each of the
/// 10 smoothed shape params gets a `[f64; MAX_BLOCK]` scratch
/// (~40 KB total) so the per-sample inner loop indexes into stack
/// arrays instead of calling 10 atomic loads per sample.
const MAX_BLOCK: usize = 512;

pub struct Eq {
    pub params: Arc<EqParams>,
    /// One stereo filter per band; L and R advance through the
    /// same coefficients in parallel `f64x2` lanes. Mono input
    /// feeds both lanes and the L lane is the sole output.
    bands: [StereoBiquad; NUM_BANDS],
    sample_rate: f64,
}

/// Identity coefficients for a fresh biquad (passes input through
/// unchanged). `reset()` writes the real coefficients before the
/// first audio block.
fn identity_coeffs() -> ::biquad::Coefficients<f64> {
    ::biquad::Coefficients {
        a1: 0.0,
        a2: 0.0,
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
    }
}

impl Eq {
    pub fn new(params: Arc<EqParams>) -> Self {
        let id = identity_coeffs();
        Self {
            params,
            bands: [
                StereoBiquad::new(id),
                StereoBiquad::new(id),
                StereoBiquad::new(id),
            ],
            sample_rate: 44100.0,
        }
    }
}

impl PluginLogic for Eq {
    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.sample_rate = sample_rate;
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
        for band in &mut self.bands {
            band.reset_state();
        }
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        let sr = self.sample_rate;
        let num_ch = buffer.channels();
        let stereo = num_ch >= 2;
        let total = buffer.num_samples();

        // Walk the buffer in `MAX_BLOCK`-sized chunks. Hosts can hand
        // us blocks larger than `MAX_BLOCK` (1024 / 2048 are common
        // DAW buffer sizes), so a single fill wouldn't cover the
        // whole buffer. Scratch arrays hoist out of the loop so we
        // pay one stack-allocation rather than ten per iteration.
        let mut low_freq = [0.0_f64; MAX_BLOCK];
        let mut low_gain = [0.0_f64; MAX_BLOCK];
        let mut low_q = [0.0_f64; MAX_BLOCK];
        let mut mid_freq = [0.0_f64; MAX_BLOCK];
        let mut mid_gain = [0.0_f64; MAX_BLOCK];
        let mut mid_q = [0.0_f64; MAX_BLOCK];
        let mut high_freq = [0.0_f64; MAX_BLOCK];
        let mut high_gain = [0.0_f64; MAX_BLOCK];
        let mut high_q = [0.0_f64; MAX_BLOCK];
        let mut output = [0.0_f64; MAX_BLOCK];
        let mut out_lin = [0.0_f64; MAX_BLOCK];

        let mut offset = 0;
        while offset < total {
            let n = (total - offset).min(MAX_BLOCK);

            // Slice-based read advances each smoother by exactly `n`
            // - one atomic-pair per param (10 pairs total) regardless
            // of chunk length, vs one pair per param per sample if we
            // called `.read()` in the inner loop. Coefficient updates
            // still happen per sample so a fast knob sweep stays
            // click-free; only the smoother traffic moves out of the
            // inner loop.
            self.params.low_freq.read_into(&mut low_freq[..n]);
            self.params.low_gain.read_into(&mut low_gain[..n]);
            self.params.low_q.read_into(&mut low_q[..n]);
            self.params.mid_freq.read_into(&mut mid_freq[..n]);
            self.params.mid_gain.read_into(&mut mid_gain[..n]);
            self.params.mid_q.read_into(&mut mid_q[..n]);
            self.params.high_freq.read_into(&mut high_freq[..n]);
            self.params.high_gain.read_into(&mut high_gain[..n]);
            self.params.high_q.read_into(&mut high_q[..n]);
            self.params.output.read_into(&mut output[..n]);

            // Vectorize the output dB → linear conversion once per
            // chunk. f64::powf is opaque to LLVM's autovectorizer;
            // truce_simd::math64::db_to_linear_block routes through
            // wide::f64x4's native `exp`, so the per-sample inner
            // loop just reads a precomputed linear gain.
            truce_simd::math64::db_to_linear_block(&mut out_lin[..n], &output[..n]);

            for i in 0..n {
                let idx = offset + i;

                // Per-sample coefficient updates because every shape
                // param is smoothed; one coefficient set serves both
                // channels.
                self.bands[0].update_coefficients(low_shelf(
                    low_freq[i],
                    low_gain[i],
                    low_q[i],
                    sr,
                ));
                self.bands[1].update_coefficients(peaking(mid_freq[i], mid_gain[i], mid_q[i], sr));
                self.bands[2].update_coefficients(high_shelf(
                    high_freq[i],
                    high_gain[i],
                    high_q[i],
                    sr,
                ));

                // Read both channels (or duplicate the mono channel
                // into the second lane) before advancing any filter,
                // so the L and R lanes step in lockstep.
                let in_l = buffer.io(0).0[idx];
                let in_r = if stereo { buffer.io(1).0[idx] } else { in_l };

                let mut sample = StereoSample::from_lr(in_l, in_r);
                for band in &mut self.bands {
                    sample = band.run(sample);
                }
                let (l, r) = sample.to_lr();
                let out_gain = out_lin[i];
                buffer.io(0).1[idx] = l * out_gain;
                if stereo {
                    buffer.io(1).1[idx] = r * out_gain;
                }
            }

            offset += n;
        }

        ProcessStatus::Normal
    }

    fn editor(&self) -> Box<dyn Editor> {
        GridLayout::build(vec![
            section(
                "LOW",
                vec![
                    knob(P::LowFreq, "Freq"),
                    knob(P::LowGain, "Gain"),
                    knob(P::LowQ, "Q"),
                ],
            ),
            section(
                "MID",
                vec![
                    knob(P::MidFreq, "Freq"),
                    knob(P::MidGain, "Gain"),
                    knob(P::MidQ, "Q"),
                ],
            ),
            section(
                "HIGH",
                vec![
                    knob(P::HighFreq, "Freq"),
                    knob(P::HighGain, "Gain"),
                    knob(P::HighQ, "Q"),
                ],
            ),
            widgets(vec![knob(P::Output, "Output")]),
        ])
        .with_title("EQ")
        .into_editor(&self.params)
    }
}

truce::plugin! {
    logic: Eq,
    params: EqParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn renders_nonzero_output() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};
        let result = driver!(Plugin)
            .duration(Duration::from_millis(12))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_nonzero(&result);
        assertions::assert_no_nans(&result);
    }

    #[test]
    fn flat_eq_passes_audio() {
        use std::time::Duration;
        use truce_test::{InputSource, driver};
        // Default EQ (0dB gain on all bands) should pass audio ~unchanged
        let result = driver!(Plugin)
            .duration(Duration::from_millis(12))
            .input(InputSource::Constant(0.5))
            .run();
        let max = result.output[0]
            .iter()
            .map(|s| s.abs())
            .fold(0.0f32, f32::max);
        assert!(max > 0.4, "Flat EQ should pass audio near unity, got {max}");
    }

    #[test]
    fn large_block_fills_whole_output() {
        use std::time::Duration;
        use truce_test::{InputSource, driver};
        // Hosts can hand us blocks larger than the plugin's internal
        // `MAX_BLOCK`; `process` must walk the whole buffer. A 2048
        // block (common DAW setting) used to leave samples 512..2048
        // pass-through stale, producing periodic noise.
        let result = driver!(Plugin)
            .block_size(2048)
            .duration(Duration::from_millis(100))
            .input(InputSource::Constant(0.5))
            .run();
        let min = result.output[0]
            .iter()
            .map(|s| s.abs())
            .fold(f32::INFINITY, f32::min);
        assert!(
            min > 0.4,
            "Every sample of a flat-EQ pass-through should be near unity; \
             min was {min} (tail of large block was dropped on the floor)"
        );
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    #[test]
    fn state_round_trips() {
        truce_test::assert_state_round_trip::<Plugin>();
    }

    // --- AU metadata ---

    #[test]
    fn au_type_codes_ascii() {
        truce_test::assert_au_type_codes_ascii::<Plugin>();
    }

    #[test]
    fn fourcc_roundtrip() {
        truce_test::assert_fourcc_roundtrip::<Plugin>();
    }

    #[test]
    fn bus_config_effect() {
        truce_test::assert_bus_config_effect::<Plugin>();
    }

    // --- GUI lifecycle ---

    #[test]
    fn editor_lifecycle() {
        truce_test::assert_editor_lifecycle::<Plugin>();
    }

    #[test]
    fn editor_size_consistent() {
        truce_test::assert_editor_size_consistent::<Plugin>();
    }

    // --- Parameters ---

    #[test]
    fn param_defaults_match() {
        truce_test::assert_param_defaults_match::<Plugin>();
    }

    #[test]
    fn param_normalized_clamped() {
        truce_test::assert_param_normalized_clamped::<Plugin>();
    }

    #[test]
    fn param_normalized_roundtrip() {
        truce_test::assert_param_normalized_roundtrip::<Plugin>();
    }

    #[test]
    fn param_count_matches() {
        truce_test::assert_param_count_matches::<Plugin>();
    }

    #[test]
    fn no_duplicate_param_ids() {
        truce_test::assert_no_duplicate_param_ids::<Plugin>();
    }

    // --- State resilience ---

    #[test]
    fn corrupt_state_no_crash() {
        truce_test::assert_corrupt_state_no_crash::<Plugin>();
    }

    #[test]
    fn empty_state_no_crash() {
        truce_test::assert_empty_state_no_crash::<Plugin>();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/eq_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/eq_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/eq_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
