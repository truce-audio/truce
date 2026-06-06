//! Vizia gain example for truce. Mirrors `truce-example-gain-egui`'s
//! shape: Gain + Pan knobs, a Pan/Gain XY pad, and a stereo level
//! meter, all wired through `truce_vizia::ParamLens` so widgets
//! sharing a param id stay in sync (knob ↔ XY pad).

#![cfg(not(target_os = "ios"))]

use std::sync::Arc;

use truce::prelude::*;
use truce_font::JETBRAINS_MONO;
use truce_vizia::vizia::prelude::*;
use truce_vizia::widgets::{self, level_meter, param_knob, param_xy_pad};
use truce_vizia::{ParamLens, ViziaEditor};

use GainParamsParamId as P;

const WINDOW_W: u32 = 176;
const WINDOW_H: u32 = 260;

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

pub struct GainVizia {
    params: Arc<GainParams>,
}

impl GainVizia {
    #[must_use]
    pub fn new(params: Arc<GainParams>) -> Self {
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
        for i in 0..buffer.num_samples() {
            let gain_db = self.params.gain.read();
            let pan = self.params.pan.read();
            let gain_linear = db_to_linear(gain_db);

            // Linear pan: left attenuates when pan > 0 (right), right
            // attenuates when pan < 0 (left). Identical to the egui
            // gain example's DSP so the screenshot baselines stay
            // comparable across backends.
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

    fn editor(&self) -> Box<dyn Editor> {
        ViziaEditor::new(self.params.clone(), (WINDOW_W, WINDOW_H), gain_view)
            .with_stylesheet(widgets::BASE_CSS)
            .with_font(JETBRAINS_MONO)
            .into_editor()
    }
}

#[allow(clippy::needless_pass_by_value)]
fn gain_view(cx: &mut Context, lens: ParamLens<GainParams>) {
    let lens_for_meter = lens.clone();
    HStack::new(cx, move |cx| {
        VStack::new(cx, move |cx| {
            HStack::new(cx, |cx| {
                // Wrap each knob in a Stretch-width HStack so the two
                // cells split the row equally. The row width matches
                // the Pan / Gain pad below (130px) so the knob row
                // aligns visually with the pad's left and right edges.
                HStack::new(cx, |cx| {
                    param_knob(cx, lens.clone(), P::Gain, "Gain");
                })
                .width(Stretch(1.0))
                .height(Auto)
                .alignment(Alignment::Center);
                HStack::new(cx, |cx| {
                    param_knob(cx, lens.clone(), P::Pan, "Pan");
                })
                .width(Stretch(1.0))
                .height(Auto)
                .alignment(Alignment::Center);
            })
            .width(Pixels(130.0))
            .height(Auto);
            // XY pad: fixed 130x130 today because vizia's resize
            // path is upstream-blocked on a `vizia_baseview`
            // patch. Once that lands the pad will swap to
            // `Stretch(1.0)` for both axes so it scales with the
            // editor window.
            param_xy_pad(
                cx,
                lens.clone(),
                P::Pan,
                P::Gain,
                "Pan / Gain",
                130.0,
                130.0,
            );
        })
        .width(Auto)
        .height(Auto)
        .vertical_gap(Pixels(13.0));

        // Meter on the right, lined up with the control column's
        // overall height.
        level_meter(
            cx,
            lens_for_meter.clone(),
            &[P::MeterLeft, P::MeterRight],
            240.0,
        );
    })
    .padding(Pixels(10.0))
    .horizontal_gap(Pixels(10.0))
    .alignment(Alignment::TopLeft);
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
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
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
    fn state_round_trips() {
        truce_test::assert_state_round_trip::<Plugin>();
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

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/gain_vizia_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/gain_vizia_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/gain_vizia_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
