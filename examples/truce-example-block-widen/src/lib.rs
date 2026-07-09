//! Stereo widener via mid-side recombination.
//!
//! Decomposes the input into mid = `(L+R)/2` and side = `(L-R)/2`,
//! then recombines as `L_out = mid + side * width` and
//! `R_out = mid - side * width`. `width = 1.0` recovers the input
//! exactly; `width = 0.0` collapses to mono; `width > 1.0`
//! exaggerates the stereo image.
//!
//! The recombination is the textbook use of [`ops::mac_block`]:
//! lay down the mid into the output (`copy_block`), then add a
//! scaled side (`mac_block`). No other example in the tree uses
//! `mac_block`; this is the cleanest demo of "additive blend with
//! a scalar coefficient".

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, meter, widgets};
use truce_simd::ops;

use WidenParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct WidenParams {
    #[param(
        name = "Width",
        range = "linear(0, 2)",
        default = 1.0,
        smooth = "exp(10)"
    )]
    pub width: FloatParam,

    #[meter]
    pub meter_left: MeterSlot,

    #[meter]
    pub meter_right: MeterSlot,
}

/// Stateless descriptor - the widener carries no DSP state, only params.
pub struct Widen;

const MAX_BLOCK: usize = 1024;

impl PluginLogic for Widen {
    type Params = WidenParams;
    type DspState = ();

    fn init(_params: &WidenParams) {}

    fn process(
        _state: &mut (),
        params: &WidenParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Mono input degrades to passthrough; the side channel is
        // identically zero, so no widening is possible.
        if buffer.channels() < 2 {
            let (inp, out) = buffer.io(0);
            ops::copy_block(out, inp);
            if buffer.num_output_channels() >= 1 {
                context.set_meter(P::MeterLeft, buffer.output_peak(0));
            }
            return ProcessStatus::Normal;
        }

        let n = buffer.num_samples().min(MAX_BLOCK);
        // Width is applied block-constant; `read_after(n)` advances
        // the smoother by the whole block so the wall-clock
        // convergence time matches the smoother declaration.
        let width = params.width.read_after(n);

        // Build mid + side from input. Scalar loop autovectorizes
        // (LLVM packs the four ops per iteration into NEON / AVX).
        let mut mid = [0.0_f32; MAX_BLOCK];
        let mut side = [0.0_f32; MAX_BLOCK];
        {
            let in_l = buffer.input(0);
            let in_r = buffer.input(1);
            for i in 0..n {
                mid[i] = 0.5 * (in_l[i] + in_r[i]);
                side[i] = 0.5 * (in_l[i] - in_r[i]);
            }
        }

        // Recombine: L = mid + side * width, R = mid - side * width.
        // The mac_block pair is what this example exists to show.
        {
            let (_, out_l) = buffer.io(0);
            ops::copy_block(&mut out_l[..n], &mid[..n]);
            ops::mac_block(&mut out_l[..n], &side[..n], width);
        }
        {
            let (_, out_r) = buffer.io(1);
            ops::copy_block(&mut out_r[..n], &mid[..n]);
            ops::mac_block(&mut out_r[..n], &side[..n], -width);
        }

        if buffer.num_output_channels() >= 1 {
            context.set_meter(P::MeterLeft, buffer.output_peak(0));
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MeterRight, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<WidenParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::Width, "Width").at(0, 0),
            meter(&[P::MeterLeft, P::MeterRight], "Level").at(1, 0),
        ])])
        .with_title("WIDEN")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: Widen,
    params: WidenParams,
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
                    s.set_param(P::Width, 0.9);
                    s.wait_ms(15);
                    s.set_param(P::Width, 0.1);
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

    /// At default width = 1.0, recombination should be the
    /// identity: `out_l` = mid + side = `(l+r)/2 + (l-r)/2` = l,
    /// and similarly `out_r` = r. Smoke test for the mid-side
    /// math.
    #[test]
    fn unity_at_default_width() {
        use std::time::Duration;
        use truce_test::{InputSource, driver};
        let result = driver!(Plugin)
            .duration(Duration::from_millis(50))
            .input(InputSource::Constant(0.4))
            .run();
        // Constant-DC input on both channels: out should match.
        let max_l = result.output[0]
            .iter()
            .map(|s| s.abs())
            .fold(0.0_f32, f32::max);
        assert!((max_l - 0.4).abs() < 0.01, "L expected ~0.4, got {max_l}");
        if result.output.len() >= 2 {
            let max_r = result.output[1]
                .iter()
                .map(|s| s.abs())
                .fold(0.0_f32, f32::max);
            assert!((max_r - 0.4).abs() < 0.01, "R expected ~0.4, got {max_r}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/block_widen_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/block_widen_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/block_widen_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
