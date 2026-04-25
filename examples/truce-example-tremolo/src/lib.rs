//! Tempo-synced tremolo — exercises the host-transport feature.
//!
//! The DSP reads `ProcessContext::transport` to lock an amplitude LFO
//! to the host's beat grid. The editor reads `EditorContext::transport`
//! to display tempo / play state / beat position live in the UI.
//!
//! Both paths drop back to sensible defaults when the host doesn't
//! expose transport: free-running at 120 BPM internally, and a dash
//! ("—") for unknown fields in the readout.

use truce::prelude::*;
use truce_egui::theme::{HEADER_BG, HEADER_TEXT};
use truce_egui::widgets::{param_knob, param_selector};
use truce_egui::{EguiEditor, ParamState};

const WINDOW_W: u32 = 270;
const WINDOW_H: u32 = 162;

// --- Parameters ---

use TremoloParamsParamId as P;

/// LFO-cycle length as a note value. Maps directly to beats per cycle
/// (`Quarter == 1.0`, `Eighth == 0.5`, ...).
#[derive(ParamEnum)]
pub enum Rate {
    Whole,
    Half,
    Quarter,
    Eighth,
    Sixteenth,
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
    fn at(self, phase: f32) -> f32 {
        match self {
            Shape::Sine => 0.5 - 0.5 * (phase * std::f32::consts::TAU).cos(),
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
    #[param(name = "Depth", range = "linear(0, 1)", smooth = "exp(5)")]
    pub depth: FloatParam,

    #[param(name = "Rate")]
    pub rate: EnumParam<Rate>,

    #[param(name = "Shape")]
    pub shape: EnumParam<Shape>,
}

// --- Plugin ---

pub struct Tremolo {
    params: std::sync::Arc<TremoloParams>,
    /// Free-running phase used when the host provides no tempo (e.g.
    /// standalone running, or a host that does not report transport).
    /// Advances at 2 Hz so the effect stays visibly active offline.
    free_phase: f32,
    sample_rate: f64,
}

impl Tremolo {
    pub fn new(params: std::sync::Arc<TremoloParams>) -> Self {
        Self {
            params,
            free_phase: 0.0,
            sample_rate: 44100.0,
        }
    }
}

/// Rate at which `free_phase` advances when no host tempo is available.
const FREE_LFO_HZ: f32 = 2.0;

impl PluginLogic for Tremolo {
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
        let beats_per_cycle = rate.beats_per_cycle() as f32;

        // Use host transport when it reports playing + a positive tempo.
        // Otherwise fall back to the free-running LFO.
        let host_sync = transport.playing && transport.tempo > 0.0;
        let sample_rate = self.sample_rate as f32;

        // Per-sample phase increments. With host sync the increment is
        // derived from tempo; without it, from a fixed free-LFO rate.
        let host_phase_inc = if host_sync {
            (transport.tempo as f32 / 60.0) / (beats_per_cycle * sample_rate)
        } else {
            0.0
        };
        let free_phase_inc = FREE_LFO_HZ / sample_rate;

        // Host phase at block start: convert `position_beats` into the
        // normalized LFO phase by dividing by beats-per-cycle.
        let mut host_phase = if host_sync {
            let beat = transport.position_beats as f32;
            (beat / beats_per_cycle).rem_euclid(1.0)
        } else {
            0.0
        };
        let mut free_phase = self.free_phase;

        for i in 0..buffer.num_samples() {
            let depth = self.params.depth.smoothed_next();
            let phase = if host_sync { host_phase } else { free_phase };
            let lfo = shape.at(phase); // in [0, 1]
            let gain = 1.0 - depth * (1.0 - lfo);

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

    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        Some(Box::new(
            EguiEditor::new((WINDOW_W, WINDOW_H), tremolo_ui)
                .with_visuals(truce_egui::theme::dark())
                .with_font(truce_gui::font::JETBRAINS_MONO),
        ))
    }
}

fn tremolo_ui(ctx: &egui::Context, state: &ParamState) {
    egui::TopBottomPanel::top("header")
        .exact_height(30.0)
        .frame(egui::Frame::NONE.fill(HEADER_BG))
        .show(ctx, |ui| {
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
        .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(10.0))
        .show(ctx, |ui| {
            draw_transport_readout(ui, state);
            ui.add_space(10.0);

            ui.spacing_mut().item_spacing = egui::vec2(16.0, 0.0);
            ui.horizontal(|ui| {
                param_knob(ui, state, P::Depth, "Depth");
                param_selector(ui, state, P::Rate, "Rate", Rate::variant_count() as u32);
                param_selector(ui, state, P::Shape, "Shape", Shape::variant_count() as u32);
            });

            // Keep the UI animating so the beat position readout updates
            // while the host transport is running.
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        });
}

/// Read the editor's transport closure and render a compact readout
/// like `▶ 128.0 BPM • 4/4 • ♩ 12.25` (or `■ — BPM` when stopped).
fn draw_transport_readout(ui: &mut egui::Ui, state: &ParamState) {
    let transport = (state.context().transport)();
    let line = format_transport(transport.as_ref());

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(line)
                .monospace()
                .size(13.0)
                .color(HEADER_TEXT),
        );
    });
}

fn format_transport(info: Option<&truce_core::events::TransportInfo>) -> String {
    let Some(t) = info else {
        return "(no host transport)".into();
    };
    let state = if t.playing { "\u{25B6}" } else { "\u{25A0}" };
    let tempo = if t.tempo > 0.0 {
        format!("{:.1} BPM", t.tempo)
    } else {
        "— BPM".into()
    };
    let sig = if t.time_sig_num > 0 && t.time_sig_den > 0 {
        format!("{}/{}", t.time_sig_num, t.time_sig_den)
    } else {
        "—/—".into()
    };
    let beat = if t.tempo > 0.0 {
        format!("\u{2669} {:.2}", t.position_beats)
    } else {
        "\u{2669} —".into()
    };
    format!("{state}  {tempo}  \u{2022}  {sig}  \u{2022}  {beat}")
}

truce::plugin! {
    logic: Tremolo,
    params: TremoloParams,
}

#[cfg(test)]
mod tests {
    use super::*;
    use truce_core::events::TransportInfo;

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn renders_without_nans() {
        let result = truce_test::render_effect::<Plugin>(512, 44100.0);
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
    fn format_transport_stopped_default() {
        assert_eq!(format_transport(None), "(no host transport)");
        let stopped = TransportInfo::default();
        let s = format_transport(Some(&stopped));
        // Stopped, no tempo reported → square + em dash placeholders.
        assert!(s.contains('\u{25A0}'));
        assert!(s.contains("— BPM"));
    }

    #[test]
    fn format_transport_playing() {
        let playing = TransportInfo {
            playing: true,
            tempo: 128.0,
            time_sig_num: 4,
            time_sig_den: 4,
            position_beats: 12.25,
            ..TransportInfo::default()
        };
        let s = format_transport(Some(&playing));
        assert!(s.contains('\u{25B6}'));
        assert!(s.contains("128.0 BPM"));
        assert!(s.contains("4/4"));
        assert!(s.contains("12.25"));
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
                let p = i as f32 / 100.0;
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
}
