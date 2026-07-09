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
    level_meter, param_dropdown, param_knob, param_slider, param_toggle, param_xy_pad,
};
use truce_font::JETBRAINS_MONO;

use ZooParamsParamId as P;
use std::sync::Arc;

const WINDOW_W: u32 = 700;
const WINDOW_H: u32 = 900;

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

    // -- Dropdowns --
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

/// Stateless descriptor - passthrough carries no DSP state, only params.
pub struct ZooEgui;

impl PluginLogic for ZooEgui {
    type Params = ZooParams;
    type DspState = ();

    fn init(_params: &ZooParams) {}

    fn process(
        _state: &mut (),
        _params: &ZooParams,
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

    fn editor(params: Arc<ZooParams>) -> Box<dyn Editor> {
        EguiEditor::new(params.clone(), (WINDOW_W, WINDOW_H), zoo_ui)
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

                    // Pair short sections side by side so the native-widget
                    // section below lands in the default (unscrolled) view.
                    ui.horizontal_top(|ui| {
                        ui.vertical(|ui| {
                            section(ui, "Toggles");
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(20.0, 0.0);
                                param_toggle(ui, state, P::TOn, "On");
                                param_toggle(ui, state, P::TOff, "Off");
                            });
                        });
                        ui.add_space(30.0);
                        ui.vertical(|ui| {
                            section(ui, "Dropdowns");
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(20.0, 0.0);
                                // `cols` controls how the popup grid is shaped.
                                param_dropdown(ui, state, P::Mode, "Mode", 1);
                                param_dropdown(ui, state, P::ModeWide, "Mode Wide", 2);
                            });
                        });
                    });

                    ui.horizontal_top(|ui| {
                        ui.vertical(|ui| {
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
                        });
                        ui.add_space(30.0);
                        ui.vertical(|ui| {
                            section(ui, "XY Pads");
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(20.0, 0.0);
                                param_xy_pad(ui, state, P::KMix, P::KGain, "small", 80.0, 80.0);
                                param_xy_pad(ui, state, P::KFreq, P::KQ, "med", 130.0, 130.0);
                                param_xy_pad(ui, state, P::KPan, P::KPhase, "big", 200.0, 200.0);
                            });
                        });
                    });

                    section(ui, "egui Widgets");
                    egui_widgets_section(ui);

                    section(ui, "Keyboard");
                    keyboard_section(ui);
                });
        });
}

/// Decode the embedded carrot.png (16x16 RGBA) into a GPU texture, cached in
/// the `Context` so it uploads once. A plugin can't open a URL, so the
/// image-widget demo embeds the bytes rather than fetching them.
fn carrot_texture(ctx: &egui::Context) -> egui::TextureHandle {
    let cache_id = egui::Id::new("zoo_carrot_tex");
    if let Some(handle) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(cache_id)) {
        return handle;
    }
    let (w, h, rgba) = decode_carrot();
    let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
    // NEAREST keeps the 16x16 pixel art crisp when scaled up.
    let handle = ctx.load_texture("zoo_carrot", image, egui::TextureOptions::NEAREST);
    ctx.data_mut(|d| d.insert_temp(cache_id, handle.clone()));
    handle
}

/// Decode the shared carrot.png to `(width, height, rgba)`.
fn decode_carrot() -> (u32, u32, Vec<u8>) {
    let mut reader = png::Decoder::new(&include_bytes!("../../../static/carrot.png")[..])
        .read_info()
        .expect("carrot.png header");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("carrot.png frame");
    assert_eq!(
        info.color_type,
        png::ColorType::Rgba,
        "carrot.png must be RGBA"
    );
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

/// Native egui widgets: button, checkbox, radio, drag value, image,
/// horizontal + vertical slider, progress bar, separator. `zoo_ui` is
/// stateless, so each widget's value lives in egui's `Context` memory.
fn egui_widgets_section(ui: &mut egui::Ui) {
    let counter_id = egui::Id::new("zoo_w_counter");
    let check_id = egui::Id::new("zoo_w_check");
    let radio_id = egui::Id::new("zoo_w_radio");
    let slider_id = egui::Id::new("zoo_w_slider");
    let vslider_id = egui::Id::new("zoo_w_vslider");
    let drag_id = egui::Id::new("zoo_w_drag");

    let mut counter = ui.data_mut(|d| d.get_temp::<u32>(counter_id).unwrap_or(0));
    let mut checked = ui.data_mut(|d| d.get_temp::<bool>(check_id).unwrap_or(true));
    let mut radio = ui.data_mut(|d| d.get_temp::<u8>(radio_id).unwrap_or(0));
    let mut sval = ui.data_mut(|d| d.get_temp::<f32>(slider_id).unwrap_or(0.4));
    let mut vsval = ui.data_mut(|d| d.get_temp::<f32>(vslider_id).unwrap_or(0.6));
    let mut drag = ui.data_mut(|d| d.get_temp::<f32>(drag_id).unwrap_or(1.0));

    // A label | widget grid keeps the mixed widget kinds aligned. (egui's
    // `DragValue` is a drag-to-edit number box - labelling it avoids the
    // "mystery text field" confusion.)
    let carrot = carrot_texture(ui.ctx());
    egui::Grid::new("zoo_egui_widgets")
        .num_columns(2)
        .spacing([16.0, 8.0])
        .show(ui, |ui| {
            ui.label("button");
            if ui.button(format!("clicked {counter}x")).clicked() {
                counter += 1;
            }
            ui.end_row();

            ui.label("checkbox");
            ui.checkbox(&mut checked, "");
            ui.end_row();

            ui.label("radio");
            ui.horizontal(|ui| {
                ui.radio_value(&mut radio, 0, "Alpha");
                ui.radio_value(&mut radio, 1, "Beta");
                ui.radio_value(&mut radio, 2, "Gamma");
            });
            ui.end_row();

            ui.label("drag value");
            ui.add(egui::DragValue::new(&mut drag).speed(0.05));
            ui.end_row();

            ui.label("slider");
            ui.add(egui::Slider::new(&mut sval, 0.0..=1.0));
            ui.end_row();

            ui.label("progress");
            ui.add(egui::ProgressBar::new(sval).show_percentage());
            ui.end_row();
        });

    // The tall widgets (vertical slider + image) sit side by side under the
    // grid so they don't each stretch a grid row.
    ui.add_space(8.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(28.0, 0.0);
        ui.vertical(|ui| {
            ui.label("v-slider");
            ui.spacing_mut().slider_width = 60.0;
            ui.add(egui::Slider::new(&mut vsval, 0.0..=1.0).vertical());
        });
        ui.vertical(|ui| {
            ui.label("image");
            ui.add(egui::Image::from_texture(&carrot).fit_to_exact_size(egui::vec2(48.0, 48.0)));
        });
    });
    ui.separator();

    ui.data_mut(|d| {
        d.insert_temp(counter_id, counter);
        d.insert_temp(check_id, checked);
        d.insert_temp(radio_id, radio);
        d.insert_temp(slider_id, sval);
        d.insert_temp(vslider_id, vsval);
        d.insert_temp(drag_id, drag);
    });
}

/// Keyboard demo: a text box (keys reach the focused widget) over a label
/// that mirrors the last key press from anywhere. `zoo_ui` is stateless, so
/// the box contents and last-key label live in egui's `Context` memory.
fn keyboard_section(ui: &mut egui::Ui) {
    let box_id = egui::Id::new("zoo_keyboard_textbox");
    let key_id = egui::Id::new("zoo_keyboard_last");

    // Immediate-mode: keys are this frame's input events. Take the most
    // recent press (any focus) and remember it.
    let pressed = ui.input(|i| {
        i.events.iter().rev().find_map(|e| match e {
            egui::Event::Key {
                key,
                pressed: true,
                physical_key,
                ..
            } => Some(match physical_key {
                Some(phys) => format!("{key:?}  phys={phys:?}"),
                None => format!("{key:?}"),
            }),
            _ => None,
        })
    });
    if let Some(label) = pressed {
        ui.data_mut(|d| d.insert_temp(key_id, label));
    }

    let mut buf = ui.data_mut(|d| d.get_temp::<String>(box_id).unwrap_or_default());
    ui.add(
        egui::TextEdit::singleline(&mut buf)
            .hint_text("type here...")
            .desired_width(360.0),
    );
    ui.data_mut(|d| d.insert_temp(box_id, buf));

    let last = ui
        .data_mut(|d| d.get_temp::<String>(key_id))
        .unwrap_or_else(|| "(press a key)".to_string());
    ui.label(format!("last key: {last}"));
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
    fn carrot_png_decodes_to_a_real_image() {
        let (w, h, rgba) = decode_carrot();
        assert_eq!((w, h), (16, 16));
        assert_eq!(rgba.len(), 16 * 16 * 4);
        // Not a uniform fill - the carrot has more than one colour.
        let first = &rgba[0..4];
        assert!(rgba.chunks(4).any(|px| px != first));
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
