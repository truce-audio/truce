//! Stereo plate reverb wired through a `fundsp` audio graph.
//!
//! ```text
//!     in (L,R) ──► high-pass (low cut) ──► low-pass (high cut) ──► reverb_stereo ──┐
//!                                                                                  │
//!     in (L,R) ──────────────────────────────────────────────────────────────► dry ┤──► out
//! ```
//!
//! See `README.md` for the integration patterns + gotchas.

use fundsp::prelude::{
    AudioUnit, Shared, U2, dc, highpass, lowpass, multipass, pass, reverb_stereo, shared, var,
};
use std::sync::Arc;
use truce::prelude::*;
use truce_gui::layout::{GridLayout, knob, meter, widgets};

use FundspReverbParamsParamId as P;

const DEFAULT_LOW_CUT_HZ: f32 = 120.0;
const DEFAULT_HIGH_CUT_HZ: f32 = 8000.0;
const DEFAULT_REVERB_MIX: f32 = 0.25;
const FILTER_Q: f32 = 0.707;
const ROOM_SIZE: f64 = 10.0;
const DAMPING: f64 = 0.5;
/// fundsp's `reverb_stereo` bakes its decay time into the FDN at
/// construction — there's no `Shared` cell for it, so live tweaks
/// would force a graph rebuild that resets the tail. We use a
/// fixed, effectively-infinite tail instead: 60 s is well past the
/// perceptual decay floor for any musical material.
const REVERB_TIME_S: f64 = 60.0;

#[derive(Params)]
pub struct FundspReverbParams {
    #[param(
        name = "Low Cut",
        range = "log(20, 2000)",
        unit = "Hz",
        default = 120.0,
        smooth = "exp(5)"
    )]
    pub low_cut: FloatParam,

    #[param(
        name = "High Cut",
        range = "log(500, 18000)",
        unit = "Hz",
        default = 8000.0,
        smooth = "exp(5)"
    )]
    pub high_cut: FloatParam,

    #[param(
        name = "Mix",
        range = "linear(0, 1)",
        default = 0.25,
        smooth = "exp(20)"
    )]
    pub mix: FloatParam,

    #[meter]
    pub meter_l: MeterSlot,

    #[meter]
    pub meter_r: MeterSlot,
}

pub struct FundspReverb {
    params: Arc<FundspReverbParams>,
    // Atomic cells the fundsp graph reads via `var()` each sample.
    low_cut_shared: Shared,
    high_cut_shared: Shared,
    mix_shared: Shared,
    graph: Box<dyn AudioUnit>,
}

impl FundspReverb {
    pub fn new(params: Arc<FundspReverbParams>) -> Self {
        Self {
            params,
            low_cut_shared: shared(DEFAULT_LOW_CUT_HZ),
            high_cut_shared: shared(DEFAULT_HIGH_CUT_HZ),
            mix_shared: shared(DEFAULT_REVERB_MIX),
            graph: Box::new(multipass::<U2>()),
        }
    }

    /// Rebuild the graph for the given sample rate. Allocates inside
    /// fundsp's `allocate()`; only called from `reset()`, off the
    /// audio thread.
    fn rebuild_graph(&mut self, sample_rate: f64) {
        // fundsp's SVF filters take 3 inputs in positional order:
        // (signal, cutoff, Q). Every input is `f32` — the type
        // system can't tell the order; stack mismatch is a silent
        // numerical-blowup bug.
        let hp_l = (pass() | var(&self.low_cut_shared) | dc(FILTER_Q)) >> highpass::<f32>();
        let hp_r = (pass() | var(&self.low_cut_shared) | dc(FILTER_Q)) >> highpass::<f32>();
        let lp_l = (pass() | var(&self.high_cut_shared) | dc(FILTER_Q)) >> lowpass::<f32>();
        let lp_r = (pass() | var(&self.high_cut_shared) | dc(FILTER_Q)) >> lowpass::<f32>();

        let filters_stereo = (hp_l | hp_r) >> (lp_l | lp_r);
        let wet = filters_stereo >> reverb_stereo(ROOM_SIZE, REVERB_TIME_S, DAMPING);
        let dry = multipass::<U2>();

        // `var(&mix)` is 1-channel; fundsp's `*` requires matching
        // output counts on both sides. Broadcast to stereo manually
        // by stacking two reads of the same `Shared`.
        let mix_stereo = || var(&self.mix_shared) | var(&self.mix_shared);
        let inv_mix_stereo =
            || (dc(1.0) - var(&self.mix_shared)) | (dc(1.0) - var(&self.mix_shared));

        // `&` (Bus): run dry and wet in parallel feeding off the same
        // input, sum their outputs.
        let mut graph: Box<dyn AudioUnit> =
            Box::new((dry * inv_mix_stereo()) & (wet * mix_stereo()));
        graph.set_sample_rate(sample_rate);
        graph.allocate();
        self.graph = graph;
    }
}

impl PluginLogic for FundspReverb {
    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
        self.rebuild_graph(sample_rate);
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // `for_each_frame::<2, _>` transposes the buffer to stereo
        // frames so fundsp's `tick(in, out)` can be called directly.
        // Per-sample smoother read + Shared write inside the closure
        // gives sample-accurate automation.
        buffer.for_each_frame::<2, _>(|frame_in, frame_out| {
            self.low_cut_shared.set_value(self.params.low_cut.read());
            self.high_cut_shared.set_value(self.params.high_cut.read());
            self.mix_shared.set_value(self.params.mix.read());
            self.graph.tick(frame_in, frame_out);
        });

        if buffer.num_output_channels() >= 1 {
            context.set_meter(P::MeterL, buffer.output_peak(0));
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MeterR, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn layout(&self) -> GridLayout {
        GridLayout::build(vec![widgets(vec![
            knob(P::LowCut, "Low Cut"),
            knob(P::HighCut, "High Cut"),
            knob(P::Mix, "Mix"),
            meter(&[P::MeterL, P::MeterR], "Level").at(3, 0).rows(3),
        ])])
        .with_title("FUNDSP REVERB")
    }
}

truce::plugin! {
    logic: FundspReverb,
    params: FundspReverbParams,
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
            .duration(Duration::from_millis(50))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_nonzero(&result);
    }

    /// Reverb tail survives a brief constant input without NaN or
    /// runaway peaks.
    #[test]
    fn reverb_tail_stays_finite() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .duration(Duration::from_millis(500))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_peak_below(&result, 2.0);
    }

    /// Regression: SVF filter input order is positional + unchecked.
    /// Stacking `(cutoff | Q | signal)` instead of `(signal | cutoff
    /// | Q)` compiles fine and feeds the filter cutoff as audio;
    /// downstream reverb FDN amplifies it past peak ~7000 within a
    /// second. 2 s of constant input exposes the runaway.
    #[test]
    fn extended_steady_state_stays_bounded() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .duration(Duration::from_secs(2))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_peak_below(&result, 2.0);
    }

    /// Regression: param → `Shared` sync. Ramps `low_cut` from 0.0 to
    /// 1.0 in 10 steps over 500 ms; asserts no NaN, bounded peak,
    /// and that the wet path stays non-silent (catches "filter froze
    /// at default cutoff because the sync write was dropped").
    #[test]
    fn cutoff_automation_stays_finite() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .duration(Duration::from_millis(750))
            .input(InputSource::Constant(0.3))
            .script(|s| {
                for step in 1..=10 {
                    let normalized = f64::from(step) / 10.0;
                    s.set_param(P::LowCut, normalized);
                    s.wait_ms(50);
                }
            })
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_peak_below(&result, 2.0);
        assertions::assert_nonzero_after(&result, Duration::from_millis(500));
    }

    /// Regression: sample-rate propagation. SVF coefficients are
    /// SR-dependent — if `set_sample_rate` misses a sub-node the
    /// filter de-tunes or blows up at non-default rates.
    #[test]
    fn stability_at_96k() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .sample_rate(96_000.0)
            .duration(Duration::from_secs(1))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_peak_below(&result, 2.0);
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
    fn editor_lifecycle() {
        truce_test::assert_editor_lifecycle::<Plugin>();
    }

    #[test]
    fn bus_config_effect() {
        truce_test::assert_bus_config_effect::<Plugin>();
    }

    #[test]
    fn param_count_matches() {
        truce_test::assert_param_count_matches::<Plugin>();
    }

    #[test]
    fn corrupt_state_no_crash() {
        truce_test::assert_corrupt_state_no_crash::<Plugin>();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/fundsp_reverb_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/fundsp_reverb_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/fundsp_reverb_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
