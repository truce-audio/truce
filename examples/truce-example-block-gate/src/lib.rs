//! Noise gate: pass audio through when the block peak is above
//! threshold, zero the output when below.
//!
//! Per-block detect/apply (not per-sample), so this gate is hard
//! and has zero ramp. That keeps the example focused on the two
//! ops it exists to demo:
//!
//! - `abs_max_block` for the per-channel peak detection in the
//!   detect stage.
//! - `zero_block` for the fast silence path in the apply stage.
//!
//! For a production gate you'd also want attack/release smoothing
//! (a per-sample envelope between 0 and 1, applied via
//! `mul_block`); that lives in `truce-example-block-gain`'s envelope
//! shape. Stripping it here keeps the diff tight.

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, meter, widgets};
use truce_simd::ops;

use GateParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct GateParams {
    #[param(
        name = "Threshold",
        range = "linear(-80, 0)",
        default = -40.0,
        unit = "dB",
        smooth = "exp(20)"
    )]
    pub threshold: FloatParam,

    #[meter]
    pub meter_left: MeterSlot,

    #[meter]
    pub meter_right: MeterSlot,
}

pub struct Gate {
    params: Arc<GateParams>,
}

impl Gate {
    pub fn new(params: Arc<GateParams>) -> Self {
        Self { params }
    }
}

impl PluginLogic for Gate {
    type Params = GateParams;

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
        // `read_after(n)` advances the smoother by the whole block
        // in one atomic pair, so the threshold's `exp(20)` smoothing
        // settles in ~20 ms wall-clock instead of ~20 blocks (which
        // a per-block `.read()` would silently downsample to).
        let threshold_lin = db_to_linear(self.params.threshold.read_after(buffer.num_samples()));
        let nch = buffer.channels();

        // Detect: peak over every input channel. The gate opens
        // if ANY channel exceeds threshold (so stereo signals
        // with content on only one side don't get spuriously
        // silenced).
        let mut peak = 0.0_f32;
        for ch in 0..nch {
            let inp = buffer.input(ch);
            let ch_peak = ops::abs_max_block(inp);
            if ch_peak > peak {
                peak = ch_peak;
            }
        }

        if peak < threshold_lin {
            // Gate closed: zero the output. One SIMD store-zero
            // pass per channel; no copy from input needed.
            for ch in 0..nch {
                let (_inp, out) = buffer.io(ch);
                ops::zero_block(out);
            }
        } else {
            // Gate open: pass input through unchanged.
            for ch in 0..nch {
                let (inp, out) = buffer.io(ch);
                ops::copy_block(out, inp);
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

    fn editor(params: Arc<GateParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::Threshold, "Thresh").at(0, 0),
            meter(&[P::MeterLeft, P::MeterRight], "Level").at(1, 0),
        ])])
        .with_title("GATE")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: Gate,
    params: GateParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn renders_nonzero_output_when_above_threshold() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};
        let result = driver!(Plugin)
            .duration(Duration::from_millis(12))
            .input(InputSource::Constant(0.3))
            .run();
        // 0.3 amplitude is well above -40 dB → gate open → pass.
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
    fn no_duplicate_param_ids() {
        truce_test::assert_no_duplicate_param_ids::<Plugin>();
    }

    /// Silent input → silent output regardless of threshold.
    /// Exercises the `zero_block` path end-to-end.
    #[test]
    #[allow(clippy::float_cmp)]
    fn silence_passes_to_silence() {
        use std::time::Duration;
        use truce_test::{InputSource, driver};
        let result = driver!(Plugin)
            .duration(Duration::from_millis(50))
            .input(InputSource::Constant(0.0))
            .run();
        for ch in &result.output {
            for &s in ch {
                assert_eq!(s, 0.0);
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/block_gate_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/block_gate_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/block_gate_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
