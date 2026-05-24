// Synth runs ADSR + voice rendering in f64 for cumulative-state
// stability (phase accumulator, envelope coefficients); the f64
// prelude makes that the buffer precision too - the format wrapper
// widens the host's f32 audio buffer to f64 at the block boundary
// and narrows on the way out.
use truce::prelude64::*;
use truce_core::midi::norm_7bit;
use truce_gui_types::layout::{GridLayout, dropdown, knob, section, widgets};

mod voice;
use voice::Voice;

// --- Waveform enum ---

#[derive(ParamEnum)]
pub enum Waveform {
    Sine,
    Saw,
    Square,
    Triangle,
}

// --- Parameters ---

use SynthParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct SynthParams {
    #[param(name = "Waveform", short_name = "Wave", default = 1)]
    pub waveform: EnumParam<Waveform>,

    #[param(
        name = "Filter Cutoff",
        short_name = "Cutoff",
        group = "Filter",
        range = "log(20, 20000)",
        default = 8000.0,
        unit = "Hz",
        smooth = "exp(5)"
    )]
    pub cutoff: FloatParam,

    #[param(
        name = "Filter Resonance",
        short_name = "Reso",
        group = "Filter",
        range = "linear(0, 1)",
        smooth = "exp(5)"
    )]
    pub resonance: FloatParam,

    #[param(
        name = "Attack",
        short_name = "Atk",
        group = "Envelope",
        range = "log(0.001, 5)",
        default = 0.01,
        unit = "s"
    )]
    pub attack: FloatParam,

    #[param(
        name = "Decay",
        short_name = "Dec",
        group = "Envelope",
        range = "log(0.001, 5)",
        default = 0.1,
        unit = "s"
    )]
    pub decay: FloatParam,

    #[param(
        name = "Sustain",
        short_name = "Sus",
        group = "Envelope",
        range = "linear(0, 1)",
        default = 0.7
    )]
    pub sustain: FloatParam,

    #[param(
        name = "Release",
        short_name = "Rel",
        group = "Envelope",
        range = "log(0.01, 10)",
        default = 0.3,
        unit = "s"
    )]
    pub release: FloatParam,

    #[param(name = "Volume", short_name = "Vol",
            range = "linear(-60, 0)", default = -6.0,
            unit = "dB", smooth = "exp(5)")]
    pub volume: FloatParam,
}

// --- Plugin ---

const MAX_VOICES: usize = 16;

pub struct Synth {
    pub params: Arc<SynthParams>,
    voices: Vec<Voice>,
    sample_rate: f64,
}

impl Synth {
    pub fn new(params: Arc<SynthParams>) -> Self {
        Self {
            params,
            voices: Vec::with_capacity(MAX_VOICES),
            sample_rate: 44100.0,
        }
    }

    fn note_on(&mut self, note: u8, velocity: f32) {
        let freq = midi_note_to_freq(note);
        let attack = self.params.attack.value();
        let decay = self.params.decay.value();
        let sustain = self.params.sustain.value();
        let release = self.params.release.value();

        self.voices.push(Voice::new(
            note,
            freq,
            velocity,
            self.sample_rate,
            attack,
            decay,
            sustain,
            release,
        ));
        if self.voices.len() > MAX_VOICES {
            self.voices.remove(0);
        }
    }

    fn note_off(&mut self, note: u8) {
        for voice in &mut self.voices {
            if voice.note == note && !voice.releasing {
                voice.release();
            }
        }
    }
}

impl PluginLogic for Synth {
    fn bus_layouts() -> Vec<BusLayout> {
        vec![BusLayout::new().with_output("Main", ChannelConfig::Stereo)]
    }

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.sample_rate = sample_rate;
        self.voices.clear();
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        let mut next_event = 0;

        for i in 0..buffer.num_samples() {
            while let Some(event) = events.get(next_event) {
                if event.sample_offset as usize > i {
                    break;
                }
                match &event.body {
                    EventBody::NoteOn { note, velocity, .. } => {
                        self.note_on(*note, norm_7bit(*velocity));
                    }
                    EventBody::NoteOff { note, .. } => self.note_off(*note),
                    _ => {}
                }
                next_event += 1;
            }

            let waveform_idx = self.params.waveform.index();
            let cutoff = self.params.cutoff.read();
            let resonance = self.params.resonance.read();
            let volume = db_to_linear(self.params.volume.read());

            let mut sample = 0.0f64;
            for voice in &mut self.voices {
                sample += voice.render(waveform_idx, cutoff, resonance, self.sample_rate);
            }
            sample *= volume;

            let out = sample.clamp(-1.0, 1.0);
            buffer.output(0)[i] = out;
            buffer.output(1)[i] = out;
        }

        self.voices.retain(|v| !v.is_done());
        if self.voices.is_empty() {
            ProcessStatus::Tail(0)
        } else {
            ProcessStatus::Normal
        }
    }

    fn editor(&self) -> Box<dyn Editor> {
        truce_gui::default_editor(
            self.params.clone(),
            GridLayout::build(vec![
                widgets(vec![
                    dropdown(P::Waveform, "Wave").cols(2),
                    knob(P::Volume, "Volume"),
                ]),
                section(
                    "FILTER",
                    vec![knob(P::Cutoff, "Cutoff"), knob(P::Resonance, "Reso")],
                ),
                section(
                    "ENVELOPE",
                    vec![
                        knob(P::Attack, "Attack"),
                        knob(P::Decay, "Decay"),
                        knob(P::Sustain, "Sustain"),
                        knob(P::Release, "Release"),
                    ],
                ),
            ])
            .with_title("TRUCE SYNTH"),
        )
    }
}

truce::plugin! {
    logic: Synth,
    params: SynthParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn silence_without_midi() {
        use std::time::Duration;
        use truce_test::{assertions, driver};
        let result = driver!(Plugin).duration(Duration::from_millis(12)).run();
        assertions::assert_silence(&result);
    }

    #[test]
    fn produces_sound_on_note_on() {
        use std::time::Duration;
        use truce_test::{assertions, driver};
        let result = driver!(Plugin)
            .duration(Duration::from_millis(12))
            .script(|s| s.note_on(60, 100.0 / 127.0))
            .run();
        assertions::assert_nonzero(&result);
        assertions::assert_no_nans(&result);
    }

    #[test]
    fn note_off_decays_to_silence() {
        use std::time::Duration;
        use truce_test::driver;
        // Render ~1 second to let the release tail finish.
        let result = driver!(Plugin)
            .duration(Duration::from_secs(1))
            .script(|s| {
                s.note_on(60, 100.0 / 127.0);
                s.note_off(60);
            })
            .run();
        // Last 1000 samples should be near silence.
        let tail: Vec<f32> = result.output[0][43000..].to_vec();
        let max = tail.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(max < 0.01, "Expected decay to silence, but max was {max}");
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    #[test]
    fn state_round_trips() {
        truce_test::assert_state_round_trip::<Plugin>();
    }

    // --- AU metadata ---

    #[test]
    fn au_type_codes_ascii() {
        truce_test::assert_au_type_codes_ascii::<Plugin>();
    }

    #[test]
    fn fourcc_roundtrip() {
        truce_test::assert_fourcc_roundtrip::<Plugin>();
    }

    #[test]
    fn bus_config_instrument() {
        truce_test::assert_bus_config_instrument::<Plugin>();
    }

    // --- GUI lifecycle ---

    #[test]
    fn editor_lifecycle() {
        truce_test::assert_editor_lifecycle::<Plugin>();
    }

    #[test]
    fn editor_size_consistent() {
        truce_test::assert_editor_size_consistent::<Plugin>();
    }

    // --- Parameters ---

    #[test]
    fn param_defaults_match() {
        truce_test::assert_param_defaults_match::<Plugin>();
    }

    #[test]
    fn param_normalized_clamped() {
        truce_test::assert_param_normalized_clamped::<Plugin>();
    }

    #[test]
    fn param_normalized_roundtrip() {
        truce_test::assert_param_normalized_roundtrip::<Plugin>();
    }

    #[test]
    fn param_count_matches() {
        truce_test::assert_param_count_matches::<Plugin>();
    }

    #[test]
    fn no_duplicate_param_ids() {
        truce_test::assert_no_duplicate_param_ids::<Plugin>();
    }

    // --- State resilience ---

    #[test]
    fn corrupt_state_no_crash() {
        truce_test::assert_corrupt_state_no_crash::<Plugin>();
    }

    #[test]
    fn empty_state_no_crash() {
        truce_test::assert_empty_state_no_crash::<Plugin>();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/synth_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/synth_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/synth_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
