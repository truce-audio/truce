use truce::prelude::*;
use truce_gui::layout::{GridLayout, GridWidget};

// --- Parameters ---

use GainParamsParamId as P;

#[repr(u32)]
#[derive(Clone, Copy)]
pub enum Meter { Left = 100, Right = 101 }
impl From<Meter> for u32 { fn from(m: Meter) -> u32 { m as u32 } }

#[derive(Params)]
pub struct GainParams {
    #[param(id = 0, name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,

    #[param(id = 1, name = "Pan", range = "linear(-1, 1)",
            unit = "pan", smooth = "exp(5)")]
    pub pan: FloatParam,

    #[param(id = 2, name = "Bypass", short_name = "Byp",
            flags = "automatable | bypass")]
    pub bypass: BoolParam,
}

// --- Plugin ---

pub struct Gain {
    params: std::sync::Arc<GainParams>,
}

impl Gain {
    pub fn new(params: std::sync::Arc<GainParams>) -> Self {
        Self { params }
    }
}

impl PluginLogic for Gain {
    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
    }

    fn process(&mut self, buffer: &mut AudioBuffer, _events: &EventList, context: &mut ProcessContext) -> ProcessStatus {
        if self.params.bypass.value() {
            context.set_meter(Meter::Left, 0.0);
            context.set_meter(Meter::Right, 0.0);
            return ProcessStatus::Normal;
        }

        for i in 0..buffer.num_samples() {
            let gain_db = self.params.gain.smoothed_next();
            let pan = self.params.pan.smoothed_next();
            let gain_linear = db_to_linear(gain_db as f64) as f32;

            let pan_angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
            let gain_l = gain_linear * pan_angle.cos();
            let gain_r = gain_linear * pan_angle.sin();

            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                let g = if ch == 0 { gain_l } else { gain_r };
                out[i] = inp[i] * g;
            }
        }

        if buffer.num_output_channels() >= 1 {
            context.set_meter(Meter::Left, buffer.output_peak(0));
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(Meter::Right, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn layout(&self) -> truce_gui::layout::GridLayout {
        GridLayout::build("GAIN hello!", "V0.1", 3, 80.0, vec![
            GridWidget::knob(P::Gain, "Gain 123"),
            GridWidget::slider(P::Pan, "Pan"),
            GridWidget::toggle(P::Bypass, "Bypass"),
            GridWidget::xy_pad(P::Pan, P::Gain, "XY"),
            GridWidget::meter(&[Meter::Left.into(), Meter::Right.into()], "Level").rows(2),
        ], vec![])
    }
}

// One macro. That's it.
truce::plugin! {
    logic: Gain,
    params: GainParams,
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
        let result = truce_test::render_effect::<Plugin>(512, 44100.0);
        truce_test::assert_nonzero(&result.output);
        truce_test::assert_no_nans(&result.output);
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
    fn au_type_codes_ascii() {
        truce_test::assert_au_type_codes_ascii::<Plugin>();
    }

    #[test]
    fn fourcc_roundtrip() {
        truce_test::assert_fourcc_roundtrip::<Plugin>();
    }

    #[test]
    fn bus_config_effect() {
        truce_test::assert_bus_config_effect::<Plugin>();
    }

    #[test]
    fn editor_lifecycle() {
        truce_test::assert_editor_lifecycle::<Plugin>();
    }

    #[test]
    fn editor_size_consistent() {
        truce_test::assert_editor_size_consistent::<Plugin>();
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
    fn param_count_matches() {
        truce_test::assert_param_count_matches::<Plugin>();
    }

    #[test]
    fn no_duplicate_param_ids() {
        truce_test::assert_no_duplicate_param_ids::<Plugin>();
    }

    #[test]
    fn corrupt_state_no_crash() {
        truce_test::assert_corrupt_state_no_crash::<Plugin>();
    }

    #[test]
    fn empty_state_no_crash() {
        truce_test::assert_empty_state_no_crash::<Plugin>();
    }

    #[test]
    fn gui_snapshot() {
        let params = std::sync::Arc::new(GainParams::new());
        let gain = Gain::new(std::sync::Arc::clone(&params));
        let layout = gain.layout();
        truce_test::assert_gui_snapshot_grid::<GainParams>(
            "gain_default", params, layout, 0,
        );
    }
}
