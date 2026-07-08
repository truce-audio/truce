use truce::prelude::*;
use truce_slint::{PluginContext, SlintEditor, SyncFn};

slint::include_modules!();

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

// --- Plugin ---

pub struct GainSlint {
    params: Arc<GainParams>,
}

impl GainSlint {
    pub fn new(params: Arc<GainParams>) -> Self {
        Self { params }
    }
}

impl PluginLogic for GainSlint {
    type Params = GainParams;

    fn reset(&mut self, config: &AudioConfig) {
        let sample_rate = config.sample_rate;
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
        SlintEditor::new(
            params.clone(),
            (176, 290),
            |state: PluginContext<GainParams>| -> SyncFn<GainParams> {
                let ui = GainUi::new().unwrap();

                // UI → host
                let s = state.clone();
                ui.on_gain_changed(move |v| s.automate(P::Gain, f64::from(v)));
                let s = state.clone();
                ui.on_pan_changed(move |v| s.automate(P::Pan, f64::from(v)));

                // host → UI (params + meters)
                Box::new(move |state: &PluginContext<GainParams>| {
                    ui.set_gain(state.get_param(P::Gain));
                    ui.set_pan(state.get_param(P::Pan));
                    ui.set_gain_text(slint::SharedString::from(state.format_param(P::Gain)));
                    ui.set_pan_text(slint::SharedString::from(state.format_param(P::Pan)));
                    ui.set_meter_left(meter_display(state.get_meter(P::MeterLeft)));
                    ui.set_meter_right(meter_display(state.get_meter(P::MeterRight)));
                })
            },
        )
        // Header strip + body fill. Slint's `.slint` markup uses
        // anchored layout (top: 0; bottom: 0) which re-runs every
        // frame at the new window dimensions.
        .resizable(true)
        // 176 px = two 60 px knobs + 10 px gap + 16 px meter +
        // 10 px column gap + 10 px padding on each side; the
        // smallest width where the XY pad column matches the
        // knob row above.
        .min_size((176, 240))
        .max_size((1200, 900))
        .into_editor()
    }
}

truce::plugin! {
    logic: GainSlint,
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
        truce_test::screenshot!(Plugin, "screenshots/gain_slint_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/gain_slint_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/gain_slint_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
