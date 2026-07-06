//! Parallel-saturation dry/wet plugin.
//!
//! Generates a "wet" signal by running the input through a
//! `tanh_block` saturator (after a fixed-ish drive boost), then
//! blends dry + wet with `mix_block` at the user-controlled ratio.
//!
//! The point of this example is `mix_block`: dry/wet cross-fade is
//! its canonical use case, and no other example in the tree calls
//! it. `tanh_block` shows up here too as the cheapest "interesting"
//! signal-shape transformation to generate a wet that's audibly
//! different from the dry.
//!
//! `mix_block`'s gain coefficients are *scalar per call*, so a
//! per-sample mix envelope doesn't apply directly. We read the
//! smoothed `mix` value once per audio block
//! (zero-order hold within the block; the smoother handles
//! block-to-block transitions). For audio mixing this is
//! indistinguishable from per-sample interpolation at typical
//! smoothing times.

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, meter, widgets};
use truce_simd::{math, ops};

use DryWetParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct DryWetParams {
    #[param(
        name = "Drive",
        range = "linear(0, 24)",
        default = 6.0,
        unit = "dB",
        smooth = "exp(10)"
    )]
    pub drive: FloatParam,

    #[param(
        name = "Mix",
        range = "linear(0, 1)",
        default = 0.5,
        unit = "%",
        smooth = "exp(20)"
    )]
    pub mix: FloatParam,

    #[meter]
    pub meter_left: MeterSlot,

    #[meter]
    pub meter_right: MeterSlot,
}

pub struct DryWet {
    params: Arc<DryWetParams>,
}

impl DryWet {
    pub fn new(params: Arc<DryWetParams>) -> Self {
        Self { params }
    }
}

const MAX_BLOCK: usize = 1024;

impl PluginLogic for DryWet {
    type Params = DryWetParams;

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
        // Wet scratch holds the saturated signal. `tanh_block`
        // wants distinct in/out so a stack scratch is the natural
        // shape; mix_block then folds wet and dry into out in one
        // SIMD pass.
        let mut wet = [0.0_f32; MAX_BLOCK];
        let mut driven = [0.0_f32; MAX_BLOCK];

        // Both mix and drive are applied block-constant, so
        // `read_after(n)` advances each smoother by the whole block
        // in one atomic pair - matching the per-block consumption
        // pattern instead of silently downsampling smoother
        // convergence to once-per-block.
        let n = buffer.num_samples();
        let mix = self.params.mix.read_after(n);
        let dry_g = 1.0 - mix;
        let wet_g = mix;

        // Drive treated as block-constant. For an example demoing
        // mix_block, the dB → linear conversion is a one-line
        // call; envelope precompute is over-engineering for two
        // gain stages.
        let drive_lin = db_to_linear(self.params.drive.read_after(n));

        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            let n = inp.len().min(MAX_BLOCK);
            let inp = &inp[..n];
            let driven = &mut driven[..n];
            let wet = &mut wet[..n];
            let out = &mut out[..n];
            ops::scale_block(driven, inp, drive_lin); // driven = inp * drive
            math::tanh_block(wet, driven); // wet = tanh(driven)
            ops::mix_block(out, inp, dry_g, wet, wet_g); // out = inp * dry_g + wet * wet_g
        }

        if buffer.num_output_channels() >= 1 {
            context.set_meter(P::MeterLeft, buffer.output_peak(0));
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MeterRight, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<DryWetParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::Drive, "Drive").at(0, 0),
            knob(P::Mix, "Mix").at(0, 1),
            meter(&[P::MeterLeft, P::MeterRight], "Level")
                .at(1, 0)
                .rows(2),
        ])])
        .with_title("DRY/WET")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: DryWet,
    params: DryWetParams,
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

    #[test]
    fn output_within_bounds() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};
        let result = driver!(Plugin)
            .duration(Duration::from_millis(50))
            .input(InputSource::Constant(0.4))
            .run();
        assertions::assert_no_nans(&result);
        // dry/wet sum can lift signal above input briefly during
        // smoother transitions; keep ceiling at 1.0 (tanh-bounded).
        assertions::assert_peak_below(&result, 1.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/block_drywet_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/block_drywet_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/block_drywet_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
