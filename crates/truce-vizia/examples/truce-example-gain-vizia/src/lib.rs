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
            // Host-drag resize: the wrapper installs the
            // `WidthSizable | HeightSizable` autoresize mask, the
            // host's drag handle grows the parent NSView, baseview's
            // `setFrameSize:` override fires `Resized`, and
            // vizia_baseview's `Resized` handler reconfigures its
            // skia surface + calls `cx.set_window_size`, which
            // re-runs vizia's layout engine against the new root
            // dimensions. CLAP `gui_set_size` / VST3 `IPlugView::onSize`
            // round-trips that don't accompany a parent resize are a
            // no-op pending the vizia_baseview upstream resize-API
            // patch tracked in `crates/truce-vizia/src/editor.rs`.
            .resizable(true)
            .min_size((176, 240))
            .max_size((1200, 900))
            .into_editor()
    }
}

#[allow(clippy::needless_pass_by_value)]
fn gain_view(cx: &mut Context, lens: ParamLens<GainParams>) {
    let lens_for_meter = lens.clone();
    HStack::new(cx, move |cx| {
        VStack::new(cx, move |cx| {
            // Knob row: take the column width so we have room to
            // centre the knobs as the editor grows, but pack the
            // knobs themselves at their natural width (no
            // Stretch-wrapping each cell). `alignment: center`
            // keeps the pair grouped under the XY pad below
            // instead of letting them drift apart with the column
            // - matches the egui / iced / slint gain examples.
            HStack::new(cx, |cx| {
                param_knob(cx, lens.clone(), P::Gain, "Gain");
                param_knob(cx, lens.clone(), P::Pan, "Pan");
            })
            .width(Stretch(1.0))
            .height(Auto)
            .horizontal_gap(Pixels(10.0))
            .alignment(Alignment::TopCenter);
            // XY pad stretches in both axes so it fills the column
            // below the (fixed-height) knob row as the editor
            // window grows.
            param_xy_pad(
                cx,
                lens.clone(),
                P::Pan,
                P::Gain,
                "Pan / Gain",
                Stretch(1.0),
                Stretch(1.0),
            );
        })
        .width(Stretch(1.0))
        .height(Stretch(1.0))
        .vertical_gap(Pixels(13.0));

        // Meter on the right: narrow band, stretches vertically
        // with the editor frame.
        level_meter(
            cx,
            lens_for_meter.clone(),
            &[P::MeterLeft, P::MeterRight],
            Stretch(1.0),
        );
    })
    // Outer row fills the editor; stretch children inside (knob
    // column, XY pad, meter) divide the remaining space relative
    // to each other.
    .width(Stretch(1.0))
    .height(Stretch(1.0))
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
