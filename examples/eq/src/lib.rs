//! 3-band parametric EQ using biquad filters.
//!
//! Each band has frequency, gain, and Q controls. Demonstrates
//! multi-parameter DSP, filter state management, and parameter groups.

use truce::prelude::*;

mod biquad;
use biquad::Biquad;

// --- Parameters ---

use EqParamsParamId as P;

#[derive(Params)]
pub struct EqParams {
    #[param(id = 0, name = "Low Freq", short_name = "LFreq",
            group = "Low", range = "log(20, 1000)",
            default = 200.0, unit = "Hz", smooth = "exp(10)")]
    pub low_freq: FloatParam,

    #[param(id = 1, name = "Low Gain", short_name = "LGain",
            group = "Low", range = "linear(-18, 18)",
            unit = "dB", smooth = "exp(10)")]
    pub low_gain: FloatParam,

    #[param(id = 2, name = "Low Q", short_name = "LQ",
            group = "Low", range = "log(0.1, 10)",
            default = 0.707, smooth = "exp(10)")]
    pub low_q: FloatParam,

    #[param(id = 3, name = "Mid Freq", short_name = "MFreq",
            group = "Mid", range = "log(200, 8000)",
            default = 1000.0, unit = "Hz", smooth = "exp(10)")]
    pub mid_freq: FloatParam,

    #[param(id = 4, name = "Mid Gain", short_name = "MGain",
            group = "Mid", range = "linear(-18, 18)",
            unit = "dB", smooth = "exp(10)")]
    pub mid_gain: FloatParam,

    #[param(id = 5, name = "Mid Q", short_name = "MQ",
            group = "Mid", range = "log(0.1, 10)",
            default = 0.707, smooth = "exp(10)")]
    pub mid_q: FloatParam,

    #[param(id = 6, name = "High Freq", short_name = "HFreq",
            group = "High", range = "log(1000, 20000)",
            default = 5000.0, unit = "Hz", smooth = "exp(10)")]
    pub high_freq: FloatParam,

    #[param(id = 7, name = "High Gain", short_name = "HGain",
            group = "High", range = "linear(-18, 18)",
            unit = "dB", smooth = "exp(10)")]
    pub high_gain: FloatParam,

    #[param(id = 8, name = "High Q", short_name = "HQ",
            group = "High", range = "log(0.1, 10)",
            default = 0.707, smooth = "exp(10)")]
    pub high_q: FloatParam,

    #[param(id = 9, name = "Output", short_name = "Out",
            range = "linear(-18, 18)", unit = "dB", smooth = "exp(5)")]
    pub output: FloatParam,
}

// --- Plugin ---

const NUM_BANDS: usize = 3;
const MAX_CHANNELS: usize = 2;

pub struct Eq {
    pub params: EqParams,
    filters: [[Biquad; NUM_BANDS]; MAX_CHANNELS],
    sample_rate: f64,
}
impl PluginLogic for Eq {
    fn new() -> Self {
        Self {
            params: EqParams::new(),
            filters: [[Biquad::new(); NUM_BANDS]; MAX_CHANNELS],
            sample_rate: 44100.0,
        }
    }

    fn params_mut(&mut self) -> Option<&mut dyn Params> {
        Some(&mut self.params)
    }

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.sample_rate = sample_rate;
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
        for ch in &mut self.filters {
            for band in ch {
                band.reset();
            }
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer, _events: &EventList, _context: &mut ProcessContext) -> ProcessStatus {
        let sr = self.sample_rate;
        let num_ch = buffer.channels().min(MAX_CHANNELS);

        for i in 0..buffer.num_samples() {
            // Read smoothed parameters
            let low_freq = self.params.low_freq.smoothed_next() as f64;
            let low_gain = self.params.low_gain.smoothed_next() as f64;
            let low_q = self.params.low_q.smoothed_next() as f64;
            let mid_freq = self.params.mid_freq.smoothed_next() as f64;
            let mid_gain = self.params.mid_gain.smoothed_next() as f64;
            let mid_q = self.params.mid_q.smoothed_next() as f64;
            let high_freq = self.params.high_freq.smoothed_next() as f64;
            let high_gain = self.params.high_gain.smoothed_next() as f64;
            let high_q = self.params.high_q.smoothed_next() as f64;
            let output = db_to_linear(self.params.output.smoothed_next() as f64);

            for ch in 0..num_ch {
                // Update filter coefficients per-sample (smoothed params change each sample)
                self.filters[ch][0].set_low_shelf(low_freq, low_gain, low_q, sr);
                self.filters[ch][1].set_peaking(mid_freq, mid_gain, mid_q, sr);
                self.filters[ch][2].set_high_shelf(high_freq, high_gain, high_q, sr);

                let (inp, out) = buffer.io(ch);
                let mut sample = inp[i] as f64;
                for band in &mut self.filters[ch] {
                    sample = band.process(sample);
                }
                out[i] = (sample * output) as f32;
            }
        }

        ProcessStatus::Normal
    }

    fn layout(&self) -> truce_gui::layout::GridLayout {
        use truce_gui::layout::{GridLayout, GridWidget};
        GridLayout::build("EQ", "V0.1", 3, 70.0, vec![
            GridWidget::knob(P::LowFreq, "Freq"),
            GridWidget::knob(P::LowGain, "Gain"),
            GridWidget::knob(P::LowQ, "Q"),
            GridWidget::knob(P::MidFreq, "Freq"),
            GridWidget::knob(P::MidGain, "Gain"),
            GridWidget::knob(P::MidQ, "Q"),
            GridWidget::knob(P::HighFreq, "Freq"),
            GridWidget::knob(P::HighGain, "Gain"),
            GridWidget::knob(P::HighQ, "Q"),
            GridWidget::knob(P::Output, "Output"),
        ], vec![
            (0, "LOW"),
            (3, "MID"),
            (6, "HIGH"),
        ])
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
        let result = truce_test::render_effect::<Plugin>(512, 44100.0);
        truce_test::assert_nonzero(&result.output);
        truce_test::assert_no_nans(&result.output);
    }

    #[test]
    fn flat_eq_passes_audio() {
        // Default EQ (0dB gain on all bands) should pass audio ~unchanged
        let result = truce_test::render_effect::<Plugin>(512, 44100.0);
        let max = result.output[0]
            .iter()
            .map(|s| s.abs())
            .fold(0.0f32, f32::max);
        assert!(max > 0.4, "Flat EQ should pass audio near unity, got {max}");
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

    #[test]
    fn gui_snapshot() {
        let eq = Eq::new();
        let layout = eq.layout();
        let params = std::sync::Arc::new(EqParams::new());
        truce_test::assert_gui_snapshot_grid::<EqParams>(
            "eq_default", params, layout, 0,
        );
    }
}
