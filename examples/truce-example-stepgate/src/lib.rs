//! Nested-array params demonstration: a tempo-synced step gate.
//!
//! The pattern is one `#[nested]` field typed as a fixed-size array
//! (`[StepParams; NUM_STEPS]`), so the same two-param group (level +
//! on/off) is instantiated `NUM_STEPS` times and the DSP + editor just
//! index into it. Under the default hash id scheme each element gets
//! its own hashed base (`step[0]`, `step[1]`, ...), so adding a param to
//! `StepParams` later leaves every existing step's ids - and the host's
//! saved automation - where they were.
//!
//! `derive(Params)` also ships a static id resolver per array field -
//! `GateParams::step_id(i, StepParamsParamId::Level)` - for code that
//! addresses an element's param without a live instance. The editor
//! below reads the live field ids instead (`params.step[i].level.id()`),
//! which is equivalent for an existing `params`.
//!
//! DSP: the host beat position selects the current step; its level (or
//! zero when the step is off) is the gate target, and a one-pole slew
//! ramps the applied gain toward it so step edges don't click. With no
//! host transport the pattern free-runs at 120 BPM.

use std::sync::Arc;

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, section, toggle};

/// Steps in the pattern. The `#[nested]` array length must be an integer
/// literal (the derive needs the count at expansion time), so this const
/// is asserted against it rather than used in the type.
const NUM_STEPS: usize = 8;
const _: () = assert!(NUM_STEPS == 8);

/// Internal LFO tempo when the host reports no transport, so the gate
/// stays audibly active standalone.
const FREE_BPM: f64 = 120.0;

/// Step length as a note value. Maps directly to beats per step
/// (`Quarter == 1.0`, `Sixteenth == 0.25`, ...).
#[derive(ParamEnum)]
pub enum Rate {
    Quarter,
    #[name = "1/8"]
    Eighth,
    #[name = "1/16"]
    Sixteenth,
    #[name = "1/32"]
    ThirtySecond,
}

impl Rate {
    /// Quarter-note beats one step spans.
    fn beats_per_step(self) -> f64 {
        match self {
            Rate::Quarter => 1.0,
            Rate::Eighth => 0.5,
            Rate::Sixteenth => 0.25,
            Rate::ThirtySecond => 0.125,
        }
    }
}

/// One step: a level and an on/off, hash ids local to the group; the
/// parent rebases each array element off them. Level is unsmoothed - the
/// gate's own slew handles ramping, so a plain `read()` returns the
/// target with no per-sample smoother to pump.
#[derive(Params)]
pub struct StepParams {
    #[param(name = "Level", range = "linear(0, 1)", unit = "%", default = 1.0)]
    pub level: FloatParam,

    #[param(name = "On", default = true)]
    pub on: BoolParam,
}

/// The pattern: a fixed-size array of steps, then the global rate and
/// edge slew. All ids are hashes of field names (per element for the
/// array), so neither reordering fields nor growing `StepParams` moves
/// the ids a shipped plugin's saved state depends on.
#[derive(Params)]
pub struct GateParams {
    #[nested]
    pub step: [StepParams; 8],

    #[param(name = "Rate")]
    pub rate: EnumParam<Rate>,

    #[param(name = "Slew", range = "linear(0, 50)", unit = "ms", default = 2.0)]
    pub slew: FloatParam,
}

/// One-pole coefficient for a `slew_ms` edge ramp at `sample_rate`:
/// `y += (target - y) * (1 - coeff)` per sample. Zero means instant.
#[allow(clippy::cast_possible_truncation)] // audio coeff; the f64 math narrows to f32 at the boundary
fn slew_coeff(slew_ms: f32, sample_rate: f64) -> f32 {
    if slew_ms <= 0.0 {
        return 0.0;
    }
    let tau = f64::from(slew_ms) * 0.001;
    (-1.0 / (tau * sample_rate)).exp() as f32
}

/// The step the beat grid is on: `floor(beats / beats_per_step)` wrapped
/// into `0..NUM_STEPS`.
// `rem_euclid` bounds the value to 0..NUM_STEPS before the `usize` cast;
// `NUM_STEPS` (8) is exact in `f64`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn step_index(beats: f64, beats_per_step: f64) -> usize {
    let idx = (beats / beats_per_step)
        .floor()
        .rem_euclid(NUM_STEPS as f64) as usize;
    idx.min(NUM_STEPS - 1)
}

/// The gate's per-block DSP state. `type DspState = Self` folds the
/// stateless descriptor into this struct, so hot-reload keeps the slew
/// ramp and the free-running clock alive across an edit-and-reload.
pub struct StepGate {
    sample_rate: f64,
    /// Free-running beat clock for when the host reports no transport.
    free_beats: f64,
    /// Slewed gate gain carried between blocks.
    gain: f32,
}

impl Default for StepGate {
    fn default() -> Self {
        StepGate {
            sample_rate: 44100.0,
            free_beats: 0.0,
            gain: 1.0,
        }
    }
}

impl PluginLogic for StepGate {
    type Params = GateParams;
    type DspState = Self;

    fn reset(state: &mut Self, _params: &GateParams, config: &AudioConfig) {
        state.sample_rate = config.sample_rate;
        state.free_beats = 0.0;
        state.gain = 1.0;
    }

    fn process(
        state: &mut Self,
        params: &GateParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        let transport = context.transport;
        let host_sync = transport.playing && transport.tempo > 0.0;
        let sr = state.sample_rate;
        let beats_per_step = params.rate.value().beats_per_step();
        let coeff = slew_coeff(params.slew.read(), sr);

        let host_inc = if host_sync {
            transport.tempo / 60.0 / sr
        } else {
            0.0
        };
        let free_inc = FREE_BPM / 60.0 / sr;
        let mut host_beats = transport.position_beats;
        let mut free_beats = state.free_beats;
        let mut gain = state.gain;

        for i in 0..buffer.num_samples() {
            let beats = if host_sync { host_beats } else { free_beats };
            let step = &params.step[step_index(beats, beats_per_step)];
            let target = if step.on.value() {
                step.level.read()
            } else {
                0.0
            };
            gain = target + (gain - target) * coeff;

            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * gain;
            }

            host_beats += host_inc;
            free_beats += free_inc;
        }

        state.free_beats = free_beats;
        state.gain = gain;
        ProcessStatus::Normal
    }

    fn editor(params: Arc<GateParams>) -> Box<dyn Editor> {
        // Static labels, so index a const table rather than formatting.
        const STEP_LABELS: [&str; NUM_STEPS] = ["1", "2", "3", "4", "5", "6", "7", "8"];
        // Level knobs fill the first row, on/off toggles the second,
        // one column per step (the steps section is exactly two full
        // rows, so the RATE section below starts on a fresh row).
        let mut steps = Vec::with_capacity(NUM_STEPS * 2);
        for (step, label) in params.step.iter().zip(STEP_LABELS) {
            steps.push(knob(step.level.id(), label));
        }
        for step in &params.step {
            steps.push(toggle(step.on.id(), ""));
        }
        GridLayout::build(vec![
            section("STEPS", steps),
            section(
                "RATE",
                vec![
                    knob(params.rate.id(), "Rate"),
                    knob(params.slew.id(), "Slew"),
                ],
            ),
        ])
        // One column per step; keep in sync with `NUM_STEPS`.
        .with_cols(8)
        .with_title("STEP GATE")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: StepGate,
    params: GateParams,
}

truce::enable_rt_paranoid!();

#[cfg(test)]
mod tests {
    use super::{GateParams, NUM_STEPS, Plugin, Rate, StepGate, StepParamsParamId, step_index};
    use std::time::Duration;
    use truce_params::Params;
    use truce_test::{InputSource, assertions, driver};

    #[test]
    fn step_ids_are_distinct() {
        let p = GateParams::new();
        // Eight steps of two params each, then rate and slew, every id
        // distinct (construction also asserts no collisions).
        assert_eq!(p.count(), NUM_STEPS * 2 + 2);
        let mut ids: Vec<u32> = p.param_infos().iter().map(|i| i.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), NUM_STEPS * 2 + 2);
    }

    #[test]
    fn id_helper_matches_live_field() {
        let p = GateParams::new();
        for (i, step) in p.step.iter().enumerate() {
            assert_eq!(
                GateParams::step_id(i, StepParamsParamId::Level),
                step.level.id()
            );
            assert_eq!(GateParams::step_id(i, StepParamsParamId::On), step.on.id());
        }
    }

    #[test]
    fn step_index_walks_and_wraps() {
        // Quarter-note steps: one step per beat, wrapping at NUM_STEPS.
        assert_eq!(step_index(0.0, 1.0), 0);
        assert_eq!(step_index(3.5, 1.0), 3);
        assert_eq!(step_index(8.0, 1.0), 0);
        assert_eq!(step_index(9.25, 1.0), 1);
        // Sixteenth steps: four per beat.
        assert_eq!(step_index(0.25, 0.25), 1);
        assert_eq!(step_index(0.5, 0.25), 2);
    }

    #[test]
    fn rate_beats_are_powers_of_two() {
        #![allow(clippy::float_cmp)]
        assert_eq!(Rate::Quarter.beats_per_step(), 1.0);
        assert_eq!(Rate::Eighth.beats_per_step(), 0.5);
        assert_eq!(Rate::Sixteenth.beats_per_step(), 0.25);
        assert_eq!(Rate::ThirtySecond.beats_per_step(), 0.125);
    }

    #[test]
    fn passthrough_at_defaults() {
        // Every step defaults on at full level, so the gate is unity: a
        // constant 0.1 input passes through near 0.1 once the slew settles.
        let result = driver!(Plugin)
            .duration(Duration::from_millis(100))
            .input(InputSource::Constant(0.1))
            .run();
        assertions::assert_no_nans(&result);
        let ch = &result.output[0];
        let tail = &ch[ch.len() - 100..];
        #[allow(clippy::cast_precision_loss)]
        let mean = tail.iter().map(|s| s.abs()).sum::<f32>() / tail.len() as f32;
        assert!((mean - 0.1).abs() < 0.01, "defaults should pass through");
    }

    #[test]
    fn process_is_allocation_free() {
        use truce_test::assert_no_audio_alloc;
        assert_no_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.25))
                .run()
        });
    }

    #[test]
    fn valid_info() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn state_round_trips_through_the_array() {
        truce_test::assert_state_round_trip::<Plugin>();
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    fn _logic_is_object_safe(_: &StepGate) {}
}
