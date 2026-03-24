use truce::prelude::*;
use truce_gui::layout::{GridLayout, GridWidget};

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

#[derive(Params)]
pub struct SynthParams {
    #[param(name = "Waveform", short_name = "Wave", default = 1)]
    pub waveform: EnumParam<Waveform>,

    #[param(name = "Filter Cutoff", short_name = "Cutoff",
            group = "Filter", range = "log(20, 20000)",
            default = 8000.0, unit = "Hz", smooth = "exp(5)")]
    pub cutoff: FloatParam,

    #[param(name = "Filter Resonance", short_name = "Reso",
            group = "Filter", range = "linear(0, 1)", smooth = "exp(5)")]
    pub resonance: FloatParam,

    #[param(name = "Attack", short_name = "Atk",
            group = "Envelope", range = "log(0.001, 5)",
            default = 0.01, unit = "s")]
    pub attack: FloatParam,

    #[param(name = "Decay", short_name = "Dec",
            group = "Envelope", range = "log(0.001, 5)",
            default = 0.1, unit = "s")]
    pub decay: FloatParam,

    #[param(name = "Sustain", short_name = "Sus",
            group = "Envelope", range = "linear(0, 1)", default = 0.7)]
    pub sustain: FloatParam,

    #[param(name = "Release", short_name = "Rel",
            group = "Envelope", range = "log(0.01, 10)",
            default = 0.3, unit = "s")]
    pub release: FloatParam,

    #[param(name = "Volume", short_name = "Vol",
            range = "linear(-60, 0)", default = -6.0,
            unit = "dB", smooth = "exp(5)")]
    pub volume: FloatParam,
}

// --- Plugin ---

const MAX_VOICES: usize = 16;

pub struct Synth {
    pub params: std::sync::Arc<SynthParams>,
    voices: Vec<Voice>,
    sample_rate: f64,
}

impl Synth {
    pub fn new(params: std::sync::Arc<SynthParams>) -> Self {
        Self {
            params,
            voices: Vec::with_capacity(MAX_VOICES),
            sample_rate: 44100.0,
        }
    }

    fn note_on(&mut self, note: u8, velocity: f32) {
        let freq = midi_note_to_freq(note);
        let attack = self.params.attack.value() as f64;
        let decay = self.params.decay.value() as f64;
        let sustain = self.params.sustain.value() as f64;
        let release = self.params.release.value() as f64;

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
    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.sample_rate = sample_rate;
        self.voices.clear();
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
    }

    fn process(&mut self, buffer: &mut AudioBuffer, events: &EventList, _context: &mut ProcessContext) -> ProcessStatus {
        let mut next_event = 0;

        for i in 0..buffer.num_samples() {
            while let Some(event) = events.get(next_event) {
                if event.sample_offset as usize > i {
                    break;
                }
                match &event.body {
                    EventBody::NoteOn { note, velocity, .. } => self.note_on(*note, *velocity),
                    EventBody::NoteOff { note, .. } => self.note_off(*note),
                    _ => {}
                }
                next_event += 1;
            }

            let waveform_idx = self.params.waveform.index();
            let cutoff = self.params.cutoff.smoothed_next() as f64;
            let resonance = self.params.resonance.smoothed_next() as f64;
            let volume = db_to_linear(self.params.volume.smoothed_next() as f64);

            let mut sample = 0.0f64;
            for voice in &mut self.voices {
                sample += voice.render(waveform_idx, cutoff, resonance, self.sample_rate);
            }
            sample *= volume;

            let out = (sample as f32).clamp(-1.0, 1.0);
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

    fn layout(&self) -> truce_gui::layout::GridLayout {
        GridLayout::build("TRUCE SYNTH", "V0.1", 4, 70.0, vec![
            GridWidget::selector(P::Waveform, "Wave").cols(2),
            GridWidget::knob(P::Volume, "Volume"),
            GridWidget::knob(P::Cutoff, "Cutoff"),
            GridWidget::knob(P::Resonance, "Reso"),
            GridWidget::knob(P::Attack, "Attack"),
            GridWidget::knob(P::Decay, "Decay"),
            GridWidget::knob(P::Sustain, "Sustain"),
            GridWidget::knob(P::Release, "Release"),
        ], vec![
            (3, "FILTER"),
            (5, "ENVELOPE"),
        ])
    }
}

truce::plugin! {
    logic: Synth,
    params: SynthParams,
    bus_layouts: [BusLayout::new().with_output("Main", ChannelConfig::Stereo)],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<__HotShellWrapper>();
    }

    #[test]
    fn silence_without_midi() {
        let result = truce_test::render_instrument::<__HotShellWrapper>(512, 44100.0, &[]);
        truce_test::assert_silence(&result.output);
    }

    #[test]
    fn produces_sound_on_note_on() {
        let events = vec![truce_test::note_on(60, 100, 0)];
        let result = truce_test::render_instrument::<__HotShellWrapper>(512, 44100.0, &events);
        truce_test::assert_nonzero(&result.output);
        truce_test::assert_no_nans(&result.output);
    }

    #[test]
    fn note_off_decays_to_silence() {
        let events = vec![truce_test::note_on(60, 100, 0), truce_test::note_off(60, 0)];
        // Render many blocks to let the release tail finish
        let result = truce_test::render_instrument::<__HotShellWrapper>(44100, 44100.0, &events);
        // Last 1000 samples should be near silence
        let tail: Vec<f32> = result.output[0][43000..].to_vec();
        let max = tail.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(max < 0.01, "Expected decay to silence, but max was {max}");
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<__HotShellWrapper>();
    }

    #[test]
    fn state_round_trips() {
        truce_test::assert_state_round_trip::<__HotShellWrapper>();
    }

    // --- AU metadata ---

    #[test]
    fn au_type_codes_ascii() {
        truce_test::assert_au_type_codes_ascii::<__HotShellWrapper>();
    }

    #[test]
    fn fourcc_roundtrip() {
        truce_test::assert_fourcc_roundtrip::<__HotShellWrapper>();
    }

    #[test]
    fn bus_config_instrument() {
        truce_test::assert_bus_config_instrument::<__HotShellWrapper>();
    }

    // --- GUI lifecycle ---

    #[test]
    fn editor_lifecycle() {
        truce_test::assert_editor_lifecycle::<__HotShellWrapper>();
    }

    #[test]
    fn editor_size_consistent() {
        truce_test::assert_editor_size_consistent::<__HotShellWrapper>();
    }

    // --- Parameters ---

    #[test]
    fn param_defaults_match() {
        truce_test::assert_param_defaults_match::<__HotShellWrapper>();
    }

    #[test]
    fn param_normalized_clamped() {
        truce_test::assert_param_normalized_clamped::<__HotShellWrapper>();
    }

    #[test]
    fn param_normalized_roundtrip() {
        truce_test::assert_param_normalized_roundtrip::<__HotShellWrapper>();
    }

    #[test]
    fn param_count_matches() {
        truce_test::assert_param_count_matches::<__HotShellWrapper>();
    }

    #[test]
    fn no_duplicate_param_ids() {
        truce_test::assert_no_duplicate_param_ids::<__HotShellWrapper>();
    }

    // --- State resilience ---

    #[test]
    fn corrupt_state_no_crash() {
        truce_test::assert_corrupt_state_no_crash::<__HotShellWrapper>();
    }

    #[test]
    fn empty_state_no_crash() {
        truce_test::assert_empty_state_no_crash::<__HotShellWrapper>();
    }

    #[test]
    fn gui_snapshot() {
        let params = std::sync::Arc::new(SynthParams::new());
        let synth = Synth::new(std::sync::Arc::clone(&params));
        let layout = synth.layout();
        truce_test::assert_gui_snapshot_grid::<SynthParams>(
            "synth_default", params, layout, 0,
        );
    }
}
