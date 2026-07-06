//! Tempo-synced tremolo - exercises the host-transport feature.
//!
//! The DSP reads `ProcessContext::transport` to lock an amplitude LFO
//! to the host's beat grid. The editor reads `PluginContext::transport`
//! to display tempo / play state / beat position live in the UI.
//!
//! Both paths drop back to sensible defaults when the host doesn't
//! expose transport: free-running at 120 BPM internally, and a dash
//! ("-") for unknown fields in the readout.

use truce::prelude::*;
use truce_core::editor::PluginContext;
use truce_egui::EguiEditor;
use truce_egui::theme::{HEADER_BG, HEADER_TEXT};
use truce_egui::widgets::{param_dropdown, param_knob};
use truce_font::JETBRAINS_MONO;

const WINDOW_W: u32 = 270;
const WINDOW_H: u32 = 180;

// --- Parameters ---

use TremoloParamsParamId as P;
use std::sync::Arc;

/// LFO-cycle length as a note value. Maps directly to beats per cycle
/// (`Quarter == 1.0`, `Eighth == 0.5`, ...).
///
/// Display labels use word forms down to Quarter, fractions below
/// (`1/8`, `1/16`, `1/32`) - common producer-facing notation in the
/// short rhythmic range where the fraction reads faster than the
/// spelled-out word.
#[derive(ParamEnum)]
pub enum Rate {
    Whole,
    Half,
    Quarter,
    #[name = "1/8"]
    Eighth,
    #[name = "1/16"]
    Sixteenth,
    #[name = "1/32"]
    ThirtySecond,
}

impl Rate {
    /// Number of quarter-note beats one cycle of the LFO spans.
    fn beats_per_cycle(self) -> f64 {
        match self {
            Rate::Whole => 4.0,
            Rate::Half => 2.0,
            Rate::Quarter => 1.0,
            Rate::Eighth => 0.5,
            Rate::Sixteenth => 0.25,
            Rate::ThirtySecond => 0.125,
        }
    }
}

#[derive(ParamEnum)]
pub enum Shape {
    Sine,
    Triangle,
    Square,
}

impl Shape {
    /// Evaluate the LFO shape at `phase` in [0, 1). Result is in [0, 1].
    fn at(self, phase: f64) -> f64 {
        match self {
            Shape::Sine => 0.5 - 0.5 * (phase * std::f64::consts::TAU).cos(),
            Shape::Triangle => {
                // 0 → 1 over [0, 0.5), 1 → 0 over [0.5, 1).
                if phase < 0.5 {
                    phase * 2.0
                } else {
                    2.0 - phase * 2.0
                }
            }
            Shape::Square => {
                if phase < 0.5 {
                    0.0
                } else {
                    1.0
                }
            }
        }
    }
}

#[derive(Params)]
pub struct TremoloParams {
    #[param(name = "Depth", range = "linear(0, 1)", unit = "%", smooth = "exp(5)")]
    pub depth: FloatParam,

    #[param(name = "Rate")]
    pub rate: EnumParam<Rate>,

    #[param(name = "Shape")]
    pub shape: EnumParam<Shape>,
}

// --- Plugin ---

pub struct Tremolo {
    params: Arc<TremoloParams>,
    /// Free-running phase used when the host provides no tempo (e.g.
    /// standalone running, or a host that does not report transport).
    /// Advances at 2 Hz so the effect stays visibly active offline.
    free_phase: f64,
    sample_rate: f64,
}

impl Tremolo {
    pub fn new(params: Arc<TremoloParams>) -> Self {
        Self {
            params,
            free_phase: 0.0,
            sample_rate: 44100.0,
        }
    }
}

/// Rate at which `free_phase` advances when no host tempo is available.
const FREE_LFO_HZ: f64 = 2.0;

impl PluginLogic for Tremolo {
    type Params = TremoloParams;

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.sample_rate = sample_rate;
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
        self.free_phase = 0.0;
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        let transport = context.transport;
        let shape = self.params.shape.value();
        let rate = self.params.rate.value();
        let beats_per_cycle = rate.beats_per_cycle();
        let sr = self.sample_rate;
        let host_sync = transport.playing && transport.tempo > 0.0;

        let host_phase_inc = if host_sync {
            transport.tempo / 60.0 / (beats_per_cycle * sr)
        } else {
            0.0
        };
        let free_phase_inc = FREE_LFO_HZ / sr;

        let mut host_phase = if host_sync {
            (transport.position_beats / beats_per_cycle).rem_euclid(1.0)
        } else {
            0.0
        };
        let mut free_phase = self.free_phase;

        for i in 0..buffer.num_samples() {
            let depth = self.params.depth.read();
            let phase = if host_sync { host_phase } else { free_phase };
            let lfo = shape.at(phase); // in [0, 1]
            // `lfo` is in [0, 1]; the f32 cast is exact for that range.
            #[allow(clippy::cast_possible_truncation)]
            let gain = 1.0 - depth * (1.0 - lfo as f32);

            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * gain;
            }

            host_phase = (host_phase + host_phase_inc).rem_euclid(1.0);
            free_phase = (free_phase + free_phase_inc).rem_euclid(1.0);
        }

        self.free_phase = free_phase;
        ProcessStatus::Normal
    }

    fn editor(params: Arc<TremoloParams>) -> Box<dyn Editor> {
        Box::new(
            EguiEditor::new(params.clone(), (WINDOW_W, WINDOW_H), tremolo_ui)
                .with_visuals(truce_egui::theme::dark())
                .with_font(JETBRAINS_MONO),
        )
    }
}

fn tremolo_ui(ui: &mut egui::Ui, state: &PluginContext<TremoloParams>) {
    egui::Panel::top("header")
        .exact_size(30.0)
        .frame(egui::Frame::NONE.fill(HEADER_BG))
        .show_inside(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("TREMOLO")
                        .size(14.0)
                        .color(HEADER_TEXT)
                        .strong(),
                );
            });
        });

    egui::CentralPanel::default()
        .frame(egui::Frame::central_panel(ui.style()).inner_margin(10.0))
        .show_inside(ui, |ui| {
            draw_transport_readout(ui, state);
            ui.add_space(10.0);

            ui.spacing_mut().item_spacing = egui::vec2(16.0, 0.0);
            ui.horizontal(|ui| {
                param_knob(ui, state, P::Depth, "Depth");
                param_knob(ui, state, P::Rate, "Rate");
                param_dropdown(ui, state, P::Shape, "Shape", 1);
            });

            // Keep the UI animating so the beat position readout updates
            // while the host transport is running.
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(33));
        });
}

/// Read the editor's transport closure and render the readout as a
/// 2x2 grid so the tempo/beat and sig/samples columns line up:
/// ```text
/// ▶ 128.0 BPM   4/4
/// ♩ 12.25       smp 132300
/// ```
fn draw_transport_readout(ui: &mut egui::Ui, state: &PluginContext<TremoloParams>) {
    let transport = state.transport();
    let cell = |ui: &mut egui::Ui, text: &str| {
        ui.label(
            egui::RichText::new(text)
                .monospace()
                .size(13.0)
                .color(HEADER_TEXT),
        );
    };
    match transport_cells(transport.as_ref()) {
        None => cell(ui, "(no host transport)"),
        Some([tempo, sig, beat, samples]) => {
            egui::Grid::new("transport")
                .num_columns(2)
                .spacing(egui::vec2(16.0, 2.0))
                .show(ui, |ui| {
                    cell(ui, &tempo);
                    cell(ui, &sig);
                    ui.end_row();
                    cell(ui, &beat);
                    cell(ui, &samples);
                    ui.end_row();
                });
        }
    }
}

/// The four readout cells, laid out as two columns / two rows:
/// `[state+tempo, time-sig, beat, samples]`. `None` when the host
/// exposes no transport.
fn transport_cells(info: Option<&TransportInfo>) -> Option<[String; 4]> {
    let t = info?;
    let state = if t.playing { "\u{25B6}" } else { "\u{25A0}" };
    let tempo = if t.tempo > 0.0 {
        format!("{state} {:.1} BPM", t.tempo)
    } else {
        format!("{state} - BPM")
    };
    let sig = if t.time_sig_num > 0 && t.time_sig_den > 0 {
        format!("{}/{}", t.time_sig_num, t.time_sig_den)
    } else {
        "-/-".into()
    };
    let beat = if t.tempo > 0.0 {
        format!("\u{2669} {:.2}", t.position_beats)
    } else {
        "\u{2669} -".into()
    };
    // Host timeline position in samples (0 at song start).
    let samples = format!("smp {}", t.position_samples);
    Some([tempo, sig, beat, samples])
}

truce::plugin! {
    logic: Tremolo,
    params: TremoloParams,
}

truce::enable_rt_paranoid!();

#[cfg(test)]
mod tests {
    // Beats-per-cycle values are powers of two (0.125, 0.5, 1.0, 4.0)
    // - bit-exact equality is the contract.
    #![allow(clippy::float_cmp, clippy::cast_precision_loss)]

    use super::*;
    use truce_core::events::TransportInfo;

    #[test]
    fn process_is_allocation_free() {
        use std::time::Duration;
        use truce_test::{InputSource, assert_no_audio_alloc, driver};
        assert_no_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.25))
                .script(|s| {
                    s.set_param(P::Depth, 0.9);
                    s.wait_ms(15);
                    s.set_param(P::Depth, 0.1);
                    s.wait_ms(15);
                })
                .run()
        });
    }

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn renders_without_nans() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};
        let result = driver!(Plugin)
            .duration(Duration::from_millis(12))
            .input(InputSource::Constant(0.5))
            .run();
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

    #[test]
    fn transport_cells_stopped_default() {
        assert!(transport_cells(None).is_none());
        let cells = transport_cells(Some(&TransportInfo::default())).unwrap();
        let joined = cells.join(" ");
        // Stopped, no tempo reported → square + em dash placeholders.
        assert!(joined.contains('\u{25A0}'));
        assert!(joined.contains("- BPM"));
        assert!(joined.contains("smp 0"));
    }

    #[test]
    fn transport_cells_playing() {
        let playing = TransportInfo {
            playing: true,
            tempo: 128.0,
            time_sig_num: 4,
            time_sig_den: 4,
            position_beats: 12.25,
            position_samples: 132_300,
            ..TransportInfo::default()
        };
        let cells = transport_cells(Some(&playing)).unwrap();
        let joined = cells.join(" ");
        assert!(joined.contains('\u{25B6}'));
        assert!(joined.contains("128.0 BPM"));
        assert!(joined.contains("4/4"));
        assert!(joined.contains("12.25"));
        assert!(joined.contains("smp 132300"));
    }

    #[test]
    fn rate_beats_are_powers_of_two_scaled() {
        assert_eq!(Rate::Quarter.beats_per_cycle(), 1.0);
        assert_eq!(Rate::Eighth.beats_per_cycle(), 0.5);
        assert_eq!(Rate::Whole.beats_per_cycle(), 4.0);
        assert_eq!(Rate::ThirtySecond.beats_per_cycle(), 0.125);
    }

    #[test]
    fn shape_bounds() {
        for s in [Shape::Sine, Shape::Triangle, Shape::Square] {
            for i in 0..100 {
                let p = f64::from(i) / 100.0;
                let v = s.at(p);
                assert!(
                    (0.0..=1.0).contains(&v),
                    "{:?} at {:.2} = {} out of range",
                    s as u8,
                    p,
                    v
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/tremolo_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/tremolo_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/tremolo_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
