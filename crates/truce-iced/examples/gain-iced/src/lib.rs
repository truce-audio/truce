use std::sync::Arc;

use iced::widget::{container, horizontal_space, text, Column, Row};
use iced::{alignment, Element, Length, Task};
use truce::prelude::*;
use truce_iced::{
    knob, meter, param_slider, param_toggle, xy_pad, EditorHandle, IcedEditor, IcedPlugin,
    Message, ParamState,
};

// --- Parameters ---

// The #[derive(Params)] macro generates `GainParamsParamId` enum:
//   GainParamsParamId::Gain = 0
//   GainParamsParamId::Pan = 1
//   GainParamsParamId::Bypass = 2
// with Into<u32> so it works everywhere u32 param IDs are accepted.
use GainParamsParamId as P;

/// Meter IDs (separate from param IDs).
#[repr(u32)]
#[derive(Clone, Copy)]
pub enum Meter {
    Left = 100,
    Right = 101,
}

impl From<Meter> for u32 {
    fn from(m: Meter) -> u32 {
        m as u32
    }
}

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

// --- Custom iced UI ---

pub struct GainUi;

#[derive(Debug, Clone)]
pub enum GainMsg {}

impl IcedPlugin<GainParams> for GainUi {
    type Message = GainMsg;

    fn new(_params: Arc<GainParams>) -> Self {
        Self
    }

    fn update(
        &mut self,
        _message: Message<GainMsg>,
        _params: &ParamState<GainParams>,
        _ctx: &EditorHandle,
    ) -> Task<Message<GainMsg>> {
        Task::none()
    }

    fn view<'a>(
        &'a self,
        params: &'a ParamState<GainParams>,
    ) -> Element<'a, Message<GainMsg>> {
        let pad = 8.0;
        let gap = 8.0;

        // Header: title left, bypass right
        let header: Element<'a, Message<GainMsg>> = container(
            Row::new()
                .push(
                    text("GAIN (iced)")
                        .size(14)
                        .color(iced::Color::from_rgb(0.75, 0.75, 0.80)),
                )
                .push(horizontal_space())
                .push(Into::<Element<'a, Message<GainMsg>>>::into(
                    param_toggle(P::Bypass, params).label("Bypass"),
                ))
                .align_y(alignment::Vertical::Center),
        )
        .padding(pad)
        .width(Length::Fill)
        .style(|_theme: &iced::Theme| container::Style {
            background: Some(iced::Color::from_rgb(0.08, 0.08, 0.10).into()),
            ..Default::default()
        })
        .into();

        // Top row: knob + slider
        let top_row: Element<'a, Message<GainMsg>> = Row::new()
            .push(Into::<Element<'a, Message<GainMsg>>>::into(
                knob(P::Gain, params).label("Gain").size(60.0),
            ))
            .push(Into::<Element<'a, Message<GainMsg>>>::into(
                param_slider(P::Pan, params).label("Pan").width(120.0),
            ))
            .spacing(gap)
            .align_y(alignment::Vertical::Center)
            .into();

        // Bottom row: XY pad + meter
        let bottom_row: Element<'a, Message<GainMsg>> = Row::new()
            .push(Into::<Element<'a, Message<GainMsg>>>::into(
                xy_pad(P::Pan, P::Gain, params).label("Pan / Gain").size(130.0),
            ))
            .push(Into::<Element<'a, Message<GainMsg>>>::into(
                meter(&[Meter::Left.into(), Meter::Right.into()], params)
                    .label("Level")
                    .size(24.0, 130.0),
            ))
            .spacing(gap)
            .align_y(alignment::Vertical::Top)
            .into();

        // Footer
        let footer: Element<'a, Message<GainMsg>> = text(format!(
            "Gain: {}  Pan: {}",
            params.label(P::Gain),
            params.label(P::Pan),
        ))
        .size(10)
        .color(iced::Color::from_rgb(0.55, 0.55, 0.60))
        .width(Length::Fill)
        .align_x(alignment::Horizontal::Center)
        .into();

        Column::new()
            .push(header)
            .push(top_row)
            .push(bottom_row)
            .push(footer)
            .spacing(gap)
            .padding(iced::Padding {
                top: 0.0,
                right: pad,
                bottom: pad,
                left: pad,
            })
            .into()
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

    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        Some(Box::new(
            IcedEditor::<GainParams, GainUi>::new(
                Arc::new(GainParams::default_for_gui()),
                (250, 330),
            )
            .with_meter_ids(vec![Meter::Left.into(), Meter::Right.into()]),
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
        let pixels =
            truce_iced::snapshot::render_iced_screenshot::<GainParams, GainUi>(
                params,
                (250, 330),
            );
        truce_test::assert_gui_snapshot_raw("gain_iced_default", &pixels, 250, 330, 0);
    }
}
