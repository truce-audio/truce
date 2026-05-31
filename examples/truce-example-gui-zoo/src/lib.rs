//! GUI zoo: a passthrough plugin whose only job is to exercise every
//! built-in widget type across a variety of spans and grid positions.
//! Layout / widget regressions surface here before they reach the
//! real example plugins.

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{
    GridLayout, dropdown, knob, meter, section, selector, slider, toggle, xy_pad,
};

use ZooParamsParamId as P;
use std::sync::Arc;

// Enums driving the selector / dropdown widgets. The dropdown's enum
// is intentionally long enough to exercise the popup's scroll path.

#[derive(ParamEnum)]
pub enum Shape {
    Sine,
    Triangle,
    Square,
    Sawtooth,
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
    // -- Knobs (1x1 default, plus wide + tall variants) --
    #[param(name = "K1", range = "linear(0, 1)")]
    pub k1: FloatParam,
    #[param(name = "K2", range = "linear(0, 1)")]
    pub k2: FloatParam,
    #[param(name = "K3", range = "linear(0, 1)")]
    pub k3: FloatParam,
    #[param(name = "K4", range = "linear(0, 1)")]
    pub k4: FloatParam,
    #[param(name = "Wide", range = "linear(0, 1)")]
    pub k_wide: FloatParam,
    #[param(name = "Tall", range = "linear(0, 1)")]
    pub k_tall: FloatParam,
    #[param(name = "Big", range = "linear(0, 1)")]
    pub k_big: FloatParam,

    // -- Sliders (float, int, and a wide variant) --
    #[param(name = "Float", range = "linear(0, 100)", unit = "%")]
    pub s_float: FloatParam,
    #[param(name = "Int", range = "discrete(0, 10)")]
    pub s_int: IntParam,
    #[param(name = "Wide", range = "linear(-60, 6)", unit = "dB")]
    pub s_wide: FloatParam,

    // -- Toggles (default-on and default-off) --
    #[param(name = "On", default = true)]
    pub t_on: BoolParam,
    #[param(name = "Off")]
    pub t_off: BoolParam,

    // -- Selector (cycle) + Dropdown (popup) --
    #[param(name = "Shape")]
    pub shape: EnumParam<Shape>,
    #[param(name = "Mode")]
    pub mode: EnumParam<Mode>,

    // -- Meters: single-channel and stereo pair --
    #[meter]
    pub m_in: MeterSlot,
    #[meter]
    pub m_l: MeterSlot,
    #[meter]
    pub m_r: MeterSlot,
}

pub struct Zoo {
    params: Arc<ZooParams>,
}

impl Zoo {
    #[must_use]
    pub fn new(params: Arc<ZooParams>) -> Self {
        Self { params }
    }
}

impl PluginLogic for Zoo {
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
        // Passthrough: copy in -> out per channel. The zoo doesn't
        // alter the signal; it just renders widgets.
        let n_in = buffer.num_input_channels();
        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            if ch < n_in {
                out.copy_from_slice(inp);
            } else {
                out.fill(0.0);
            }
        }

        // Drive meters from the output peak (passthrough, so in == out).
        // Meters animate when audio flows; the zoo is also a meter-
        // rendering test.
        if buffer.num_output_channels() >= 1 {
            let p = buffer.output_peak(0);
            context.set_meter(P::MIn, p);
            context.set_meter(P::ML, p);
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MR, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn editor(&self) -> Box<dyn Editor> {
        GridLayout::build(vec![
            section(
                "Knobs",
                vec![
                    knob(P::K1, "1x1"),
                    knob(P::K2, "1x1"),
                    knob(P::K3, "1x1"),
                    knob(P::K4, "1x1"),
                    knob(P::KWide, "2x1").cols(2),
                    knob(P::KTall, "1x2").rows(2),
                    knob(P::KBig, "2x2").cols(2).rows(2),
                ],
            ),
            section(
                "Sliders",
                vec![
                    slider(P::SFloat, "Float"),
                    slider(P::SInt, "Int"),
                    slider(P::SWide, "Wide").cols(2),
                ],
            ),
            section(
                "Toggles & Selector",
                vec![
                    toggle(P::TOn, "On"),
                    toggle(P::TOff, "Off"),
                    selector(P::Shape, "Cycle").cols(2),
                ],
            ),
            section("Dropdown", vec![dropdown(P::Mode, "Popup").cols(2)]),
            section(
                "Meters",
                vec![
                    meter(&[P::MIn], "Input").rows(2),
                    meter(&[P::ML, P::MR], "L / R").rows(2),
                ],
            ),
            section(
                "XY Pad",
                vec![
                    xy_pad(P::K1, P::K2, "2x2"),
                    xy_pad(P::K3, P::K4, "3x3").cols(3).rows(3),
                ],
            ),
        ])
        .with_cols(6)
        .with_title("GUI ZOO")
        .with_subtitle("widget reference")
        .into_editor(&self.params)
    }
}

truce::plugin! {
    logic: Zoo,
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
        truce_test::screenshot!(Plugin, "screenshots/zoo_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/zoo_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/zoo_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
