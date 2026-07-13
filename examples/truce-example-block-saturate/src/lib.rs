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

/// Descriptor; the SIMD scratch lives in [`SaturateDsp`], sized in `reset`.
pub struct Saturate;

/// Per-instance SIMD scratch, sized to the host's max block in `reset`.
/// `sx` holds the driven input, `sy` holds `tanh(sx)`; the three-stage
/// chain (drive -> tanh -> output) can't alias the audio output with the
/// tanh input, so a distinct scratch pair is the cleanest shape.
#[derive(Default)]
pub struct SaturateDsp {
    sx: Vec<f32>,
    sy: Vec<f32>,
    drive_db: Vec<f32>,
    output_db: Vec<f32>,
    drive_lin_buf: Vec<f32>,
    output_lin_buf: Vec<f32>,
}

impl PluginLogic for Saturate {
    type Params = SaturateParams;
    type DspState = SaturateDsp;

    fn reset(state: &mut SaturateDsp, _params: &SaturateParams, config: &AudioConfig) {
        for buf in [
            &mut state.sx,
            &mut state.sy,
            &mut state.drive_db,
            &mut state.output_db,
            &mut state.drive_lin_buf,
            &mut state.output_lin_buf,
        ] {
            buf.clear();
            buf.resize(config.max_block_size, 0.0);
        }
    }

    fn process(
        state: &mut SaturateDsp,
        params: &SaturateParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        if !params.drive.is_smoothing() && !params.output.is_smoothing() {
            // Fast path: constant drive + output gains.
            let drive_lin = db_to_linear(params.drive.value());
            let output_lin = db_to_linear(params.output.value());

            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                let n = inp.len();
                let sx = &mut state.sx[..n];
                let sy = &mut state.sy[..n];
                ops::scale_block(sx, inp, drive_lin); // sx = inp * drive
                math::tanh_block(sy, sx); // sy = tanh(sx)
                ops::scale_block(out, sy, output_lin); // out = sy * output
            }
        } else {
            // Slow path: per-sample drive + output envelopes.
            // `read_into` advances each smoother by exactly `n`, so the
            // value doesn't step at the next block edge.
            let n = buffer.num_samples();
            params.drive.read_into(&mut state.drive_db[..n]);
            params.output.read_into(&mut state.output_db[..n]);
            math::db_to_linear_block(&mut state.drive_lin_buf[..n], &state.drive_db[..n]);
            math::db_to_linear_block(&mut state.output_lin_buf[..n], &state.output_db[..n]);

            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                let nn = inp.len();
                let drive_lin = &state.drive_lin_buf[..nn];
                let output_lin = &state.output_lin_buf[..nn];
                let sx = &mut state.sx[..nn];
                let sy = &mut state.sy[..nn];
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
    fn large_block_processes_full_buffer() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};
        // A block larger than the former hard-coded 1024 scratch. The whole
        // block must be written, not truncated at 1024 samples.
        let result = driver!(Plugin)
            .block_size(2048)
            .duration(Duration::from_millis(100))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_no_nans(&result);
        let ch0 = &result.output[0];
        assert!(ch0.len() >= 2048);
        assert!(
            ch0[1024..2048].iter().all(|&s| s != 0.0),
            "output past 1024 samples was not processed"
        );
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
