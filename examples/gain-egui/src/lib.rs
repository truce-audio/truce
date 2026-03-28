use truce::prelude::*;
use truce_egui::{EguiEditor, ParamState};
use truce_egui::theme::{HEADER_BG, HEADER_TEXT};
use truce_egui::widgets::{param_knob, param_xy_pad, level_meter};

const WINDOW_W: u32 = 225;
const WINDOW_H: u32 = 300;

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

pub struct GainEgui {
    params: std::sync::Arc<GainParams>,
}

impl GainEgui {
    pub fn new(params: std::sync::Arc<GainParams>) -> Self {
        Self { params }
    }
}

impl PluginLogic for GainEgui {
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
            context.set_meter(P::MeterLeft, buffer.output_peak(0));
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MeterRight, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        Some(Box::new(
            EguiEditor::new((WINDOW_W, WINDOW_H), gain_ui)
                .with_visuals(truce_egui::theme::dark())
                .with_font(truce_gui::font::JETBRAINS_MONO),
        ))
    }
}

fn gain_ui(ctx: &egui::Context, state: &ParamState) {
    egui::TopBottomPanel::top("header")
        .exact_height(30.0)
        .frame(egui::Frame::NONE.fill(HEADER_BG))
        .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("TRUCE GAIN (EGUI)")
                        .size(14.0)
                        .color(HEADER_TEXT)
                        .strong(),
                );
            });
        });
    egui::CentralPanel::default().show(ctx, |ui| {
        let content_h = ui.available_height();
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    param_knob(ui, state, P::Gain, "Gain");
                    ui.add_space(16.0);
                    param_knob(ui, state, P::Pan, "Pan");
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    param_xy_pad(ui, state, P::Pan, P::Gain, "Pan / Gain", 140.0, 140.0);
                });
            });
            ui.add_space(16.0);
            level_meter(ui, state, &[P::MeterLeft, P::MeterRight], "Level", content_h - 35.0);
        });
    });
}

truce::plugin! {
    logic: GainEgui,
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
        truce_egui::snapshot::assert_snapshot(
            "screenshots", "gain_egui_default",
            WINDOW_W, WINDOW_H, 2.0, 0,
            Some(truce_gui::font::JETBRAINS_MONO),
            |ctx, state| gain_ui(ctx, state),
        );
    }
}
