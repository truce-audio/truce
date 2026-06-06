//! vizia counterpart of `truce-example-gui-zoo`. Same param shapes
//! (so every unit / range / discrete-snap path is exercised) but
//! laid out through vizia containers instead of the built-in
//! `GridLayout`. Layout / widget regressions in `truce-vizia`
//! surface here before they reach plugins that consume it.

#![cfg(not(target_os = "ios"))]

use std::sync::Arc;

use truce::prelude::*;
use truce_font::JETBRAINS_MONO;
use truce_vizia::vizia::prelude::*;
use truce_vizia::widgets::{
    self, level_meter, param_dropdown, param_knob, param_slider, param_toggle, param_xy_pad,
};
use truce_vizia::{ParamLens, ViziaEditor};

use ZooParamsParamId as P;

const WINDOW_W: u32 = 604;
const WINDOW_H: u32 = 920;

// Zoo-local layout CSS. Lives in the example because it's
// zoo-specific (section header band, inter-row gap, section title
// styling) and shouldn't bleed into plugins that consume the
// truce-vizia widgets. Pairs with `widgets::BASE_CSS` which carries
// the vizia compatibility shims the widgets need.
const ZOO_CSS: &str = r"
.zoo-root {
    width: 1s;
    height: 1s;
}
.zoo-body {
    padding: 10px;
    vertical-gap: 12px;
}
.zoo-section-title {
    font-size: 11px;
    font-weight: bold;
    padding-top: 4px;
    padding-bottom: 2px;
}
.zoo-section-row {
    height: auto;
    horizontal-gap: 14px;
    alignment: top left;
}
";

fn section(cx: &mut Context, title: &str) {
    Label::new(cx, title.to_string()).class("zoo-section-title");
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
    // formatter + range-parser path through the vizia widgets. --
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

pub struct ZooVizia {
    params: Arc<ZooParams>,
}

impl ZooVizia {
    #[must_use]
    pub fn new(params: Arc<ZooParams>) -> Self {
        Self { params }
    }
}

impl PluginLogic for ZooVizia {
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
        // `widgets::BASE_CSS` is the truce-vizia compat shim; ZOO_CSS
        // is the zoo's own layout (section bands, gaps). No custom
        // palette - the zoo renders against vizia's default light
        // theme.
        ViziaEditor::new(self.params.clone(), (WINDOW_W, WINDOW_H), zoo_view)
            .with_stylesheet(widgets::BASE_CSS)
            .with_stylesheet(ZOO_CSS)
            .with_font(JETBRAINS_MONO)
            .into_editor()
    }
}

fn zoo_view(cx: &mut Context, lens: ParamLens<ZooParams>) {
    VStack::new(cx, move |cx| {
        VStack::new(cx, move |cx| {
            section(cx, "Knobs");
            HStack::new(cx, |cx| {
                param_knob(cx, lens.clone(), P::KMix, "Mix");
                param_knob(cx, lens.clone(), P::KGain, "Gain");
                param_knob(cx, lens.clone(), P::KFreq, "Freq");
                param_knob(cx, lens.clone(), P::KQ, "Q");
                param_knob(cx, lens.clone(), P::KPhase, "Phase");
                param_knob(cx, lens.clone(), P::KPitch, "Pitch");
                param_knob(cx, lens.clone(), P::KTime, "Time");
                param_knob(cx, lens.clone(), P::KRelease, "Rel");
                param_knob(cx, lens.clone(), P::KPan, "Pan");
            })
            .class("zoo-section-row");

            section(cx, "Sliders");
            HStack::new(cx, |cx| {
                param_slider(cx, lens.clone(), P::SFloat, "Float", 120.0);
                param_slider(cx, lens.clone(), P::SInt, "Int", 120.0);
                param_slider(cx, lens.clone(), P::SWide, "Wide (dB)", 280.0);
            })
            .class("zoo-section-row");

            section(cx, "Toggles");
            HStack::new(cx, |cx| {
                param_toggle(cx, lens.clone(), P::TOn, "On");
                param_toggle(cx, lens.clone(), P::TOff, "Off");
            })
            .class("zoo-section-row");

            section(cx, "Dropdowns");
            HStack::new(cx, |cx| {
                // Vary the per-option width arg so the difference is
                // visible at a glance: a tight 70px single-column
                // dropdown next to a wide 120px two-column one.
                param_dropdown(cx, lens.clone(), P::Mode, "Mode", 8, 1, 70.0);
                param_dropdown(cx, lens.clone(), P::ModeWide, "Mode Wide", 8, 2, 120.0);
            })
            .class("zoo-section-row");

            section(cx, "Meters");
            HStack::new(cx, |cx| {
                level_meter(cx, lens.clone(), &[P::MIn], 140.0);
                level_meter(cx, lens.clone(), &[P::ML, P::MR], 140.0);
                level_meter(
                    cx,
                    lens.clone(),
                    &[P::M6a, P::M6b, P::M6c, P::M6d, P::M6e, P::M6f],
                    140.0,
                );
            })
            .class("zoo-section-row");

            section(cx, "XY Pads");
            HStack::new(cx, |cx| {
                param_xy_pad(cx, lens.clone(), P::KMix, P::KGain, "small", 80.0, 80.0);
                param_xy_pad(cx, lens.clone(), P::KFreq, P::KQ, "med", 130.0, 130.0);
                param_xy_pad(cx, lens.clone(), P::KPan, P::KPhase, "big", 200.0, 200.0);
            })
            .class("zoo-section-row");
        })
        .class("zoo-body");
    })
    .class("zoo-root");
}

truce::plugin! {
    logic: ZooVizia,
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
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_vizia_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_vizia_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_vizia_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
