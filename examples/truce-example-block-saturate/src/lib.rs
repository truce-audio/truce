//! Series saturator: drive → tanh soft-clip → make-up gain.
//!
//! Exercises `truce_simd::math::tanh_block` in series with two
//! gain stages. Demonstrates how to compose `ops::*` and `math::*`
//! through scratch buffers when the chain has more than one stage.
//!
//! Fast path (smoothers converged): scalar drive / output linear
//! constants, two `gain_block` calls around a single `tanh_block`.
//!
//! Slow path (smoothing in progress): vectorize the dB → linear
//! envelope via `math::db_to_linear_block`, then apply the chain
//! per channel via `mul_block` (per-sample envelope) instead of
//! `gain_block` (scalar).

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, meter, widgets};
use truce_simd::{math, ops};

use SaturateParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct SaturateParams {
    #[param(
        name = "Drive",
        range = "linear(0, 36)",
        default = 6.0,
        unit = "dB",
        smooth = "exp(10)"
    )]
    pub drive: FloatParam,

    #[param(
        name = "Output",
        range = "linear(-24, 6)",
        unit = "dB",
        smooth = "exp(10)"
    )]
    pub output: FloatParam,

    #[meter]
    pub meter_left: MeterSlot,

    #[meter]
    pub meter_right: MeterSlot,
}

/// Stateless descriptor - saturation carries no DSP state, only params.
pub struct Saturate;

const MAX_BLOCK: usize = 1024;

impl PurePluginLogic for Saturate {
    type Params = SaturateParams;

    fn process(
        params: &SaturateParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Two scratch buffers: sx holds the driven input, sy holds
        // tanh(sx). Three-stage chain (drive → tanh → output) can't
        // alias the audio output buffer with the tanh input, so a
        // pair of stack scratches is the cleanest shape.
        let mut sx = [0.0_f32; MAX_BLOCK];
        let mut sy = [0.0_f32; MAX_BLOCK];

        if !params.drive.is_smoothing() && !params.output.is_smoothing() {
            // Fast path: constant drive + output gains.
            let drive_lin = db_to_linear(params.drive.value());
            let output_lin = db_to_linear(params.output.value());

            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                let n = inp.len().min(MAX_BLOCK);
                let inp = &inp[..n];
                let sx = &mut sx[..n];
                let sy = &mut sy[..n];
                let out = &mut out[..n];
                ops::scale_block(sx, inp, drive_lin); // sx = inp * drive
                math::tanh_block(sy, sx); // sy = tanh(sx)
                ops::scale_block(out, sy, output_lin); // out = sy * output
            }
        } else {
            // Slow path: per-sample drive + output envelopes.
            // `read_into` advances each smoother by exactly `n`, so the
            // value doesn't step at the next block edge.
            let n = buffer.num_samples().min(MAX_BLOCK);
            let mut drive_db = [0.0_f32; MAX_BLOCK];
            let mut output_db = [0.0_f32; MAX_BLOCK];
            params.drive.read_into(&mut drive_db[..n]);
            params.output.read_into(&mut output_db[..n]);
            let mut drive_lin_buf = [0.0_f32; MAX_BLOCK];
            let mut output_lin_buf = [0.0_f32; MAX_BLOCK];
            math::db_to_linear_block(&mut drive_lin_buf[..n], &drive_db[..n]);
            math::db_to_linear_block(&mut output_lin_buf[..n], &output_db[..n]);

            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                let nn = inp.len().min(n);
                let inp = &inp[..nn];
                let drive_lin = &drive_lin_buf[..nn];
                let output_lin = &output_lin_buf[..nn];
                let sx = &mut sx[..nn];
                let sy = &mut sy[..nn];
                let out = &mut out[..nn];
                ops::mul_block(sx, inp, drive_lin); // sx = inp * drive
                math::tanh_block(sy, sx); // sy = tanh(sx)
                ops::mul_block(out, sy, output_lin); // out = sy * output
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

    fn editor(params: Arc<SaturateParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::Drive, "Drive").at(0, 0),
            knob(P::Output, "Output").at(0, 1),
            meter(&[P::MeterLeft, P::MeterRight], "Level")
                .at(1, 0)
                .rows(2),
        ])])
        .with_title("SATURATE")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: Saturate,
    params: SaturateParams,
}

truce::enable_rt_paranoid!();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_is_allocation_free() {
        use std::time::Duration;
        use truce_test::{InputSource, assert_no_audio_alloc, driver};
        assert_no_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.25))
                .script(|s| {
                    s.set_param(P::Drive, 0.9);
                    s.wait_ms(15);
                    s.set_param(P::Drive, 0.1);
                    s.wait_ms(15);
                })
                .run()
        });
    }

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
            .input(InputSource::Constant(0.3))
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

    /// Drive a 0.5 DC signal: with default drive (+6 dB) and
    /// output (0 dB), the tanh stage saturates well below ±1, so
    /// the output should land somewhere in (0.5, 1.0) and never
    /// clip. Smoke test for the chain order.
    #[test]
    fn output_stays_in_range() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};
        let result = driver!(Plugin)
            .duration(Duration::from_millis(50))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_peak_below(&result, 1.0);
        assertions::assert_nonzero(&result);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/block_saturate_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/block_saturate_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/block_saturate_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
