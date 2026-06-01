//! egui counterpart of `truce-example-gui-zoo`. Same param shapes
//! (so every unit / range / discrete-snap path is exercised) but
//! laid out through egui's containers instead of the built-in
//! `GridLayout`. Layout / widget regressions in `truce-egui`
//! surface here before they reach plugins that consume it.

use truce::prelude::*;
use truce_core::editor::PluginContext;
use truce_egui::EguiEditor;
use truce_egui::theme::{HEADER_BG, HEADER_TEXT};
use truce_egui::widgets::{
    level_meter, param_dropdown, param_knob, param_selector, param_slider, param_toggle,
    param_xy_pad,
};
use truce_font::JETBRAINS_MONO;

use ZooParamsParamId as P;
use std::sync::Arc;

const WINDOW_W: u32 = 700;
const WINDOW_H: u32 = 900;

#[derive(ParamEnum)]
pub enum Shape {
    Sine,
    Triangle,
    Square,
    Sawtooth,
}

#[derive(ParamEnum)]
pub enum Mode {
    #[name = "Mode A"]
    A,
    #[name = "Mode B"]
    B,
    #[name = "Mode C"]
    C,
    #[name = "Mode D"]
    D,
    #[name = "Mode E"]
    E,
    #[name = "Mode F"]
    F,
    #[name = "Mode G"]
    G,
    #[name = "Mode H"]
    H,
}

#[derive(Params)]
pub struct ZooParams {
    // -- Knobs: mixed ranges, units, defaults to exercise every
    // formatter + range-parser path through the egui widgets. --
    #[param(name = "Mix", range = "linear(0, 1)", default = 0.5, unit = "%")]
    pub k_mix: FloatParam,
    #[param(name = "Gain", range = "linear(-60, 6)", default = 0, unit = "dB")]
    pub k_gain: FloatParam,
    #[param(name = "Freq", range = "log(20, 20000)", default = 1000, unit = "Hz")]
    pub k_freq: FloatParam,
    #[param(name = "Q", range = "log(0.1, 20)", default = std::f64::consts::PI)]
    pub k_q: FloatParam,
    #[param(name = "Phase", range = "linear(0, 360)", default = 180, unit = "deg")]
    pub k_phase: FloatParam,
    #[param(name = "Pitch", range = "discrete(-12, 12)", default = 0, unit = "st")]
    pub k_pitch: IntParam,
    #[param(name = "Time", range = "linear(0, 1000)", default = 200, unit = "ms")]
    pub k_time: FloatParam,
    #[param(name = "Release", range = "linear(0, 10)", default = 1.5, unit = "s")]
    pub k_release: FloatParam,
    #[param(name = "Pan", range = "linear(-1, 1)", default = 0, unit = "pan")]
    pub k_pan: FloatParam,

    // -- Sliders --
    #[param(name = "Float", range = "linear(0, 1)", default = 0.5, unit = "%")]
    pub s_float: FloatParam,
    #[param(name = "Int", range = "discrete(0, 10)", default = 5)]
    pub s_int: IntParam,
    #[param(name = "Wide", range = "linear(-60, 6)", default = 0, unit = "dB")]
    pub s_wide: FloatParam,

    // -- Toggles --
    #[param(name = "On", default = true)]
    pub t_on: BoolParam,
    #[param(name = "Off")]
    pub t_off: BoolParam,

    // -- Selector + dropdowns --
    #[param(name = "Shape")]
    pub shape: EnumParam<Shape>,
    #[param(name = "Mode")]
    pub mode: EnumParam<Mode>,
    #[param(name = "Mode Wide")]
    pub mode_wide: EnumParam<Mode>,

    // -- Meters: single, stereo pair, six-channel bus --
    #[meter]
    pub m_in: MeterSlot,
    #[meter]
    pub m_l: MeterSlot,
    #[meter]
    pub m_r: MeterSlot,
    #[meter]
    pub m_6a: MeterSlot,
    #[meter]
    pub m_6b: MeterSlot,
    #[meter]
    pub m_6c: MeterSlot,
    #[meter]
    pub m_6d: MeterSlot,
    #[meter]
    pub m_6e: MeterSlot,
    #[meter]
    pub m_6f: MeterSlot,
}

pub struct ZooEgui {
    params: Arc<ZooParams>,
}

impl ZooEgui {
    #[must_use]
    pub fn new(params: Arc<ZooParams>) -> Self {
        Self { params }
    }
}

impl PluginLogic for ZooEgui {
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
        // Passthrough copy in -> out per channel.
        let n_in = buffer.num_input_channels();
        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            if ch < n_in {
                out.copy_from_slice(inp);
            } else {
                out.fill(0.0);
            }
        }

        // Stepped fractions of the output peak so the 6-channel bus
        // meter renders six distinct heights even from a single input.
        if buffer.num_output_channels() >= 1 {
            let p = buffer.output_peak(0);
            context.set_meter(P::MIn, p);
            context.set_meter(P::ML, p);
            context.set_meter(P::M6a, p);
            context.set_meter(P::M6b, p * 0.83);
            context.set_meter(P::M6c, p * 0.66);
            context.set_meter(P::M6d, p * 0.5);
            context.set_meter(P::M6e, p * 0.33);
            context.set_meter(P::M6f, p * 0.17);
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MR, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn editor(&self) -> Box<dyn Editor> {
        EguiEditor::new(self.params.clone(), (WINDOW_W, WINDOW_H), zoo_ui)
            .with_visuals(truce_egui::theme::dark())
            .with_font(JETBRAINS_MONO)
            .into_editor()
    }
}

fn zoo_ui(ui: &mut egui::Ui, state: &PluginContext<ZooParams>) {
    egui::Panel::top("header")
        .exact_size(30.0)
        .frame(egui::Frame::NONE.fill(HEADER_BG))
        .show_inside(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("GUI ZOO (egui)")
                        .size(14.0)
                        .color(HEADER_TEXT)
                        .strong(),
                );
            });
        });
    egui::CentralPanel::default()
        .frame(egui::Frame::central_panel(ui.style()).inner_margin(10.0))
        .show_inside(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(10.0, 12.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    section(ui, "Knobs");
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(10.0, 0.0);
                        param_knob(ui, state, P::KMix, "Mix");
                        param_knob(ui, state, P::KGain, "Gain");
                        param_knob(ui, state, P::KFreq, "Freq");
                        param_knob(ui, state, P::KQ, "Q");
                        param_knob(ui, state, P::KPhase, "Phase");
                        param_knob(ui, state, P::KPitch, "Pitch");
                        param_knob(ui, state, P::KTime, "Time");
                        param_knob(ui, state, P::KRelease, "Rel");
                        param_knob(ui, state, P::KPan, "Pan");
                    });

                    section(ui, "Sliders");
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(20.0, 0.0);
                        ui.vertical(|ui| {
                            ui.label("Float");
                            param_slider(ui, state, P::SFloat);
                        });
                        ui.vertical(|ui| {
                            ui.label("Int");
                            param_slider(ui, state, P::SInt);
                        });
                        ui.vertical(|ui| {
                            ui.label("Wide (dB)");
                            param_slider(ui, state, P::SWide);
                        });
                    });

                    section(ui, "Toggles & Selector");
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(20.0, 0.0);
                        param_toggle(ui, state, P::TOn, "On");
                        param_toggle(ui, state, P::TOff, "Off");
                        // `step_count` for `param_selector` is the
                        // number of options; matches `Shape` variants.
                        param_selector(ui, state, P::Shape, "Shape", 4);
                    });

                    section(ui, "Dropdowns");
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(20.0, 0.0);
                        // `cols` controls how the popup grid is shaped.
                        param_dropdown(ui, state, P::Mode, "Mode", 1);
                        param_dropdown(ui, state, P::ModeWide, "Mode Wide", 2);
                    });

                    section(ui, "Meters");
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(20.0, 0.0);
                        level_meter(ui, state, &[P::MIn], 140.0);
                        level_meter(ui, state, &[P::ML, P::MR], 140.0);
                        level_meter(
                            ui,
                            state,
                            &[P::M6a, P::M6b, P::M6c, P::M6d, P::M6e, P::M6f],
                            140.0,
                        );
                    });

                    section(ui, "XY Pads");
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(20.0, 0.0);
                        param_xy_pad(ui, state, P::KMix, P::KGain, "small", 80.0, 80.0);
                        param_xy_pad(ui, state, P::KFreq, P::KQ, "med", 130.0, 130.0);
                        param_xy_pad(ui, state, P::KPan, P::KPhase, "big", 200.0, 200.0);
                    });
                });
        });
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .size(11.0)
            .color(HEADER_TEXT)
            .strong(),
    );
}

truce::plugin! {
    logic: ZooEgui,
    params: ZooParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
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
    fn editor_size_consistent() {
        truce_test::assert_editor_size_consistent::<Plugin>();
    }

    #[test]
    fn param_defaults_match() {
        truce_test::assert_param_defaults_match::<Plugin>();
    }

    #[test]
    fn no_duplicate_param_ids() {
        truce_test::assert_no_duplicate_param_ids::<Plugin>();
    }

    #[test]
    fn passthrough() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .sample_rate(44_100.0)
            .channels(2)
            .duration(Duration::from_millis(20))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_nonzero(&result);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_egui_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_egui_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_egui_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
