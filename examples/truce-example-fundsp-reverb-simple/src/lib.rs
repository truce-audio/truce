//! Stereo plate reverb wired through a `fundsp` audio graph.
//!
//! **Pedagogical variant.** This crate rebuilds the fundsp graph
//! *inline on the audio thread* whenever the Time parameter crosses
//! the hysteresis threshold. That's a real-time-safety violation
//! (`Box::new` + `graph.allocate()` can block on the system allocator)
//! and is shown here only because it makes the integration shape
//! obvious in one file. The production pattern rebuilds the graph
//! on a dedicated worker thread and swaps it in via a lock-free
//! queue.
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
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, meter, widgets};

use FundspReverbSimpleParamsParamId as P;

const DEFAULT_LOW_CUT_HZ: f32 = 120.0;
const DEFAULT_HIGH_CUT_HZ: f32 = 8000.0;
const DEFAULT_REVERB_MIX: f32 = 0.25;
const DEFAULT_TIME_S: f32 = 3.0;
const FILTER_Q: f32 = 0.707;
const ROOM_SIZE: f64 = 10.0;
const DAMPING: f64 = 0.5;

/// Hysteresis on Time changes - fundsp's `reverb_stereo` bakes RT60
/// into the FDN at construction, so each crossing triggers a full
/// rebuild (= delay lines reset, tail dropped). Threshold keeps tiny
/// drifts from rebuilding.
const TIME_REBUILD_THRESHOLD_S: f32 = 0.05;

#[derive(Params)]
pub struct FundspReverbSimpleParams {
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

    // Unsmoothed: Time changes rebuild the FDN once on the raw
    // target; the smoother would have no reader.
    #[param(
        name = "Time",
        range = "log(0.1, 20)",
        unit = "s",
        default = 3.0,
        smooth = "none"
    )]
    pub time: FloatParam,

    #[param(
        name = "Mix",
        range = "linear(0, 1)",
        default = 0.25,
        unit = "%",
        smooth = "exp(20)"
    )]
    pub mix: FloatParam,

    #[meter]
    pub meter_l: MeterSlot,

    #[meter]
    pub meter_r: MeterSlot,
}

/// Stateless descriptor - DSP state lives in [`FundspReverbSimpleDspState`].
pub struct FundspReverbSimple;

/// Per-instance DSP state: the fundsp graph, the atomic cells it
/// reads, and the inputs the current graph was built with.
pub struct FundspReverbSimpleDspState {
    // Atomic cells the fundsp graph reads each sample via `var()`.
    low_cut_shared: Shared,
    high_cut_shared: Shared,
    mix_shared: Shared,
    graph: Box<dyn AudioUnit>,
    // Inputs the current graph was built with. Cached so unchanged
    // values don't trigger a rebuild.
    last_built_sr: f64,
    last_built_time_s: f32,
}

impl Default for FundspReverbSimpleDspState {
    fn default() -> Self {
        Self {
            low_cut_shared: shared(DEFAULT_LOW_CUT_HZ),
            high_cut_shared: shared(DEFAULT_HIGH_CUT_HZ),
            mix_shared: shared(DEFAULT_REVERB_MIX),
            graph: Box::new(multipass::<U2>()),
            last_built_sr: 0.0,
            last_built_time_s: DEFAULT_TIME_S,
        }
    }
}

impl FundspReverbSimpleDspState {
    /// Allocates via `Box::new` + `allocate()`. **Called from
    /// `process()` on the audio thread when Time changes** - that's
    /// the simplification this variant exists to highlight. See the
    /// `truce-example-fundsp-reverb-worker` crate for the
    /// worker-thread + lock-free swap pattern that keeps `process()`
    /// alloc-free.
    fn rebuild_graph(&mut self, sample_rate: f64, time_s: f32) {
        // fundsp SVFs take inputs positionally as `(signal, cutoff, Q)`;
        // every input is `f32` so the type system can't catch a swap.
        let hp_l = (pass() | var(&self.low_cut_shared) | dc(FILTER_Q)) >> highpass::<f32>();
        let hp_r = (pass() | var(&self.low_cut_shared) | dc(FILTER_Q)) >> highpass::<f32>();
        let lp_l = (pass() | var(&self.high_cut_shared) | dc(FILTER_Q)) >> lowpass::<f32>();
        let lp_r = (pass() | var(&self.high_cut_shared) | dc(FILTER_Q)) >> lowpass::<f32>();

        let filters_stereo = (hp_l | hp_r) >> (lp_l | lp_r);
        let wet = filters_stereo >> reverb_stereo(ROOM_SIZE, f64::from(time_s), DAMPING);
        let dry = multipass::<U2>();

        // `var()` is mono; broadcast to stereo by stacking two reads
        // so `*` matches the (stereo) wet/dry counts.
        let mix_stereo = || var(&self.mix_shared) | var(&self.mix_shared);
        let inv_mix_stereo =
            || (dc(1.0) - var(&self.mix_shared)) | (dc(1.0) - var(&self.mix_shared));

        // `&` is Bus: dry + wet share the input and sum their outputs.
        let mut graph: Box<dyn AudioUnit> =
            Box::new((dry * inv_mix_stereo()) & (wet * mix_stereo()));
        graph.set_sample_rate(sample_rate);
        graph.allocate();
        self.graph = graph;
    }
}

impl PluginLogic for FundspReverbSimple {
    type Params = FundspReverbSimpleParams;
    type DspState = FundspReverbSimpleDspState;

    fn reset(
        state: &mut FundspReverbSimpleDspState,
        params: &FundspReverbSimpleParams,
        config: &AudioConfig,
    ) {
        let sample_rate = config.sample_rate;
        let time_s = params.time.value();
        // SR is a discrete host setting, not a measurement.
        let sr_changed = sample_rate.to_bits() != state.last_built_sr.to_bits();
        let time_changed = (time_s - state.last_built_time_s).abs() > TIME_REBUILD_THRESHOLD_S;
        if sr_changed || time_changed {
            state.rebuild_graph(sample_rate, time_s);
            state.last_built_sr = sample_rate;
            state.last_built_time_s = time_s;
        }
    }

    fn process(
        state: &mut FundspReverbSimpleDspState,
        params: &FundspReverbSimpleParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Read the raw target, not `read()`: a smoothed value would
        // crawl across the threshold for ~200 ms and rebuild every
        // block - audible as an unstable tail until it settles.
        let time_s = params.time.value();
        if (time_s - state.last_built_time_s).abs() > TIME_REBUILD_THRESHOLD_S {
            // This is the rt-safety violation called out at the top
            // of the file: the rebuild path allocates on the audio
            // thread. Acceptable for a teaching example; not for
            // shipping.
            state.rebuild_graph(state.last_built_sr, time_s);
            state.last_built_time_s = time_s;
        }

        // `for_each_frame::<2>` transposes channel-major to stereo
        // frames so fundsp's `tick(in, out)` fits the closure.
        buffer.for_each_frame::<2, _>(|frame_in, frame_out| {
            state.low_cut_shared.set_value(params.low_cut.read());
            state.high_cut_shared.set_value(params.high_cut.read());
            state.mix_shared.set_value(params.mix.read());
            state.graph.tick(frame_in, frame_out);
        });

        if buffer.num_output_channels() >= 1 {
            context.set_meter(P::MeterL, buffer.output_peak(0));
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MeterR, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<FundspReverbSimpleParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::LowCut, "Low Cut").at(0, 0),
            knob(P::HighCut, "High Cut").at(1, 0),
            knob(P::Time, "Time").at(0, 1),
            knob(P::Mix, "Mix").at(1, 1),
            meter(&[P::MeterL, P::MeterR], "Level").at(2, 0).rows(2),
        ])])
        .with_title("FUNDSP REVERB (SIMPLE)")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: FundspReverbSimple,
    params: FundspReverbSimpleParams,
}

// Install the real-time allocation checker under `--features rt-paranoid`
// (no-op otherwise), so the tests below can gate on audio-thread allocs.
truce::enable_rt_paranoid!();

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

    /// With Time held constant the graph is never rebuilt, so `process`
    /// is allocation-free. Only checks under `--features rt-paranoid`.
    #[test]
    fn constant_time_process_is_alloc_free() {
        use std::time::Duration;
        use truce_test::{InputSource, assert_no_audio_alloc, driver};

        assert_no_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.5))
                .run()
        });
    }

    /// Changing the Time knob rebuilds the fundsp graph *on the audio
    /// thread* (`Box::new` + `allocate()`) - the intentional real-time
    /// violation this teaching example exists to show, and the reason
    /// `truce-example-fundsp-reverb-worker` offloads the rebuild. This
    /// test pins that behavior: under `--features rt-paranoid` the Time
    /// change must trip the allocation checker.
    #[test]
    fn time_change_rebuild_trips_rt_check() {
        use std::time::Duration;
        use truce_test::{InputSource, assert_audio_alloc, driver};

        assert_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.5))
                .script(|sc| {
                    sc.set_param(P::Time, 0.1);
                    sc.wait_ms(15);
                    // A large jump crosses the rebuild hysteresis
                    // threshold, forcing a graph rebuild mid-render.
                    sc.set_param(P::Time, 0.95);
                    sc.wait_ms(15);
                })
                .run()
        });
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
    /// SR-dependent - if `set_sample_rate` misses a sub-node the
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
        truce_test::screenshot!(Plugin, "screenshots/fundsp_reverb_simple_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/fundsp_reverb_simple_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(
            Plugin,
            "screenshots/fundsp_reverb_simple_default_windows.png"
        )
        .pixel_threshold(2)
        .run();
    }
}
