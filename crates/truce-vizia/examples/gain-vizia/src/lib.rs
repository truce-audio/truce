use truce::prelude::*;
use truce_vizia::ViziaEditor;

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

    #[param(name = "Bypass", short_name = "Byp",
            flags = "automatable | bypass")]
    pub bypass: BoolParam,

    #[meter]
    pub meter_left: MeterSlot,

    #[meter]
    pub meter_right: MeterSlot,
}

// --- Plugin ---

pub struct GainVizia {
    params: std::sync::Arc<GainParams>,
}

impl GainVizia {
    pub fn new(params: std::sync::Arc<GainParams>) -> Self {
        Self { params }
    }
}

impl PluginLogic for GainVizia {
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
            context.set_meter(P::MeterLeft, 0.0);
            context.set_meter(P::MeterRight, 0.0);
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
            context.set_meter(P::MeterLeft, buffer.output_peak(0));
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MeterRight, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        Some(Box::new(ViziaEditor::new((400, 300), gain_vizia_ui)))
    }
}

pub fn gain_vizia_ui(cx: &mut vizia::prelude::Context) {
    use truce_vizia::widgets::{ParamKnob, ParamSlider, ParamToggle, LevelMeter};
    use vizia::prelude::*;

    // Opt in to the truce dark theme. Omit this to use vizia's defaults
    // or provide your own stylesheet.
    truce_vizia::apply_default_theme(cx);

    VStack::new(cx, |cx| {
        // Header
        HStack::new(cx, |cx| {
            Label::new(cx, "Gain (vizia)").class("header-title");
        })
        .class("header");

        // Controls row
        HStack::new(cx, |cx| {
            ParamKnob::new(cx, P::Gain, "Gain");
            ParamKnob::new(cx, P::Pan, "Pan");
            ParamToggle::new(cx, P::Bypass, "Bypass");
            LevelMeter::new(cx, &[P::MeterLeft.into(), P::MeterRight.into()], "Level")
                .width(Pixels(24.0))
                .height(Pixels(50.0));
        })
        .horizontal_gap(Pixels(10.0))
        .height(Auto)
        .left(Pixels(10.0))
        .right(Pixels(10.0))
        .top(Pixels(10.0));

        // Separator
        Element::new(cx)
            .class("separator")
            .top(Pixels(8.0))
            .bottom(Pixels(8.0))
            .left(Pixels(10.0))
            .right(Pixels(10.0));

        // Sliders
        VStack::new(cx, |cx| {
            ParamSlider::new(cx, P::Gain, "Gain");
            ParamSlider::new(cx, P::Pan, "Pan")
                .top(Pixels(4.0));
        })
        .left(Pixels(10.0))
        .right(Pixels(10.0));
    });
}

truce::plugin! {
    logic: GainVizia,
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

}
