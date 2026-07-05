use truce::prelude::*;
use truce_core::editor::PluginContext;
use truce_egui::EguiEditor;
use truce_egui::theme::{HEADER_BG, HEADER_TEXT};
use truce_egui::widgets::{level_meter, param_knob, param_xy_pad};
use truce_font::JETBRAINS_MONO;

// Resize demo: the header strip stays a fixed 30 px tall and the
// central panel (knob row + XY pad + meter) fills the remaining
// area, growing with the window. Pickable defaults; the meter
// stretches vertically with the window while the knob row + XY
// pad stay at their natural sizes.
const WINDOW_W: u32 = 176;
const WINDOW_H: u32 = 290;
// Two 60 px knobs + 10 px gap = 130 px; plus a 16 px meter,
// 10 px meter-to-column gap, and 10 px padding on each side =
// 176 px - the smallest width where the XY pad column matches
// the knob row above.
const MIN_W: u32 = 176;
const MIN_H: u32 = 260;
const MAX_W: u32 = 1200;
const MAX_H: u32 = 900;
// Layout constants shared between width measurements and the
// render pass.
const METER_W: f32 = 16.0;
const GAP: f32 = 10.0;
const KNOB_W: f32 = 60.0;
const KNOB_GAP: f32 = 10.0;

// --- Parameters ---

use GainParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct GainParams {
    #[param(
        name = "Gain",
        range = "linear(-60, 6)",
        unit = "dB",
        smooth = "exp(5)"
    )]
    pub gain: FloatParam,

    #[param(name = "Pan", range = "linear(-1, 1)", unit = "pan", smooth = "exp(5)")]
    pub pan: FloatParam,

    #[meter]
    pub meter_left: MeterSlot,

    #[meter]
    pub meter_right: MeterSlot,
}

// --- Plugin ---

pub struct GainEgui {
    params: Arc<GainParams>,
}

impl GainEgui {
    pub fn new(params: Arc<GainParams>) -> Self {
        Self { params }
    }
}

impl PluginLogic for GainEgui {
    type Params = GainParams;

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
            let gain_db = self.params.gain.read();
            let pan = self.params.pan.read();
            let gain_linear = db_to_linear(gain_db);

            let gain_l = gain_linear * (1.0 - pan.max(0.0));
            let gain_r = gain_linear * (1.0 + pan.min(0.0));

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

    fn editor(params: Arc<GainParams>) -> Box<dyn Editor> {
        EguiEditor::new(params.clone(), (WINDOW_W, WINDOW_H), gain_ui)
            .with_visuals(truce_egui::theme::dark())
            .with_font(JETBRAINS_MONO)
            .resizable(true)
            .min_size((MIN_W, MIN_H))
            .max_size((MAX_W, MAX_H))
            .into_editor()
    }
}

fn gain_ui(ui: &mut egui::Ui, state: &PluginContext<GainParams>) {
    egui::Panel::top("header")
        .exact_size(30.0)
        .frame(egui::Frame::NONE.fill(HEADER_BG))
        .show_inside(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("GAIN (egui)")
                        .size(14.0)
                        .color(HEADER_TEXT)
                        .strong(),
                );
            });
        });
    egui::CentralPanel::default()
        .frame(egui::Frame::central_panel(ui.style()).inner_margin(10.0))
        .show_inside(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::TOP), |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 0.0);
                // Control column on the left: knob row up top
                // (fixed height, knobs centred) and an XY pad
                // below that grows in both axes. Allocate the
                // column up front with an explicit width so the
                // meter on the right stays 16 px wide instead of
                // getting whatever egui's greedy left-to-right
                // pass leaves behind.
                let col_w = (ui.available_width() - METER_W - GAP).max(80.0);
                let col_h = ui.available_height();
                ui.allocate_ui_with_layout(
                    egui::vec2(col_w, col_h),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(10.0, 0.0);
                            // Centre the two 60 px knobs in the
                            // column by absorbing the leading
                            // padding here; plain `ui.horizontal`
                            // packs items at the left edge, which
                            // looks off as the editor widens.
                            let row_natural = KNOB_W * 2.0 + KNOB_GAP;
                            let leading = ((ui.available_width() - row_natural) * 0.5).max(0.0);
                            ui.add_space(leading);
                            param_knob(ui, state, P::Gain, "Gain");
                            param_knob(ui, state, P::Pan, "Pan");
                        });
                        // XY pad: fills the remaining area
                        // independently in width and height.
                        // Subtract 16 px for the label that
                        // `param_xy_pad` appends below the pad
                        // (`LABEL_H` in
                        // `truce_egui::widgets::xy_pad`) so the
                        // label stays visible inside the panel.
                        // Floor each axis at 80 px so the pad
                        // stays usable at `min_size`.
                        let pad_w = ui.available_width().max(80.0);
                        let pad_h = (ui.available_height() - 16.0).max(80.0);
                        param_xy_pad(ui, state, P::Pan, P::Gain, "Pan / Gain", pad_w, pad_h);
                    },
                );
                // Meter on the right - stays at its natural width
                // and stretches vertically. No explicit
                // `add_space` here: the outer layout's
                // `item_spacing.x = GAP` already inserts the
                // gap between the column and the meter, and an
                // extra `add_space(GAP)` would push the meter
                // 10 px past the right padding (clipping the
                // meter band).
                level_meter(ui, state, &[P::MeterLeft, P::MeterRight], col_h);
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
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};
        let result = driver!(Plugin)
            .duration(Duration::from_millis(12))
            .input(InputSource::Constant(0.5))
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

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/gain_egui_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/gain_egui_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/gain_egui_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
