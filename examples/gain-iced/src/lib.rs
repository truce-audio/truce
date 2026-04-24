use std::sync::Arc;

use iced::widget::{container, text, Column, Row};
use iced::{alignment, Element, Font, Length};

const JETBRAINS_MONO: Font = Font {
    family: iced::font::Family::Name("JetBrains Mono"),
    ..Font::DEFAULT
};
const WINDOW_W: u32 = 176;
const WINDOW_H: u32 = 290;

use truce::prelude::*;
use truce_iced::{knob, meter, xy_pad, IcedEditor, IcedPlugin, IntoElement, Message, ParamState};

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

    fn view<'a>(&'a self, params: &'a ParamState<GainParams>) -> Element<'a, Message<GainMsg>> {
        let pad = 10.0;
        let gap = 10.0;

        let header: Element<'a, Message<GainMsg>> = container(
            text("GAIN (iced)")
                .size(14)
                .font(JETBRAINS_MONO)
                .color(iced::Color::from_rgb(0.75, 0.75, 0.80)),
        )
        .padding(iced::Padding::from([8.0, 10.0]))
        .width(Length::Fill)
        .style(|_theme: &iced::Theme| container::Style {
            background: Some(iced::Color::from_rgb(0.08, 0.08, 0.10).into()),
            ..Default::default()
        })
        .into();

        // Left column: knobs, XY pad
        let left: Element<'a, Message<GainMsg>> = Column::new()
            .push(
                Row::new()
                    .push(knob(P::Gain, params).label("Gain").size(60.0).el())
                    .push(knob(P::Pan, params).label("Pan").size(60.0).el())
                    .spacing(gap)
                    .align_y(alignment::Vertical::Center),
            )
            .push(
                xy_pad(P::Pan, P::Gain, params)
                    .label("Pan / Gain")
                    .size(130.0)
                    .el(),
            )
            .spacing(gap)
            .into();

        // Body: left column + meter spanning full height
        let body: Element<'a, Message<GainMsg>> = Row::new()
            .push(left)
            .push(
                meter(&[P::MeterLeft, P::MeterRight], params)
                    .size(16.0, 222.0)
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

pub struct GainIced {
    params: Arc<GainParams>,
}

impl GainIced {
    pub fn new(params: Arc<GainParams>) -> Self {
        Self { params }
    }
}

impl PluginLogic for GainIced {
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

    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        Some(Box::new(
            IcedEditor::<GainParams, GainUi>::new(
                Arc::new(GainParams::default_for_gui()),
                (WINDOW_W, WINDOW_H),
            )
            .with_meter_ids(vec![P::MeterLeft, P::MeterRight])
            .with_font("JetBrains Mono", truce_gui::font::JETBRAINS_MONO),
        ))
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
    fn gui_snapshot_iced() {
        let params = Arc::new(GainParams::new());
        let (pixels, w, h) = truce_iced::snapshot::render_iced_screenshot::<GainParams, GainUi>(
            params,
            (WINDOW_W, WINDOW_H),
            2.0,
            Some(("JetBrains Mono", truce_gui::font::JETBRAINS_MONO)),
        );
        truce_test::assert_gui_snapshot_raw("gain_iced_default", &pixels, w, h, 0);
    }
}
