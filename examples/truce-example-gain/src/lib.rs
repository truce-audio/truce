use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, meter, widgets, xy_pad};

// --- Parameters ---

use GainParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct GainParams {
    #[param(
        name = "Gain",
        range = "linear(-60, 6)",
        unit = "dB",
        smooth = "exp(5)"
    )]
    pub gain: FloatParam,

    #[param(name = "Pan", range = "linear(-1, 1)", unit = "pan", smooth = "exp(5)")]
    pub pan: FloatParam,

    #[meter]
    pub meter_left: MeterSlot,

    #[meter]
    pub meter_right: MeterSlot,
}

// --- Plugin ---

pub struct Gain {
    params: Arc<GainParams>,
}

impl Gain {
    pub fn new(params: Arc<GainParams>) -> Self {
        Self { params }
    }
}

impl PluginLogic for Gain {
    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        for i in 0..buffer.num_samples() {
            let gain_db = self.params.gain.read();
            let pan = self.params.pan.read();
            let gain_linear = db_to_linear(gain_db);

            let gain_l = gain_linear * (1.0 - pan.max(0.0));
            let gain_r = gain_linear * (1.0 + pan.min(0.0));

            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                let g = if ch == 0 { gain_l } else { gain_r };
                out[i] = inp[i] * g;
            }
        }

        if buffer.num_output_channels() >= 1 {
            context.set_meter(P::MeterLeft, buffer.output_peak(0));
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MeterRight, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn editor(&self) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::Gain, "Gain"),
            knob(P::Pan, "Pan"),
            meter(&[P::MeterLeft, P::MeterRight], "Level")
                .at(2, 0)
                .rows(3),
            xy_pad(P::Pan, P::Gain, "XY"),
        ])])
        .with_title("GAIN")
        .into_editor(&self.params)
    }
}

truce::plugin! {
    logic: Gain,
    params: GainParams,
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
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    #[test]
    fn state_round_trips() {
        truce_test::assert_state_round_trip::<Plugin>();
    }

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

    #[test]
    fn editor_lifecycle() {
        truce_test::assert_editor_lifecycle::<Plugin>();
    }

    #[test]
    fn editor_size_consistent() {
        truce_test::assert_editor_size_consistent::<Plugin>();
    }

    #[test]
    fn param_defaults_match() {
        truce_test::assert_param_defaults_match::<Plugin>();
    }

    /// Sample-accurate chunking: a `set_param` scheduled mid-block
    /// shouldn't show up in the output until its sample offset.
    ///
    /// We script a step from +6 dB (gain=1.0) to -60 dB (gain~=0.001)
    /// at sample 4800 of a 9600-sample (200 ms) run @ 48 kHz, with a
    /// constant unit-DC input. With the chunker doing its job, the
    /// first ~100 samples should hold near 6 dB (~2.0) and the last
    /// samples should be near -60 dB (~0.001) - the smoother needs
    /// time to ramp, but it should start ramping AT the event sample,
    /// not at sample 0 of the block the event lands in. The pre-event
    /// segment is therefore solidly +6 dB; the eager-apply behaviour
    /// would have shoved it toward -60 dB starting from sample 0.
    #[test]
    fn set_param_chunks_at_event_sample() {
        use std::time::Duration;
        use truce_test::{InputSource, driver};

        let sr = 48_000.0;
        let result = driver!(Plugin)
            .sample_rate(sr)
            .duration(Duration::from_millis(200))
            .input(InputSource::Constant(1.0))
            // Pre-load +6 dB so the run starts already snapped to the
            // max of the gain range (avoids the first-block ramp from
            // the default into the test's starting target). The
            // `PluginDriver::set_param` setter goes through
            // `Params::set_normalized` (normalized [0, 1]); the
            // script-side `Script::set_param` below pushes a
            // `ParamChange` event whose value the chunker delivers
            // to `Params::set_plain` (plain units - matches the CLAP
            // / VST3 wire convention).
            .set_param(P::Gain, 1.0)
            // Schedule a step to -60 dB at sample 4800.
            .script(|s| {
                s.wait_samples(4800);
                s.set_param(P::Gain, -60.0);
            })
            .run();

        // Pre-event tail (samples 4700..4800): the smoother has had
        // an exp(5ms) tail to settle to +6 dB, so output sits near
        // 2.0. With eager apply this region would already be ramping
        // *toward* -60 dB and the mean magnitude would be noticeably
        // below 2.0.
        let pre = &result.output[0][4700..4800];
        #[allow(clippy::cast_precision_loss)]
        let pre_mean = pre.iter().map(|s| s.abs()).sum::<f32>() / pre.len() as f32;
        assert!(
            pre_mean > 1.5,
            "pre-event region should sit near +6 dB (~2.0); got mean={pre_mean}"
        );

        // Post-event tail (last 100 samples): the smoother has had
        // ~100 ms after the event to ramp toward -60 dB; output should
        // be far below the pre-event level.
        let post = &result.output[0][result.output[0].len() - 100..];
        #[allow(clippy::cast_precision_loss)]
        let post_mean = post.iter().map(|s| s.abs()).sum::<f32>() / post.len() as f32;
        assert!(
            post_mean < 0.05,
            "post-event tail should ramp toward -60 dB (~0.001); got mean={post_mean}"
        );
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
        truce_test::screenshot!(Plugin, "screenshots/gain_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/gain_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/gain_default_windows.png")
            .pixel_threshold(2)
            .run();
    }

    /// End-to-end check of [`truce_test::PluginDriver`] on an
    /// effect: feed a block of non-zero input, assert the output is
    /// non-silent and not clipping or `NaNing`. The canonical smoke
    /// test for the driver pipeline.
    #[test]
    fn driver_passthrough() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .sample_rate(44_100.0)
            .channels(2)
            .block_size(256)
            .duration(Duration::from_secs(1))
            .input(InputSource::Constant(0.5))
            .run();

        assertions::assert_no_nans(&result);
        assertions::assert_nonzero(&result);
        assertions::assert_peak_below(&result, 1.0);
    }
}
