## Building a synthesizer

Here is the actual synth example from `examples/synth/` showing
how instruments work. Like the gain example, everything lives in a
single crate with `src/lib.rs` and `src/main.rs`.

### src/lib.rs

```rust
use truce::prelude::*;
use truce_gui::layout::{GridLayout, dropdown, knob, section, widgets};

mod voice;
use voice::Voice;

// --- Waveform enum ---

#[derive(ParamEnum)]
pub enum Waveform { Sine, Saw, Square, Triangle }

// --- Parameters ---

use SynthParamsParamId as P;

#[derive(Params)]
pub struct SynthParams {
    #[param(name = "Waveform", short_name = "Wave", default = 1)]
    pub waveform: EnumParam<Waveform>,

    #[param(name = "Filter Cutoff", short_name = "Cutoff",
            group = "Filter", range = "log(20, 20000)", default = 8000,
            unit = "Hz", smooth = "exp(5)")]
    pub cutoff: FloatParam,

    #[param(name = "Filter Resonance", short_name = "Reso",
            group = "Filter", range = "linear(0, 1)", smooth = "exp(5)")]
    pub resonance: FloatParam,

    #[param(name = "Attack", short_name = "Atk", group = "Envelope",
            range = "log(0.001, 5)", default = 0.01, unit = "s")]
    pub attack: FloatParam,

    #[param(name = "Decay", short_name = "Dec", group = "Envelope",
            range = "log(0.001, 5)", default = 0.1, unit = "s")]
    pub decay: FloatParam,

    #[param(name = "Sustain", short_name = "Sus", group = "Envelope",
            range = "linear(0, 1)", default = 0.7)]
    pub sustain: FloatParam,

    #[param(name = "Release", short_name = "Rel", group = "Envelope",
            range = "log(0.01, 10)", default = 0.3, unit = "s")]
    pub release: FloatParam,

    #[param(name = "Volume", short_name = "Vol",
            range = "linear(-60, 0)", default = -6, unit = "dB",
            smooth = "exp(5)")]
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
        let attack = self.params.attack.value() as f64;
        let decay = self.params.decay.value() as f64;
        let sustain = self.params.sustain.value() as f64;
        let release = self.params.release.value() as f64;

        self.voices.push(Voice::new(
            note, freq, velocity, self.sample_rate, attack, decay, sustain, release,
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
        // Smoother methods take &self (atomic internals)
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
                if event.sample_offset as usize > i { break; }
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
        if self.voices.is_empty() { ProcessStatus::Tail(0) } else { ProcessStatus::Normal }
    }

    fn layout(&self) -> truce_gui::layout::GridLayout {
        GridLayout::build("TRUCE SYNTH", "V0.1", 4, 70.0, vec![
            widgets(vec![
                dropdown(P::Waveform, "Wave").cols(2),
                knob(P::Volume, "Volume"),
            ]),
            section("FILTER", vec![
                knob(P::Cutoff, "Cutoff"),
                knob(P::Resonance, "Reso"),
            ]),
            section("ENVELOPE", vec![
                knob(P::Attack, "Attack"),
                knob(P::Decay, "Decay"),
                knob(P::Sustain, "Sustain"),
                knob(P::Release, "Release"),
            ]),
        ])
    }
}

// --- Export (one macro, all formats) ---

truce::plugin! {
    logic: Synth,
    params: SynthParams,
    bus_layouts: [BusLayout::new().with_output("Main", ChannelConfig::Stereo)],
}
```

---


---

[← Previous](06-channels.md) | [Next →](08-gui.md) | [Index](README.md)
