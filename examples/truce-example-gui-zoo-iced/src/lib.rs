//! iced counterpart of `truce-example-gui-zoo`. Same param shapes
//! (so every unit / range / discrete-snap path is exercised) but
//! laid out through iced's containers instead of the built-in
//! `GridLayout`.

// Same iOS gate as `truce-example-gain-iced`: iced's `iced_winit`
// dep doesn't build for iOS.
#![cfg(not(target_os = "ios"))]

use std::sync::Arc;

use iced::widget::{Column, Row, container, text, text_input};
use iced::{Element, Font, Length, Task, alignment};

const JETBRAINS_MONO: Font = Font {
    family: iced::font::Family::Name("JetBrains Mono"),
    ..Font::DEFAULT
};
const WINDOW_W: u32 = 700;
const WINDOW_H: u32 = 900;
const HEADER_BG: iced::Color = iced::Color::from_rgb(0.08, 0.08, 0.10);
const HEADER_TEXT: iced::Color = iced::Color::from_rgb(0.75, 0.75, 0.80);

use truce::prelude::*;
use truce_iced::{
    IcedEditor, IcedPlugin, IntoElement, Message, ParamCache, PluginContext, knob, meter,
    param_dropdown, param_slider, param_toggle, xy_pad,
};

use ZooParamsParamId as P;

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
    // -- Knobs (mixed ranges + units to exercise every formatter path) --
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

    // -- Dropdown --
    #[param(name = "Mode")]
    pub mode: EnumParam<Mode>,

    // -- Meters: single, stereo pair, 6-channel bus --
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

// --- Custom iced UI ---

#[derive(Default)]
pub struct ZooUi {
    /// Contents of the keyboard-section text box (widget-path keyboard).
    textbox: String,
    /// Label for the most recent key press (subscription-path keyboard).
    last_key: String,
}

#[derive(Debug, Clone)]
pub enum ZooMsg {
    /// Text box edited (from the `text_input` widget).
    TextChanged(String),
    /// A key was pressed anywhere in the editor (from the subscription).
    KeyPressed(String),
}

/// Human-readable label for a key press: logical key (or typed text) plus
/// the layout-independent physical code.
fn key_label(
    key: &iced::keyboard::Key,
    physical: iced::keyboard::key::Physical,
    text: Option<&str>,
) -> String {
    use iced::keyboard::Key;
    let logical = match key {
        Key::Character(c) => format!("'{c}'"),
        Key::Named(named) => format!("{named:?}"),
        Key::Unidentified => "Unidentified".to_string(),
    };
    let phys = match physical {
        iced::keyboard::key::Physical::Code(code) => format!("{code:?}"),
        iced::keyboard::key::Physical::Unidentified(_) => "?".to_string(),
    };
    match text {
        Some(t) if !t.trim().is_empty() => format!("{logical}  text={t:?}  phys={phys}"),
        _ => format!("{logical}  phys={phys}"),
    }
}

impl IcedPlugin<ZooParams> for ZooUi {
    type Message = ZooMsg;

    fn new(_params: Arc<ZooParams>) -> Self {
        Self {
            textbox: String::new(),
            last_key: String::from("(press a key)"),
        }
    }

    fn update(
        &mut self,
        message: Message<ZooMsg>,
        _params: &ParamCache<ZooParams>,
        _ctx: &PluginContext<ZooParams>,
    ) -> Task<Message<ZooMsg>> {
        if let Message::Plugin(msg) = message {
            match msg {
                ZooMsg::TextChanged(s) => self.textbox = s,
                ZooMsg::KeyPressed(label) => self.last_key = label,
            }
        }
        Task::none()
    }

    fn subscription(&self) -> iced::Subscription<Message<ZooMsg>> {
        // Mirror every key press, regardless of which widget has focus, by
        // listening to the raw event stream (the truce-iced subscription
        // pump drives this). Returning `None` skips releases / modifier
        // changes so only presses update the label.
        iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key,
                physical_key,
                text,
                ..
            }) => Some(Message::Plugin(ZooMsg::KeyPressed(key_label(
                &key,
                physical_key,
                text.as_deref(),
            )))),
            _ => None,
        })
    }

    fn view<'a>(&'a self, params: &'a ParamCache<ZooParams>) -> Element<'a, Message<ZooMsg>> {
        let pad = 12.0;
        let gap = 10.0;

        let header: Element<'a, Message<ZooMsg>> = container(
            text("GUI ZOO (iced)")
                .size(14)
                .font(JETBRAINS_MONO)
                .color(HEADER_TEXT),
        )
        .padding(iced::Padding::from([8.0, 10.0]))
        .width(Length::Fill)
        .style(|_theme: &iced::Theme| container::Style {
            background: Some(HEADER_BG.into()),
            ..Default::default()
        })
        .into();

        let knobs_row: Element<'a, Message<ZooMsg>> = Row::new()
            .push(knob(P::KMix, params).label("Mix").size(60.0).el())
            .push(knob(P::KGain, params).label("Gain").size(60.0).el())
            .push(knob(P::KFreq, params).label("Freq").size(60.0).el())
            .push(knob(P::KQ, params).label("Q").size(60.0).el())
            .push(knob(P::KPhase, params).label("Phase").size(60.0).el())
            .push(knob(P::KPitch, params).label("Pitch").size(60.0).el())
            .push(knob(P::KTime, params).label("Time").size(60.0).el())
            .push(knob(P::KRelease, params).label("Rel").size(60.0).el())
            .push(knob(P::KPan, params).label("Pan").size(60.0).el())
            .spacing(gap)
            .align_y(alignment::Vertical::Center)
            .into();

        let sliders_row: Element<'a, Message<ZooMsg>> = Row::new()
            .push(
                param_slider(P::SFloat, params)
                    .label("Float")
                    .width(160.0)
                    .el(),
            )
            .push(param_slider(P::SInt, params).label("Int").width(160.0).el())
            .push(
                param_slider(P::SWide, params)
                    .label("Wide (dB)")
                    .width(220.0)
                    .el(),
            )
            .spacing(20.0)
            .align_y(alignment::Vertical::Bottom)
            .into();

        let toggles_row: Element<'a, Message<ZooMsg>> = Row::new()
            .push(param_toggle(P::TOn, params).label("On").el())
            .push(param_toggle(P::TOff, params).label("Off").el())
            .spacing(20.0)
            .align_y(alignment::Vertical::Bottom)
            .into();

        let dropdown_row: Element<'a, Message<ZooMsg>> = Row::new()
            .push(param_dropdown(P::Mode, params).label("Mode").el())
            .spacing(20.0)
            .align_y(alignment::Vertical::Bottom)
            .into();

        let meters_row: Element<'a, Message<ZooMsg>> = Row::new()
            .push(meter(&[P::MIn], params).size(16.0, 140.0).el())
            .push(meter(&[P::ML, P::MR], params).size(16.0, 140.0).el())
            .push(
                meter(&[P::M6a, P::M6b, P::M6c, P::M6d, P::M6e, P::M6f], params)
                    .size(40.0, 140.0)
                    .el(),
            )
            .spacing(20.0)
            .align_y(alignment::Vertical::Top)
            .into();

        let xy_row: Element<'a, Message<ZooMsg>> = Row::new()
            .push(
                xy_pad(P::KMix, P::KGain, params)
                    .label("small")
                    .size(80.0)
                    .el(),
            )
            .push(
                xy_pad(P::KFreq, P::KQ, params)
                    .label("med")
                    .size(130.0)
                    .el(),
            )
            .push(
                xy_pad(P::KPan, P::KPhase, params)
                    .label("big")
                    .size(200.0)
                    .el(),
            )
            .spacing(20.0)
            .align_y(alignment::Vertical::Top)
            .into();

        // Keyboard section: a text box (keys reach the focused widget) over
        // a label that mirrors the last key press from anywhere (the
        // subscription path).
        let keyboard_row: Element<'a, Message<ZooMsg>> = Column::new()
            .push(
                text_input("type here...", &self.textbox)
                    .on_input(|s| Message::Plugin(ZooMsg::TextChanged(s)))
                    .font(JETBRAINS_MONO)
                    .size(13)
                    .padding(6)
                    .width(Length::Fixed(360.0)),
            )
            .push(
                text(format!("last key: {}", self.last_key))
                    .size(12)
                    .font(JETBRAINS_MONO)
                    .color(HEADER_TEXT),
            )
            .spacing(8.0)
            .into();

        let body: Element<'a, Message<ZooMsg>> = Column::new()
            .push(section_label("Knobs"))
            .push(knobs_row)
            .push(section_label("Sliders"))
            .push(sliders_row)
            .push(section_label("Toggles"))
            .push(toggles_row)
            .push(section_label("Dropdown"))
            .push(dropdown_row)
            .push(section_label("Meters"))
            .push(meters_row)
            .push(section_label("XY Pads"))
            .push(xy_row)
            .push(section_label("Keyboard"))
            .push(keyboard_row)
            .spacing(gap)
            .padding(pad)
            .into();

        Column::new().push(header).push(body).into()
    }
}

fn section_label(label: &str) -> Element<'_, Message<ZooMsg>> {
    text(label)
        .size(11)
        .font(JETBRAINS_MONO)
        .color(HEADER_TEXT)
        .into()
}

// --- Plugin ---

pub struct ZooIced {
    params: Arc<ZooParams>,
}

impl ZooIced {
    #[must_use]
    pub fn new(params: Arc<ZooParams>) -> Self {
        Self { params }
    }
}

impl PluginLogic for ZooIced {
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
        // Passthrough.
        let n_in = buffer.num_input_channels();
        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            if ch < n_in {
                out.copy_from_slice(inp);
            } else {
                out.fill(0.0);
            }
        }

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
        IcedEditor::<ZooParams, ZooUi>::new(Arc::new(ZooParams::new()), (WINDOW_W, WINDOW_H))
            .with_meter_ids(vec![
                P::MIn,
                P::ML,
                P::MR,
                P::M6a,
                P::M6b,
                P::M6c,
                P::M6d,
                P::M6e,
                P::M6f,
            ])
            .with_font(truce_font::JETBRAINS_MONO)
            .into_editor()
    }
}

truce::plugin! {
    logic: ZooIced,
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
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_iced_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_iced_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_iced_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
