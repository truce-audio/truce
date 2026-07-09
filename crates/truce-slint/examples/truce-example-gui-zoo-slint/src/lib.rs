//! Slint counterpart of `truce-example-gui-zoo`. Same param shapes
//! (so every unit / range / discrete-snap path is exercised) but
//! laid out through a `.slint` markup file. Slint's `Meter`
//! widget is hardcoded stereo, so the 6-channel meter the CPU /
//! egui / iced zoos exercise has no slint equivalent here; a pair
//! of stereo meters covers the rendering surface that slint does
//! support.

use std::cell::Cell;

use truce::prelude::*;
use truce_core::meter_display;
use truce_slint::{PluginContext, SlintEditor, SyncFn};

slint::include_modules!();

use ZooParamsParamId as P;

// Per-frame meter release. The DSP writes a peak each block, but a
// stopped transport stops feeding blocks, so the raw slot freezes at
// the last peak. The GUI eases the displayed level down whenever the
// slot stops changing, instead of sticking full.
const METER_DECAY: f32 = 0.85;

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
    // -- Knobs (mixed ranges + units to exercise every formatter path) --
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

    // -- Dropdown --
    #[param(name = "Mode")]
    pub mode: EnumParam<Mode>,

    // -- Meters (mono + stereo; the slint widget is locked to 2 bars) --
    #[meter]
    pub m_l: MeterSlot,
    #[meter]
    pub m_r: MeterSlot,
}

/// Stateless descriptor - the zoo is a passthrough with no DSP state.
pub struct ZooSlint;

impl PluginLogic for ZooSlint {
    type Params = ZooParams;
    type DspState = ();

    fn init(_params: &ZooParams) {}

    fn process(
        _state: &mut (),
        _params: &ZooParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Passthrough.
        let n_in = buffer.num_input_channels();
        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            if ch < n_in {
                out.copy_from_slice(inp);
            } else {
                out.fill(0.0);
            }
        }

        if buffer.num_output_channels() >= 1 {
            context.set_meter(P::ML, buffer.output_peak(0));
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MR, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<ZooParams>) -> Box<dyn Editor> {
        SlintEditor::new(
            params.clone(),
            (700, 900),
            |state: PluginContext<ZooParams>| -> SyncFn<ZooParams> {
                let ui = ZooUi::new().unwrap();

                // -- UI -> host: per-widget callbacks emit `automate`. --

                // Knobs (9).
                let s = state.clone();
                ui.on_k_mix_changed(move |v| s.automate(P::KMix, f64::from(v)));
                let s = state.clone();
                ui.on_k_gain_changed(move |v| s.automate(P::KGain, f64::from(v)));
                let s = state.clone();
                ui.on_k_freq_changed(move |v| s.automate(P::KFreq, f64::from(v)));
                let s = state.clone();
                ui.on_k_q_changed(move |v| s.automate(P::KQ, f64::from(v)));
                let s = state.clone();
                ui.on_k_phase_changed(move |v| s.automate(P::KPhase, f64::from(v)));
                let s = state.clone();
                ui.on_k_pitch_changed(move |v| s.automate(P::KPitch, f64::from(v)));
                let s = state.clone();
                ui.on_k_time_changed(move |v| s.automate(P::KTime, f64::from(v)));
                let s = state.clone();
                ui.on_k_release_changed(move |v| s.automate(P::KRelease, f64::from(v)));
                let s = state.clone();
                ui.on_k_pan_changed(move |v| s.automate(P::KPan, f64::from(v)));

                // Sliders (3).
                let s = state.clone();
                ui.on_s_float_changed(move |v| s.automate(P::SFloat, f64::from(v)));
                let s = state.clone();
                ui.on_s_int_changed(move |v| s.automate(P::SInt, f64::from(v)));
                let s = state.clone();
                ui.on_s_wide_changed(move |v| s.automate(P::SWide, f64::from(v)));

                // Toggles (2).
                let s = state.clone();
                ui.on_t_on_toggled(move |v| s.automate(P::TOn, if v { 1.0 } else { 0.0 }));
                let s = state.clone();
                ui.on_t_off_toggled(move |v| s.automate(P::TOff, if v { 1.0 } else { 0.0 }));

                // Dropdown (Mode) - integer-index callback. Map index
                // -> normalized via `discrete_norm` for the 8-variant Mode.
                let s = state.clone();
                ui.on_mode_changed(move |idx| {
                    #[allow(clippy::cast_sign_loss)]
                    let i = idx.max(0) as usize;
                    let norm = truce_core::cast::discrete_norm(i, 8);
                    s.automate(P::Mode, norm);
                });

                // XY pads (3) - each axis is an independent automate call.
                let s = state.clone();
                ui.on_xy_small_changed_x(move |v| s.automate(P::KMix, f64::from(v)));
                let s = state.clone();
                ui.on_xy_small_changed_y(move |v| s.automate(P::KGain, f64::from(v)));
                let s = state.clone();
                ui.on_xy_med_changed_x(move |v| s.automate(P::KFreq, f64::from(v)));
                let s = state.clone();
                ui.on_xy_med_changed_y(move |v| s.automate(P::KQ, f64::from(v)));
                let s = state.clone();
                ui.on_xy_big_changed_x(move |v| s.automate(P::KPan, f64::from(v)));
                let s = state.clone();
                ui.on_xy_big_changed_y(move |v| s.automate(P::KPhase, f64::from(v)));

                // -- host -> UI: per-frame sync. --
                // Displayed meter level + last raw slot reading, kept across
                // frames so the meter can decay when the transport stops.
                let meter_l = Cell::new(0.0f32);
                let meter_r = Cell::new(0.0f32);
                let prev_raw_l = Cell::new(0.0f32);
                let prev_raw_r = Cell::new(0.0f32);
                Box::new(move |state: &PluginContext<ZooParams>| {
                    // Knobs.
                    ui.set_k_mix(state.get_param(P::KMix));
                    ui.set_k_mix_text(slint::SharedString::from(state.format_param(P::KMix)));
                    ui.set_k_gain(state.get_param(P::KGain));
                    ui.set_k_gain_text(slint::SharedString::from(state.format_param(P::KGain)));
                    ui.set_k_freq(state.get_param(P::KFreq));
                    ui.set_k_freq_text(slint::SharedString::from(state.format_param(P::KFreq)));
                    ui.set_k_q(state.get_param(P::KQ));
                    ui.set_k_q_text(slint::SharedString::from(state.format_param(P::KQ)));
                    ui.set_k_phase(state.get_param(P::KPhase));
                    ui.set_k_phase_text(slint::SharedString::from(state.format_param(P::KPhase)));
                    ui.set_k_pitch(state.get_param(P::KPitch));
                    ui.set_k_pitch_text(slint::SharedString::from(state.format_param(P::KPitch)));
                    ui.set_k_time(state.get_param(P::KTime));
                    ui.set_k_time_text(slint::SharedString::from(state.format_param(P::KTime)));
                    ui.set_k_release(state.get_param(P::KRelease));
                    ui.set_k_release_text(slint::SharedString::from(
                        state.format_param(P::KRelease),
                    ));
                    ui.set_k_pan(state.get_param(P::KPan));
                    ui.set_k_pan_text(slint::SharedString::from(state.format_param(P::KPan)));

                    // Sliders.
                    ui.set_s_float(state.get_param(P::SFloat));
                    ui.set_s_float_text(slint::SharedString::from(state.format_param(P::SFloat)));
                    ui.set_s_int(state.get_param(P::SInt));
                    ui.set_s_int_text(slint::SharedString::from(state.format_param(P::SInt)));
                    ui.set_s_wide(state.get_param(P::SWide));
                    ui.set_s_wide_text(slint::SharedString::from(state.format_param(P::SWide)));

                    // Toggles.
                    ui.set_t_on(state.get_param(P::TOn) > 0.5);
                    ui.set_t_off(state.get_param(P::TOff) > 0.5);

                    // Dropdown - 8-variant `Mode`.
                    let norm = f64::from(state.get_param(P::Mode));
                    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                    let idx = truce_core::cast::discrete_index(norm, 8) as i32;
                    ui.set_mode_index(idx);

                    // Meters. A changed slot means a fresh audio block
                    // arrived, so follow it; a frozen slot (stopped
                    // transport) eases down instead of sticking at the
                    // last peak.
                    let raw_l = state.get_meter(P::ML);
                    let disp_l = if (raw_l - prev_raw_l.get()).abs() > f32::EPSILON {
                        meter_display(raw_l)
                    } else {
                        meter_l.get() * METER_DECAY
                    };
                    prev_raw_l.set(raw_l);
                    meter_l.set(disp_l);
                    ui.set_m_l(disp_l);

                    let raw_r = state.get_meter(P::MR);
                    let disp_r = if (raw_r - prev_raw_r.get()).abs() > f32::EPSILON {
                        meter_display(raw_r)
                    } else {
                        meter_r.get() * METER_DECAY
                    };
                    prev_raw_r.set(raw_r);
                    meter_r.set(disp_r);
                    ui.set_m_r(disp_r);

                    // XY pads - read the same params back.
                    ui.set_xy_small_x(state.get_param(P::KMix));
                    ui.set_xy_small_y(state.get_param(P::KGain));
                    ui.set_xy_med_x(state.get_param(P::KFreq));
                    ui.set_xy_med_y(state.get_param(P::KQ));
                    ui.set_xy_big_x(state.get_param(P::KPan));
                    ui.set_xy_big_y(state.get_param(P::KPhase));
                })
            },
        )
        .into_editor()
    }
}

truce::plugin! {
    logic: ZooSlint,
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
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_slint_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_slint_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/gui_zoo_slint_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
