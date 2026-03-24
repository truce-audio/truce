## Building a synthesizer

Here is the actual synth example from `examples/synth/` showing
how instruments work. Like the gain example, everything lives in a
single crate with `src/lib.rs` and `src/main.rs`.

### src/lib.rs

```rust
use truce::params::{EnumParam, FloatParam, ParamEnum};
use truce::prelude::*;
use truce_params_derive::Params;

mod voice;
use voice::Voice;

// --- Waveform enum ---

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Waveform { Sine, Saw, Square, Triangle }

impl ParamEnum for Waveform {
    fn from_index(index: usize) -> Self {
        match index {
            0 => Self::Sine, 1 => Self::Saw,
            2 => Self::Square, _ => Self::Triangle,
        }
    }
    fn to_index(&self) -> usize { *self as usize }
    fn name(&self) -> &'static str {
        match self {
            Self::Sine => "Sine", Self::Saw => "Saw",
            Self::Square => "Square", Self::Triangle => "Triangle",
        }
    }
    fn variant_count() -> usize { 4 }
    fn variant_names() -> &'static [&'static str] {
        &["Sine", "Saw", "Square", "Triangle"]
    }
}

// --- Parameters ---

#[derive(Params)]
pub struct SynthParams {
    #[param(id = 0, name = "Waveform", short_name = "Wave",
            range = "enum(4)", default = 1)]
    pub waveform: EnumParam<Waveform>,

    #[param(id = 1, name = "Filter Cutoff", short_name = "Cutoff",
            group = "Filter", range = "log(20, 20000)", default = 8000,
            unit = "Hz", smooth = "exp(5)")]
    pub cutoff: FloatParam,

    #[param(id = 2, name = "Filter Resonance", short_name = "Reso",
            group = "Filter", range = "linear(0, 1)", smooth = "exp(5)")]
    pub resonance: FloatParam,

    #[param(id = 3, name = "Attack", short_name = "Atk", group = "Envelope",
            range = "log(0.001, 5)", default = 0.01, unit = "s")]
    pub attack: FloatParam,

    #[param(id = 4, name = "Decay", short_name = "Dec", group = "Envelope",
            range = "log(0.001, 5)", default = 0.1, unit = "s")]
    pub decay: FloatParam,

    #[param(id = 5, name = "Sustain", short_name = "Sus", group = "Envelope",
            range = "linear(0, 1)", default = 0.7)]
    pub sustain: FloatParam,

    #[param(id = 6, name = "Release", short_name = "Rel", group = "Envelope",
            range = "log(0.01, 10)", default = 0.3, unit = "s")]
    pub release: FloatParam,

    #[param(id = 7, name = "Volume", short_name = "Vol",
            range = "linear(-60, 0)", default = -6, unit = "dB",
            smooth = "exp(5)")]
    pub volume: FloatParam,
}

// Use the generated param ID enum for type-safe references
use SynthParamsParamId as P;

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
    fn bus_layouts() -> Vec<BusLayout> {
        // Instrument: no audio input, stereo output
        vec![BusLayout::new().with_output("Main", ChannelConfig::Stereo)]
    }

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

    fn layout(&self) -> truce_gui::layout::PluginLayout {
        truce_gui::layout!("TRUCE SYNTH", "V0.1", 70.0, {
            row {
                selector(P::Waveform, "Wave") .span(2)
                knob(P::Volume, "Volume")
            }
            section("FILTER") {
                knob(P::Cutoff, "Cutoff")
                knob(P::Resonance, "Reso")
            }
            section("ENVELOPE") {
                knob(P::Attack, "Attack")
                knob(P::Decay, "Decay")
                knob(P::Sustain, "Sustain")
                knob(P::Release, "Release")
            }
        })
    }
}

// --- Export (one macro, all formats) ---

truce::plugin! { logic: Synth, params: SynthParams }
```

---


---

[← Previous](06-channels.md) | [Next →](08-gui.md) | [Index](README.md)
