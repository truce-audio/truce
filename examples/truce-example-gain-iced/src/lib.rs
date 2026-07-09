use std::sync::Arc;

use truce_iced::iced::widget::{Column, Row, container, text};
use truce_iced::iced::{Alignment, Element, Font, Length, alignment};

const JETBRAINS_MONO: Font = Font {
    family: truce_iced::iced::font::Family::Name("JetBrains Mono"),
    ..Font::DEFAULT
};
const WINDOW_W: u32 = 176;
const WINDOW_H: u32 = 290;

use truce::prelude::*;
use truce_iced::{IcedEditor, IcedPlugin, IntoElement, Message, ParamCache, knob, meter, xy_pad};

// --- Parameters ---

use GainParamsParamId as P;

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

// --- Custom iced UI ---

pub struct GainUi;

#[derive(Debug, Clone)]
pub enum GainMsg {}

impl IcedPlugin<GainParams> for GainUi {
    type Message = GainMsg;

    fn new(_params: Arc<GainParams>) -> Self {
        Self
    }

    fn view<'a>(&'a self, params: &'a ParamCache<GainParams>) -> Element<'a, Message<GainMsg>> {
        let pad = 10.0;
        let gap = 10.0;

        let header: Element<'a, Message<GainMsg>> = container(
            text("GAIN (iced)")
                .size(14)
                .font(JETBRAINS_MONO)
                .color(truce_iced::iced::Color::from_rgb(0.75, 0.75, 0.80)),
        )
        .padding(truce_iced::iced::Padding::from([8.0, 10.0]))
        .width(Length::Fill)
        .style(|_theme: &truce_iced::iced::Theme| container::Style {
            background: Some(truce_iced::iced::Color::from_rgb(0.08, 0.08, 0.10).into()),
            ..Default::default()
        })
        .into();

        // Control column on the left: knob row (fixed height,
        // centred horizontally so the two knobs stay grouped
        // under the XY pad) over an XY pad that stretches to
        // fill the remaining area in both axes.
        // The knob row is wrapped in a `container` so it can
        // span the column width and centre its content -
        // `truce_iced::iced::widget::Row` exposes `align_y` (cross-axis) but
        // not `align_x`, so centring the children horizontally
        // needs a container with `.align_x(Center)`.
        let knob_row: Element<'a, Message<GainMsg>> = container(
            Row::new()
                .push(knob(P::Gain, params).label("Gain").size(60.0).el())
                .push(knob(P::Pan, params).label("Pan").size(60.0).el())
                .spacing(gap)
                .align_y(alignment::Vertical::Center),
        )
        .width(Length::Fill)
        .align_x(Alignment::Center)
        .into();

        let controls: Element<'a, Message<GainMsg>> = Column::new()
            .push(knob_row)
            .push(
                xy_pad(P::Pan, P::Gain, params)
                    .label("Pan / Gain")
                    .fill()
                    .el(),
            )
            .spacing(gap)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        // Body: control column on the left takes the remaining
        // width; meter pinned to the right at its natural width,
        // stretching vertically.
        let body: Element<'a, Message<GainMsg>> = Row::new()
            .push(controls)
            .push(
                meter(&[P::MeterLeft, P::MeterRight], params)
                    .width(Length::Fixed(16.0))
                    .fill()
                    .el(),
            )
            .spacing(gap)
            .padding(pad)
            .align_y(alignment::Vertical::Top)
            .into();

        Column::new().push(header).push(body).into()
    }
}

// --- Plugin ---

/// Stateless descriptor - carries no DSP state, only params.
pub struct GainIced;

impl PluginLogic for GainIced {
    type Params = GainParams;
    type DspState = ();

    fn init(_params: &GainParams) {}

    fn process(
        _state: &mut (),
        params: &GainParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        for i in 0..buffer.num_samples() {
            let gain_db = params.gain.read();
            let pan = params.pan.read();
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
        IcedEditor::<GainParams, GainUi>::new(params, (WINDOW_W, WINDOW_H))
            .with_meter_ids(vec![P::MeterLeft, P::MeterRight])
            .with_font(truce_font::JETBRAINS_MONO)
            // Header strip + content area; iced's existing
            // `Length::Fill` columns let widgets stretch
            // naturally as the window grows.
            .resizable(true)
            // 176 px = two 60 px knobs + 10 px gap + 16 px meter +
            // 10 px column gap + 10 px padding on each side; the
            // smallest width where the XY pad column matches the
            // knob row above.
            .min_size((176, 260))
            .max_size((1200, 900))
            .into_editor()
    }
}

truce::plugin! {
    logic: GainIced,
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
        truce_test::screenshot!(Plugin, "screenshots/gain_iced_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/gain_iced_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/gain_iced_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
