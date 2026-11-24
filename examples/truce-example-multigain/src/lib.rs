//! Nested-array params demonstration: a rack of identical gain stages
//! applied in series.
//!
//! The stages are one `#[nested]` field typed as a fixed-size array
//! (`[StageParams; NUM_STAGES]`), so the same parameter group is
//! instantiated `NUM_STAGES` times and the DSP + editor just loop over
//! it. Each element is its own id range, packed contiguously after the
//! one before it; `#[nested(base = 0)]` pins element 0 so the flattened
//! ids are wire-stable.
//!
//! `derive(Params)` also ships a static id resolver per array field -
//! `MultiGainParams::stages_id(i, StageParamsParamId::Gain)` - for code
//! that addresses an element's param without a live instance. The editor
//! below reads the live field ids instead (`params.stages[i].gain.id()`),
//! which is equivalent for an existing `params`.

use std::sync::Arc;

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, section, toggle};

/// Number of gain stages in the rack. The `#[nested]` array length must
/// be an integer literal (the derive needs the count at expansion time),
/// so this const is asserted against it rather than used in the type.
const NUM_STAGES: usize = 3;
const _: () = assert!(NUM_STAGES == 3);

/// One rack stage: a gain in dB plus an on/off toggle. Ordinal ids so
/// the group numbers locally 0 (gain), 1 (active); the parent rebases
/// each array element off those.
#[derive(Params)]
#[params(id_scheme = "ordinal")]
pub struct StageParams {
    #[param(
        name = "Gain",
        range = "linear(-24, 24)",
        unit = "dB",
        default = 0.0,
        smooth = "exp(5)"
    )]
    pub gain: FloatParam,

    #[param(name = "Active", default = true)]
    pub active: BoolParam,
}

/// The rack: a fixed-size array of stages followed by a master trim.
/// Pinned bases (stages at 0, master after the array's span) fix the
/// flattened ids a shipped plugin's saved state depends on.
#[derive(Params)]
#[params(id_scheme = "ordinal")]
pub struct MultiGainParams {
    #[nested(base = 0)]
    pub stages: [StageParams; 3],

    #[param(
        id = 6,
        name = "Master",
        range = "linear(-24, 24)",
        unit = "dB",
        default = 0.0,
        smooth = "exp(5)"
    )]
    pub master: FloatParam,
}

/// Stateless descriptor - the DSP is a product of per-stage gains.
pub struct MultiGain;

impl PurePluginLogic for MultiGain {
    type Params = MultiGainParams;

    fn process(
        params: &MultiGainParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        for i in 0..buffer.num_samples() {
            // `read` advances each smoother one step, so call it once per
            // sample for every stage (even an inactive one) to keep the
            // ramps moving; multiply in only the active stages.
            let mut gain = db_to_linear(params.master.read());
            for stage in &params.stages {
                let stage_gain = db_to_linear(stage.gain.read());
                if stage.active.value() {
                    gain *= stage_gain;
                }
            }
            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * gain;
            }
        }
        ProcessStatus::Normal
    }

    fn editor(params: Arc<MultiGainParams>) -> Box<dyn Editor> {
        // One column per stage. Labels are static, so index a const
        // table rather than formatting per stage.
        const STAGE_LABELS: [&str; NUM_STAGES] = ["Stage 1", "Stage 2", "Stage 3"];
        let mut sections: Vec<_> = (0..NUM_STAGES)
            .map(|i| {
                section(
                    STAGE_LABELS[i],
                    vec![
                        knob(params.stages[i].gain.id(), "Gain"),
                        toggle(params.stages[i].active.id(), "On"),
                    ],
                )
            })
            .collect();
        sections.push(section("MASTER", vec![knob(params.master.id(), "Trim")]));
        GridLayout::build(sections)
            .with_title("MULTIGAIN")
            .into_editor(&params)
    }
}

truce::plugin! {
    logic: MultiGain,
    params: MultiGainParams,
}

#[cfg(test)]
mod tests {
    use super::{MultiGain, MultiGainParams, NUM_STAGES, Plugin, StageParamsParamId};
    use std::time::Duration;
    use truce_params::Params;
    use truce_test::{InputSource, assertions, driver};

    #[test]
    fn stage_ids_pack_contiguously() {
        let p = MultiGainParams::new();
        // Three stages of two params each (0..5), then master at 6.
        // (`param_infos` lists own params first, so sort before comparing.)
        assert_eq!(p.count(), NUM_STAGES * 2 + 1);
        let mut ids: Vec<u32> = p.param_infos().iter().map(|i| i.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn id_helper_matches_live_field() {
        let p = MultiGainParams::new();
        for i in 0..NUM_STAGES {
            assert_eq!(
                MultiGainParams::stages_id(i, StageParamsParamId::Gain),
                p.stages[i].gain.id()
            );
            assert_eq!(
                MultiGainParams::stages_id(i, StageParamsParamId::Active),
                p.stages[i].active.id()
            );
        }
    }

    // Mean magnitude of the settled tail (after the smoothers ramp in).
    fn tail_mean(result: &truce_test::DriverResult<Plugin>) -> f32 {
        let ch = &result.output[0];
        let tail = &ch[ch.len() - 100..];
        #[allow(clippy::cast_precision_loss)]
        let n = tail.len() as f32;
        tail.iter().map(|s| s.abs()).sum::<f32>() / n
    }

    #[test]
    fn unity_at_defaults() {
        // Every stage defaults to 0 dB and active, so the product is
        // unity: a constant 0.1 input passes through near 0.1.
        let result = driver!(Plugin)
            .duration(Duration::from_millis(100))
            .input(InputSource::Constant(0.1))
            .run();
        assertions::assert_no_nans(&result);
        assert!((tail_mean(&result) - 0.1).abs() < 0.01);
    }

    #[test]
    fn deactivating_a_stage_drops_its_gain() {
        // Stage 0 at +24 dB active makes the rack loud; toggling it off
        // returns the product to unity.
        let gain0 = MultiGainParams::stages_id(0, StageParamsParamId::Gain);
        let active0 = MultiGainParams::stages_id(0, StageParamsParamId::Active);

        let loud = driver!(Plugin)
            .duration(Duration::from_millis(100))
            .input(InputSource::Constant(0.1))
            .set_param(gain0, 1.0) // normalized top of the range: +24 dB
            .run();
        let bypassed = driver!(Plugin)
            .duration(Duration::from_millis(100))
            .input(InputSource::Constant(0.1))
            .set_param(gain0, 1.0)
            .set_param(active0, 0.0) // off
            .run();

        assert!(tail_mean(&loud) > 1.0, "+24 dB stage should amplify");
        assert!(
            (tail_mean(&bypassed) - 0.1).abs() < 0.01,
            "an inactive stage must not contribute gain"
        );
    }

    #[test]
    fn state_round_trips_through_the_array() {
        truce_test::assert_state_round_trip::<Plugin>();
    }

    #[test]
    fn valid_info() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    fn _logic_is_object_safe(_: &MultiGain) {}
}
