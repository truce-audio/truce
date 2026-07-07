// Synth runs ADSR + voice rendering in f64 for cumulative-state
// stability (phase accumulator, envelope coefficients); the f64
// prelude makes that the buffer precision too - the format wrapper
// widens the host's f32 audio buffer to f64 at the block boundary
// and narrows on the way out.
use std::f64::consts::TAU;

use truce::prelude64::*;
use truce_core::midi::{norm_7bit, norm_pitch_bend};
use truce_gui::IntoLayoutEditor;
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
pub struct FilterParams {
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
}

#[derive(Params)]
pub struct EnvParams {
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
}

#[derive(Params)]
pub struct SynthParams {
    #[param(name = "Waveform", short_name = "Wave", default = 1)]
    pub waveform: EnumParam<Waveform>,

    #[nested]
    pub filter: FilterParams,

    #[nested]
    pub envelope: EnvParams,

    #[param(name = "Volume", short_name = "Vol",
            range = "linear(-60, 0)", default = -6.0,
            unit = "dB", smooth = "exp(5)")]
    pub volume: FloatParam,

    /// Pitch-bend target. VST3 has no native pitch-bend input event,
    /// so the host routes the pitch wheel to this parameter via
    /// `IMidiMapping`; the wrapper bridges the resulting change back
    /// into an `EventBody::PitchBend`. Hidden because it is a MIDI
    /// proxy, not a knob the user reaches for. AU / CLAP deliver pitch
    /// bend as raw MIDI and ignore this binding.
    #[param(
        name = "Pitch Bend",
        short_name = "Bend",
        range = "linear(-1, 1)",
        default = 0.0,
        flags = "hidden | automatable",
        midi_source = "pitchbend"
    )]
    pub bend: FloatParam,

    /// Mod-wheel (CC1) target, driving vibrato depth. Same story as
    /// `bend`: VST3 routes the wheel here via `IMidiMapping` and the
    /// wrapper bridges it back to an `EventBody::ControlChange`, while
    /// AU / CLAP deliver the CC as raw MIDI. Hidden MIDI proxy.
    #[param(
        name = "Mod Wheel",
        short_name = "Mod",
        range = "linear(0, 1)",
        default = 0.0,
        flags = "hidden | automatable",
        midi_cc = 1
    )]
    pub mod_wheel: FloatParam,
}

// --- Plugin ---

const MAX_VOICES: usize = 16;

/// Pitch-bend range in semitones at full deflection, matching the
/// MIDI default of +/-2 semitones.
const PITCH_BEND_RANGE: f64 = 2.0;

/// Vibrato LFO rate in Hz, and its depth in semitones at a fully
/// raised mod wheel.
const VIBRATO_RATE_HZ: f64 = 5.0;
const VIBRATO_DEPTH_SEMITONES: f64 = 0.5;

pub struct Synth {
    pub params: Arc<SynthParams>,
    voices: Vec<Voice>,
    sample_rate: f64,
    /// Channel pitch bend as a frequency multiplier. `1.0` is centered.
    pitch_bend_mult: f64,
    /// Mod-wheel position (CC1), `0.0..=1.0`, scaling vibrato depth.
    mod_wheel: f64,
    /// Vibrato LFO phase, `0.0..1.0`.
    lfo_phase: f64,
}

impl Synth {
    pub fn new(params: Arc<SynthParams>) -> Self {
        Self {
            params,
            voices: Vec::with_capacity(MAX_VOICES),
            sample_rate: 44100.0,
            pitch_bend_mult: 1.0,
            mod_wheel: 0.0,
            lfo_phase: 0.0,
        }
    }

    /// Map a 14-bit pitch-bend code to a frequency multiplier.
    fn pitch_bend(&mut self, value: u16) {
        let semitones = f64::from(norm_pitch_bend(value)) * PITCH_BEND_RANGE;
        self.pitch_bend_mult = 2.0_f64.powf(semitones / 12.0);
    }

    fn note_on(&mut self, note: u8, velocity: f32) {
        let freq = midi_note_to_freq(note);
        let attack = self.params.envelope.attack.value();
        let decay = self.params.envelope.decay.value();
        let sustain = self.params.envelope.sustain.value();
        let release = self.params.envelope.release.value();

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
    type Params = SynthParams;

    fn bus_layouts() -> Vec<BusLayout> {
        vec![BusLayout::new().with_output("Main", ChannelConfig::Stereo)]
    }

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.sample_rate = sample_rate;
        self.voices.clear();
        self.pitch_bend_mult = 1.0;
        self.mod_wheel = 0.0;
        self.lfo_phase = 0.0;
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
        // A mono or multi-mono host instance hands us a single output
        // channel; writing a second would be out of bounds.
        let out_channels = buffer.num_output_channels();

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
                    EventBody::PitchBend { value, .. } => self.pitch_bend(*value),
                    // CC1 is the mod wheel; steer vibrato depth from it.
                    EventBody::ControlChange { cc: 1, value, .. } => {
                        self.mod_wheel = f64::from(norm_7bit(*value));
                    }
                    _ => {}
                }
                next_event += 1;
            }

            let waveform_idx = self.params.waveform.index();
            let cutoff = self.params.filter.cutoff.read();
            let resonance = self.params.filter.resonance.read();
            let volume = db_to_linear(self.params.volume.read());

            // Advance the shared vibrato LFO and fold it into the
            // channel pitch bend, so every voice gets one combined
            // pitch multiplier this sample.
            let vibrato_semitones =
                self.mod_wheel * VIBRATO_DEPTH_SEMITONES * (self.lfo_phase * TAU).sin();
            self.lfo_phase += VIBRATO_RATE_HZ / self.sample_rate;
            if self.lfo_phase >= 1.0 {
                self.lfo_phase -= 1.0;
            }
            let pitch_mult = self.pitch_bend_mult * 2.0_f64.powf(vibrato_semitones / 12.0);

            let mut sample = 0.0f64;
            for voice in &mut self.voices {
                sample += voice.render(
                    waveform_idx,
                    cutoff,
                    resonance,
                    self.sample_rate,
                    pitch_mult,
                );
            }
            sample *= volume;

            let out = sample.clamp(-1.0, 1.0);
            buffer.output(0)[i] = out;
            if out_channels > 1 {
                buffer.output(1)[i] = out;
            }
        }

        self.voices.retain(|v| !v.is_done());
        if self.voices.is_empty() {
            ProcessStatus::Tail(0)
        } else {
            ProcessStatus::Normal
        }
    }

    fn editor(params: Arc<SynthParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![
            widgets(vec![
                dropdown(P::Waveform, "Wave").cols(2),
                knob(P::Volume, "Volume"),
            ]),
            section(
                "FILTER",
                vec![
                    knob(params.filter.cutoff.id(), "Cutoff"),
                    knob(params.filter.resonance.id(), "Reso"),
                ],
            ),
            section(
                "ENVELOPE",
                vec![
                    knob(params.envelope.attack.id(), "Attack"),
                    knob(params.envelope.decay.id(), "Decay"),
                    knob(params.envelope.sustain.id(), "Sustain"),
                    knob(params.envelope.release.id(), "Release"),
                ],
            ),
        ])
        .with_title("TRUCE SYNTH")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: Synth,
    params: SynthParams,
}

truce::enable_rt_paranoid!();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_is_allocation_free() {
        use std::time::Duration;
        use truce_test::{InputSource, assert_no_audio_alloc, driver};
        assert_no_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.25))
                .script(|s| {
                    s.note_on(60, 0.8);
                    s.set_param(P::Waveform, 0.9);
                    s.wait_ms(15);
                    s.set_param(P::Waveform, 0.1);
                    s.wait_ms(15);
                    s.note_off(60);
                })
                .run()
        });
    }

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
    fn pitch_bend_raises_pitch() {
        use std::time::Duration;
        use truce_test::driver;

        // Count sign changes (a proxy for frequency) over the tail of
        // a steady note, with and without an upward pitch bend.
        fn zero_crossings(samples: &[f32]) -> usize {
            samples
                .windows(2)
                .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
                .count()
        }

        let plain = driver!(Plugin)
            .duration(Duration::from_millis(200))
            .script(|s| s.note_on(60, 100.0 / 127.0))
            .run();

        let bent = driver!(Plugin)
            .duration(Duration::from_millis(200))
            .script(|s| {
                s.note_on(60, 100.0 / 127.0);
                s.pitch_bend(1.0); // full bend up
            })
            .run();

        // Compare the settled tail so the envelope attack doesn't skew
        // the counts.
        let plain_xings = zero_crossings(&plain.output[0][4000..]);
        let bent_xings = zero_crossings(&bent.output[0][4000..]);
        assert!(
            bent_xings > plain_xings,
            "expected pitch bend up to raise frequency: bent={bent_xings}, plain={plain_xings}"
        );
    }

    #[test]
    fn mod_wheel_adds_vibrato() {
        use std::time::Duration;
        use truce_test::{assertions, driver};

        // A raised mod wheel applies a pitch LFO, so the output diverges
        // from the steady, unmodulated note.
        let plain = driver!(Plugin)
            .duration(Duration::from_millis(200))
            .script(|s| s.note_on(60, 100.0 / 127.0))
            .run();

        let vibrato = driver!(Plugin)
            .duration(Duration::from_millis(200))
            .script(|s| {
                s.note_on(60, 100.0 / 127.0);
                s.cc(1, 1.0); // mod wheel fully up
            })
            .run();

        assertions::assert_no_nans(&vibrato);
        let max_diff = plain.output[0]
            .iter()
            .zip(&vibrato.output[0])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_diff > 0.01,
            "expected mod wheel to modulate the tone, but max diff was {max_diff}"
        );
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
