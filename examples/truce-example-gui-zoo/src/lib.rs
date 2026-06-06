//! GUI zoo: a passthrough plugin whose only job is to exercise every
//! built-in widget type across a variety of spans and grid positions.
//! Layout / widget regressions surface here before they reach the
//! real example plugins.

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, dropdown, knob, meter, section, slider, toggle, xy_pad};

use ZooParamsParamId as P;
use std::sync::Arc;

// Enums driving the dropdown widgets. `Mode` is intentionally long
// enough to exercise the popup's scroll path.

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
    // -- Knobs (mixed ranges, units, and defaults so the zoo exercises
    // every range parser + unit formatter path) --
    // `Percent` formatter multiplies by 100; range must stay [0, 1].
    #[param(name = "Mix", range = "linear(0, 1)", default = 0.5, unit = "%")]
    pub k1: FloatParam,
    #[param(name = "Gain", range = "linear(-60, 6)", default = 0, unit = "dB")]
    pub k2: FloatParam,
    #[param(name = "Freq", range = "log(20, 20000)", default = 1000, unit = "Hz")]
    pub k3: FloatParam,
    #[param(name = "Drive", range = "linear(0, 1)", default = 0.25)]
    pub k4: FloatParam,
    #[param(name = "Pan", range = "linear(-1, 1)", default = 0, unit = "pan")]
    pub k_wide: FloatParam,
    #[param(name = "Depth", range = "linear(0, 1)", default = 0.75)]
    pub k_tall: FloatParam,
    // Exercises the `deg` unit. Default lands at center of 0..360.
    #[param(name = "Phase", range = "linear(0, 360)", default = 180, unit = "deg")]
    pub k_big: FloatParam,
    // Filler 1x1 knobs - auto-flow drops these into the 3x2 hole to
    // the right of KBig (cols 3-5, rows 1-2) so the wide / tall /
    // big variants don't leave a giant blank patch on row 1-2.
    // `default = std::f64::consts::PI` exercises the 0.49.18 / 0.49.19
    // const-path parser. PI ≈ 3.14 lands inside the log(0.1, 20) Q range.
    #[param(name = "Q", range = "log(0.1, 20)", default = std::f64::consts::PI)]
    pub k5: FloatParam,
    #[param(name = "Pitch", range = "discrete(-12, 12)", default = 0, unit = "st")]
    pub k6: IntParam,
    #[param(name = "Time", range = "linear(0, 1000)", default = 200, unit = "ms")]
    pub k7: FloatParam,
    #[param(name = "Trim", range = "linear(-12, 12)", default = 0, unit = "dB")]
    pub k8: FloatParam,
    #[param(name = "Release", range = "linear(0, 10)", default = 1.5, unit = "s")]
    pub k9: FloatParam,
    #[param(name = "Hi", range = "linear(0, 1)", default = 0.66)]
    pub k10: FloatParam,

    // -- Sliders (float, int, a knob between, and a wide variant) --
    #[param(name = "Float", range = "linear(0, 1)", unit = "%")]
    pub s_float: FloatParam,
    #[param(name = "Int", range = "discrete(0, 10)")]
    pub s_int: IntParam,
    // Mid-row knob - inserted between the slim sliders and the wide
    // slider to exercise mixed-widget-kind row layout.
    #[param(name = "Mid", range = "linear(0, 1)", default = 0.5)]
    pub k_mid: FloatParam,
    #[param(name = "Wide", range = "linear(-60, 6)", unit = "dB")]
    pub s_wide: FloatParam,

    // -- Toggles (default-on and default-off) --
    #[param(name = "On", default = true)]
    pub t_on: BoolParam,
    #[param(name = "Off")]
    pub t_off: BoolParam,

    // -- Dropdowns (default + wide) --
    #[param(name = "Mode")]
    pub mode: EnumParam<Mode>,
    #[param(name = "Mode Wide")]
    pub mode_wide: EnumParam<Mode>,

    // -- Meters: single-channel, stereo pair, and a 6-channel bus --
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
        // The 6-channel bus meter gets stepped fractions of the peak so
        // each channel renders at a different height even when only one
        // input channel is feeding signal.
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
                    // Auto-flow drops these six into cols 3-5 of rows
                    // 1-2, filling the gap to the right of KBig.
                    knob(P::K5, "1x1"),
                    knob(P::K6, "1x1"),
                    knob(P::K7, "1x1"),
                    knob(P::K8, "1x1"),
                    knob(P::K9, "1x1"),
                    knob(P::K10, "1x1"),
                ],
            ),
            section(
                "Sliders",
                vec![
                    slider(P::SFloat, "Float"),
                    slider(P::SInt, "Int"),
                    knob(P::KMid, "Mid"),
                    slider(P::SWide, "Wide").cols(2),
                ],
            ),
            section(
                "Toggles",
                vec![toggle(P::TOn, "On"), toggle(P::TOff, "Off")],
            ),
            section(
                "Dropdown",
                vec![
                    dropdown(P::Mode, "Popup").cols(2),
                    dropdown(P::ModeWide, "Wide").cols(4),
                ],
            ),
            section(
                "Meters",
                vec![
                    meter(&[P::MIn], "Input").rows(2),
                    meter(&[P::ML, P::MR], "L / R").rows(2),
                    meter(&[P::M6a, P::M6b, P::M6c, P::M6d, P::M6e, P::M6f], "6ch")
                        .cols(2)
                        .rows(2),
                ],
            ),
            section(
                "XY Pad",
                vec![
                    xy_pad(P::K5, P::K6, "1x1").cols(1).rows(1),
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
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
