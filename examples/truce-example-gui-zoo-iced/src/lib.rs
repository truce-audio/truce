//! iced counterpart of `truce-example-gui-zoo`. Same param shapes
//! (so every unit / range / discrete-snap path is exercised) but
//! laid out through iced's containers instead of the built-in
//! `GridLayout`.

use std::sync::Arc;

use truce_iced::iced::widget::{
    Column, Row, button, checkbox, container, image, progress_bar, radio, rule, scrollable, slider,
    text, text_input, toggler, tooltip, vertical_slider,
};
use truce_iced::iced::{Element, Font, Length, Task, alignment};

const JETBRAINS_MONO: Font = Font {
    family: truce_iced::iced::font::Family::Name("JetBrains Mono"),
    ..Font::DEFAULT
};
const WINDOW_W: u32 = 700;
const WINDOW_H: u32 = 900;
const HEADER_BG: truce_iced::iced::Color = truce_iced::iced::Color::from_rgb(0.08, 0.08, 0.10);
const HEADER_TEXT: truce_iced::iced::Color = truce_iced::iced::Color::from_rgb(0.75, 0.75, 0.80);
const BTN_TEXT: truce_iced::iced::Color = truce_iced::iced::Color::from_rgb(0.93, 0.93, 0.96);

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

/// Options for the radio buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Alpha,
    Beta,
    Gamma,
}

pub struct ZooUi {
    /// Contents of the keyboard-section text box (widget-path keyboard).
    textbox: String,
    /// Label for the most recent key press (subscription-path keyboard).
    last_key: String,
    // -- native iced widget state --
    counter: u32,
    checked: bool,
    toggled: bool,
    radio: Pick,
    slider: f32,
    vslider: f32,
    /// Decoded once - `Handle::from_rgba` mints a new id per call, so
    /// rebuilding it each frame would re-upload the texture every frame.
    carrot: image::Handle,
}

/// Decode the shared carrot.png (16x16 RGBA) to an iced image handle. A
/// plugin can't open a URL, so the image demo embeds the bytes.
fn decode_carrot() -> image::Handle {
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
    image::Handle::from_rgba(info.width, info.height, buf)
}

#[derive(Debug, Clone)]
pub enum ZooMsg {
    /// Text box edited (from the `text_input` widget).
    TextChanged(String),
    /// A key was pressed anywhere in the editor (from the subscription).
    KeyPressed(String),
    Increment,
    Checked(bool),
    Toggled(bool),
    Radio(Pick),
    Slider(f32),
    VSlider(f32),
}

/// Human-readable label for a key press: logical key (or typed text) plus
/// the layout-independent physical code.
fn key_label(
    key: &truce_iced::iced::keyboard::Key,
    physical: truce_iced::iced::keyboard::key::Physical,
    text: Option<&str>,
) -> String {
    use truce_iced::iced::keyboard::Key;
    let logical = match key {
        Key::Character(c) => format!("'{c}'"),
        Key::Named(named) => format!("{named:?}"),
        Key::Unidentified => "Unidentified".to_string(),
    };
    let phys = match physical {
        truce_iced::iced::keyboard::key::Physical::Code(code) => format!("{code:?}"),
        truce_iced::iced::keyboard::key::Physical::Unidentified(_) => "?".to_string(),
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
            counter: 0,
            checked: true,
            toggled: false,
            radio: Pick::Alpha,
            slider: 0.4,
            vslider: 0.6,
            carrot: decode_carrot(),
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
                ZooMsg::Increment => self.counter += 1,
                ZooMsg::Checked(b) => self.checked = b,
                ZooMsg::Toggled(b) => self.toggled = b,
                ZooMsg::Radio(p) => self.radio = p,
                ZooMsg::Slider(v) => self.slider = v,
                ZooMsg::VSlider(v) => self.vslider = v,
            }
        }
        Task::none()
    }

    fn subscription(&self) -> truce_iced::iced::Subscription<Message<ZooMsg>> {
        // Mirror every key press, regardless of which widget has focus, by
        // listening to the raw event stream (the truce-iced subscription
        // pump drives this). Returning `None` skips releases / modifier
        // changes so only presses update the label.
        truce_iced::iced::event::listen_with(|event, _status, _window| match event {
            truce_iced::iced::Event::Keyboard(truce_iced::iced::keyboard::Event::KeyPressed {
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
        .padding(truce_iced::iced::Padding::from([8.0, 10.0]))
        .width(Length::Fill)
        .style(|_theme: &truce_iced::iced::Theme| container::Style {
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

        let body: Element<'a, Message<ZooMsg>> = Column::new()
            .push(section_label("Knobs"))
            .push(knobs_row)
            .push(section_label("Sliders"))
            .push(sliders_row)
            // Pair short sections side by side so the native-widget section
            // below lands in the default (unscrolled) view.
            .push(
                Row::new()
                    .push(labeled("Toggles", toggles_row))
                    .push(labeled("Dropdown", dropdown_row))
                    .spacing(40.0),
            )
            .push(
                Row::new()
                    .push(labeled("Meters", meters_row))
                    .push(labeled("XY Pads", xy_row))
                    .spacing(40.0),
            )
            .push(section_label("iced Widgets"))
            .push(self.widgets_section())
            .push(section_label("Keyboard"))
            .push(self.keyboard_section())
            .spacing(gap)
            .padding(pad)
            .into();

        // Scrollable so the growing zoo fits any window height.
        Column::new()
            .push(header)
            .push(scrollable(body).height(Length::Fill))
            .into()
    }
}

impl ZooUi {
    /// Native iced widgets: button, tooltip, checkbox, toggler, radio,
    /// image, slider, `vertical_slider`, `progress_bar`, rule.
    fn widgets_section(&self) -> Element<'_, Message<ZooMsg>> {
        let on_inc = || Message::Plugin(ZooMsg::Increment);
        Column::new()
            .push(
                Row::new()
                    .push(
                        button(
                            text(format!("clicked {}x", self.counter))
                                .size(12)
                                .font(JETBRAINS_MONO)
                                .color(BTN_TEXT),
                        )
                        .on_press(on_inc()),
                    )
                    .push(tooltip(
                        button(
                            text("hover me")
                                .size(12)
                                .font(JETBRAINS_MONO)
                                .color(BTN_TEXT),
                        )
                        .on_press(on_inc()),
                        container(text("a tooltip").size(11)).padding(6).style(
                            |_t: &truce_iced::iced::Theme| container::Style {
                                background: Some(HEADER_BG.into()),
                                ..Default::default()
                            },
                        ),
                        tooltip::Position::Top,
                    ))
                    .push(
                        checkbox(self.checked)
                            .label("checkbox")
                            .on_toggle(|b| Message::Plugin(ZooMsg::Checked(b))),
                    )
                    .push(
                        toggler(self.toggled)
                            .label("toggler")
                            .on_toggle(|b| Message::Plugin(ZooMsg::Toggled(b))),
                    )
                    .spacing(16.0)
                    .align_y(alignment::Vertical::Center),
            )
            .push(
                Row::new()
                    .push(radio("Alpha", Pick::Alpha, Some(self.radio), |p| {
                        Message::Plugin(ZooMsg::Radio(p))
                    }))
                    .push(radio("Beta", Pick::Beta, Some(self.radio), |p| {
                        Message::Plugin(ZooMsg::Radio(p))
                    }))
                    .push(radio("Gamma", Pick::Gamma, Some(self.radio), |p| {
                        Message::Plugin(ZooMsg::Radio(p))
                    }))
                    // NEAREST keeps the 16x16 pixel art crisp when scaled up.
                    .push(
                        image(self.carrot.clone())
                            .filter_method(image::FilterMethod::Nearest)
                            .width(Length::Fixed(48.0))
                            .height(Length::Fixed(48.0)),
                    )
                    .spacing(16.0)
                    .align_y(alignment::Vertical::Center),
            )
            .push(
                Row::new()
                    .push(
                        slider(0.0..=1.0, self.slider, |v| {
                            Message::Plugin(ZooMsg::Slider(v))
                        })
                        // iced's slider defaults `step` to 1.0, which would
                        // snap a 0..1 range to just its two ends.
                        .step(0.01)
                        .width(Length::Fixed(200.0)),
                    )
                    .push(
                        vertical_slider(0.0..=1.0, self.vslider, |v| {
                            Message::Plugin(ZooMsg::VSlider(v))
                        })
                        .step(0.01)
                        .height(Length::Fixed(60.0)),
                    )
                    .push(
                        progress_bar(0.0..=1.0, self.slider)
                            .length(Length::Fixed(160.0))
                            .girth(Length::Fixed(16.0)),
                    )
                    .spacing(16.0)
                    .align_y(alignment::Vertical::Center),
            )
            .push(rule::horizontal(1.0))
            .spacing(10.0)
            .into()
    }

    /// Keyboard demo: a text box (keys reach the focused widget) over a
    /// label that mirrors the last key press from anywhere (subscription).
    fn keyboard_section(&self) -> Element<'_, Message<ZooMsg>> {
        Column::new()
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
            .into()
    }
}

fn section_label(label: &str) -> Element<'_, Message<ZooMsg>> {
    text(label)
        .size(11)
        .font(JETBRAINS_MONO)
        .color(HEADER_TEXT)
        .into()
}

/// A titled sub-section (label over content), for placing two sections in
/// one row.
fn labeled<'a>(
    title: &'a str,
    content: Element<'a, Message<ZooMsg>>,
) -> Element<'a, Message<ZooMsg>> {
    Column::new()
        .push(section_label(title))
        .push(content)
        .spacing(10.0)
        .into()
}

// --- Plugin ---

/// Stateless descriptor - passthrough carries no DSP state, only params.
pub struct ZooIced;

impl PluginLogic for ZooIced {
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

    fn editor(params: Arc<ZooParams>) -> Box<dyn Editor> {
        IcedEditor::<ZooParams, ZooUi>::new(params, (WINDOW_W, WINDOW_H))
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
    fn carrot_decodes_to_an_rgba_handle() {
        // `decode_carrot` panics if the embedded png is missing/corrupt.
        let handle = decode_carrot();
        let image::Handle::Rgba {
            width,
            height,
            pixels,
            ..
        } = handle
        else {
            panic!("expected an RGBA handle");
        };
        assert_eq!((width, height), (16, 16));
        assert_eq!(pixels.len(), 16 * 16 * 4);
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
