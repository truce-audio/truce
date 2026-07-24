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
//!
//! `Oversample` runs the tanh stage through [`truce_oversample`] to
//! curb aliasing from the nonlinearity. Takes effect on the next
//! `reset` (activate), not mid-stream. See `latency` doc below.

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, dropdown, knob, meter, widgets};
use truce_oversample::Oversampler;
use truce_simd::{math, ops};

use SaturateParamsParamId as P;
use std::sync::Arc;

/// Taps per halfband stage. `(17 - 1) / 2 = 8`, divisible by 2/4/8, so
/// `Oversampler::latency_samples` is exact rather than rounded.
const TAPS_PER_STAGE: usize = 17;

/// Oversampling factor for the tanh stage. `ParamEnum` derives
/// `Clone` / `Copy` / `PartialEq`.
#[derive(ParamEnum)]
pub enum OversampleFactor {
    #[name = "Off"]
    Off,
    #[name = "2x"]
    X2,
    #[name = "4x"]
    X4,
    #[name = "8x"]
    X8,
}

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

    #[param(name = "Oversample", short_name = "OS", default = 0)]
    pub oversample: EnumParam<OversampleFactor>,

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
    /// One chain per channel (independent delay-line state). Empty at
    /// `Off`. Rebuilt in `reset` - factor changes apply next activate,
    /// not mid-stream.
    oversamplers: Vec<Oversampler>,
}

/// Stereo cap (default bus layout).
const MAX_CHANNELS: usize = 2;

/// `sy = tanh(sx)`, oversampled when `oversampler` is set.
fn apply_tanh(oversampler: Option<&mut Oversampler>, sy: &mut [f32], sx: &[f32]) {
    match oversampler {
        Some(os) => {
            sy.copy_from_slice(sx);
            os.process_block(sy, |hi| hi.iter_mut().for_each(|s| *s = s.tanh()));
        }
        None => math::tanh_block(sy, sx),
    }
}

impl PluginLogic for Saturate {
    type Params = SaturateParams;
    type DspState = SaturateDsp;

    fn reset(state: &mut SaturateDsp, params: &SaturateParams, config: &AudioConfig) {
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

        let factor = match params.oversample.value() {
            OversampleFactor::Off => None,
            OversampleFactor::X2 => Some(2),
            OversampleFactor::X4 => Some(4),
            OversampleFactor::X8 => Some(8),
        };
        state.oversamplers = match factor {
            None => Vec::new(),
            Some(factor) => (0..MAX_CHANNELS)
                .map(|_| {
                    let mut os = Oversampler::new(factor, TAPS_PER_STAGE);
                    os.prepare(config.max_block_size);
                    os
                })
                .collect(),
        };
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
                apply_tanh(state.oversamplers.get_mut(ch), sy, sx); // sy = tanh(sx)
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
                apply_tanh(state.oversamplers.get_mut(ch), sy, sx); // sy = tanh(sx)
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

    /// Every channel's chain has identical latency.
    fn latency(state: &SaturateDsp) -> u32 {
        state
            .oversamplers
            .first()
            .map_or(0, Oversampler::latency_samples)
    }

    fn editor(params: Arc<SaturateParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::Drive, "Drive").at(0, 0),
            knob(P::Output, "Output").at(0, 1),
            dropdown(P::Oversample, "Oversample").at(0, 2),
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
    use truce::core::plugin::PluginRuntime;

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

    /// `.setup` runs after the driver's own `reset`, and `Oversample`
    /// only takes effect on `reset` (see [`SaturateDsp::oversamplers`]
    /// doc) - so re-`reset` here to pick up the change before `run`
    /// processes any blocks.
    fn set_oversample(
        plugin: &mut Plugin,
        cx: &truce_test::SetupContext,
        factor: OversampleFactor,
    ) {
        plugin.params().oversample.set_value(factor);
        plugin.reset(&AudioConfig::new(cx.sample_rate, cx.block_size));
    }

    #[test]
    fn oversample_process_is_allocation_free() {
        use std::time::Duration;
        use truce_test::{InputSource, assert_no_audio_alloc, driver};
        assert_no_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.25))
                .setup(|p, cx| set_oversample(p, cx, OversampleFactor::X4))
                .run()
        });
    }

    /// Same shape as `output_stays_in_range`, with 4x oversampling
    /// active - the tanh chain's output bound doesn't depend on where
    /// tanh runs, so this should hold exactly like the non-oversampled
    /// case.
    #[test]
    fn oversampled_output_stays_in_range() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};
        let result = driver!(Plugin)
            .duration(Duration::from_millis(50))
            .input(InputSource::Constant(0.5))
            .setup(|p, cx| set_oversample(p, cx, OversampleFactor::X4))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_peak_below(&result, 1.0);
        assertions::assert_nonzero(&result);
    }

    #[test]
    fn latency_matches_selected_factor() {
        use std::time::Duration;
        use truce_test::driver;
        for (factor, expected_os_factor) in [
            (OversampleFactor::Off, None),
            (OversampleFactor::X2, Some(2)),
            (OversampleFactor::X4, Some(4)),
            (OversampleFactor::X8, Some(8)),
        ] {
            let expected = expected_os_factor
                .map_or(0, |f| Oversampler::new(f, TAPS_PER_STAGE).latency_samples());
            // Post-run `plugin` is the driver's actual instance, so
            // `.latency()` reads the same path a host's PDC query would.
            let result = driver!(Plugin)
                .duration(Duration::from_millis(5))
                .setup(move |p, cx| set_oversample(p, cx, factor))
                .run();
            assert_eq!(
                result.plugin.latency(),
                expected,
                "factor index {}",
                factor.to_index()
            );
        }
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
