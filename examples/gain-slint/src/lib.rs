use std::f32::consts::FRAC_PI_4;

use truce::prelude::*;
use truce_slint::{SlintEditor, ParamState};

slint::include_modules!();

// --- Parameters ---

use GainParamsParamId as P;

#[derive(Params)]
pub struct GainParams {
    #[param(name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,

    #[param(name = "Pan", range = "linear(-1, 1)",
            unit = "pan", smooth = "exp(5)")]
    pub pan: FloatParam,

    #[meter]
    pub meter_left: MeterSlot,

    #[meter]
    pub meter_right: MeterSlot,
}

// --- Plugin ---

pub struct GainSlint {
    params: Arc<GainParams>,
}

impl GainSlint {
    pub fn new(params: Arc<GainParams>) -> Self {
        Self { params }
    }
}

impl PluginLogic for GainSlint {
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
        for i in 0..buffer.num_samples() {
            let gain_db = self.params.gain.smoothed_next();
            let pan = self.params.pan.smoothed_next();
            let gain_linear = db_to_linear(gain_db as f64) as f32;

            let pan_angle = (pan + 1.0) * FRAC_PI_4;
            let gain_l = gain_linear * pan_angle.cos();
            let gain_r = gain_linear * pan_angle.sin();

            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                let g = if ch == 0 { gain_l } else { gain_r };
                out[i] = inp[i] * g;
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

    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        Some(Box::new(SlintEditor::new((176, 290), |state: ParamState| {
            let ui = GainUi::new().unwrap();

            // UI → host
            let s = state.clone();
            ui.on_gain_changed(move |v| s.set_immediate(P::Gain, v as f64));
            let s = state.clone();
            ui.on_pan_changed(move |v| s.set_immediate(P::Pan, v as f64));

            // host → UI (params + meters)
            Box::new(move |state: &ParamState| {
                ui.set_gain(state.get(P::Gain) as f32);
                ui.set_pan(state.get(P::Pan) as f32);
                ui.set_gain_text(slint::SharedString::from(state.format(P::Gain)));
                ui.set_pan_text(slint::SharedString::from(state.format(P::Pan)));
                ui.set_meter_left(meter_display(state.meter(P::MeterLeft)));
                ui.set_meter_right(meter_display(state.meter(P::MeterRight)));
            })
        })))
    }
}

truce::plugin! {
    logic: GainSlint,
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
    fn gui_snapshot() {
        truce_slint::snapshot::assert_snapshot(
            "screenshots", "gain_slint_default",
            176, 290, 2.0, 0,
            |state| {
                let ui = GainUi::new().unwrap();
                Box::new(move |state: &truce_slint::ParamState| {
                    ui.set_gain(state.get(P::Gain) as f32);
                    ui.set_pan(state.get(P::Pan) as f32);
                    ui.set_gain_text(slint::SharedString::from(state.format(P::Gain)));
                    ui.set_pan_text(slint::SharedString::from(state.format(P::Pan)));
                    ui.set_meter_left(meter_display(state.meter(P::MeterLeft)));
                    ui.set_meter_right(meter_display(state.meter(P::MeterRight)));
                })
            },
        );
    }
}
