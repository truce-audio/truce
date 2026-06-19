//! Monophonic sine instrument that plays incoming notes *and* emits
//! MIDI back to the host: it echoes each note and sends a per-block
//! channel-pressure (mono aftertouch) ramp while a note is held.
//!
//! Exercises an instrument (audio output, not a note effect) declaring
//! a MIDI output port, which AU v3, VST3, and LV2 only do for a plugin
//! that sets `midi_output = true` in truce.toml.

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, widgets};

use MidiSynthParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct MidiSynthParams {
    #[param(
        name = "Gain",
        range = "linear(0, 1)",
        default = 0.8,
        unit = "%",
        smooth = "exp(5)"
    )]
    pub gain: FloatParam,
}

pub struct MidiSynth {
    params: Arc<MidiSynthParams>,
    sample_rate: f64,
    /// Currently sounding note, or `None` when silent.
    note: Option<u8>,
    phase: f64,
    /// Phase of the slow aftertouch LFO emitted while a note is held.
    pressure_phase: f64,
}

impl MidiSynth {
    pub fn new(params: Arc<MidiSynthParams>) -> Self {
        Self {
            params,
            sample_rate: 44100.0,
            note: None,
            phase: 0.0,
            pressure_phase: 0.0,
        }
    }
}

/// Frequency in Hz for a MIDI note number, A4 (note 69) = 440 Hz.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn note_hz(note: u8) -> f64 {
    440.0 * 2.0_f64.powf((f64::from(note) - 69.0) / 12.0)
}

/// Unipolar `[0, 1]` level to a 7-bit MIDI value.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn level_to_7bit(level: f64) -> u8 {
    (level.clamp(0.0, 1.0) * 127.0).round() as u8
}

/// f64 sine sample as f32 - the cast is the DSP output boundary.
#[allow(clippy::cast_possible_truncation)]
fn sine_f32(phase: f64) -> f32 {
    (phase * std::f64::consts::TAU).sin() as f32
}

impl PluginLogic for MidiSynth {
    fn bus_layouts() -> Vec<BusLayout> {
        // Instrument: audio output only, no input.
        vec![BusLayout::new().with_output("Main", ChannelConfig::Stereo)]
    }

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.sample_rate = sample_rate;
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
        self.note = None;
        self.phase = 0.0;
        self.pressure_phase = 0.0;
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Trigger from note input, and echo every note to the host so a
        // downstream plugin sees what we're playing.
        for event in events.iter() {
            match &event.body {
                EventBody::NoteOn { note, .. } => {
                    self.note = Some(*note);
                    self.phase = 0.0;
                }
                EventBody::NoteOff { note, .. } if self.note == Some(*note) => {
                    self.note = None;
                }
                _ => {}
            }
            context.output_events.push(*event);
        }

        let gain = self.params.gain.read();
        let inc = self.note.map_or(0.0, |n| note_hz(n) / self.sample_rate);

        for i in 0..buffer.num_samples() {
            let sample = if self.note.is_some() {
                sine_f32(self.phase) * gain
            } else {
                0.0
            };
            // Instrument: write the output buses directly (there's no
            // input to pair against via `io`).
            buffer.output(0)[i] = sample;
            buffer.output(1)[i] = sample;
            self.phase = (self.phase + inc).rem_euclid(1.0);
        }

        // While a note sounds, emit a slow channel-pressure ramp so the
        // host receives more than just note events.
        if self.note.is_some() {
            let lfo = 0.5 - 0.5 * (self.pressure_phase * std::f64::consts::TAU).cos();
            context.output_events.push(Event {
                sample_offset: 0,
                body: EventBody::ChannelPressure {
                    group: 0,
                    channel: 0,
                    pressure: level_to_7bit(lfo),
                },
            });
            let pps = 4.0 / self.sample_rate * num_samples_f64(buffer.num_samples());
            self.pressure_phase = (self.pressure_phase + pps).rem_euclid(1.0);
        }

        ProcessStatus::Normal
    }

    fn editor(&self) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![knob(P::Gain, "Gain")])])
            .with_title("MIDI SYNTH")
            .into_editor(&self.params)
    }
}

/// Block size as `f64` for phase math (small integers, exact in f64).
#[allow(clippy::cast_precision_loss)]
fn num_samples_f64(n: usize) -> f64 {
    n as f64
}

truce::plugin! {
    logic: MidiSynth,
    params: MidiSynthParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn instrument_emits_midi() {
        use truce_core::info::PluginCategory;
        use truce_core::plugin::PluginRuntime;
        let info = <Plugin as PluginRuntime>::info();
        assert_eq!(info.category, PluginCategory::Instrument);
        assert!(
            info.emits_midi,
            "instrument with midi_output = true emits MIDI"
        );
    }

    #[test]
    fn echoes_note_and_emits_pressure() {
        let params = Arc::new(MidiSynthParams::new());
        let mut plugin = MidiSynth::new(Arc::clone(&params));
        plugin.params.gain.set_value(1.0);
        plugin.reset(44100.0, 256);

        // Instrument: output-only buffer (no inputs).
        let input_refs: Vec<&[f32]> = Vec::new();
        let mut output = vec![vec![0.0f32; 256]; 2];
        let mut output_refs: Vec<&mut [f32]> =
            output.iter_mut().map(std::vec::Vec::as_mut_slice).collect();
        let mut buffer = unsafe { AudioBuffer::from_slices(&input_refs, &mut output_refs, 256) };

        let mut events = EventList::default();
        events.push(Event {
            sample_offset: 0,
            body: EventBody::NoteOn {
                group: 0,
                channel: 0,
                note: 69,
                velocity: 100,
            },
        });

        let transport = TransportInfo::default();
        let mut output_events = EventList::default();
        let mut context = ProcessContext::new(&transport, 44100.0, 256, &mut output_events);
        plugin.process(&mut buffer, &events, &mut context);

        let echoed_note = output_events
            .iter()
            .any(|e| matches!(e.body, EventBody::NoteOn { note: 69, .. }));
        let pressure = output_events
            .iter()
            .any(|e| matches!(e.body, EventBody::ChannelPressure { .. }));
        assert!(echoed_note, "note should be echoed to the host");
        assert!(pressure, "a channel-pressure event should be emitted");
        // The synth produced audio for the held note.
        assert!(output[0].iter().any(|s| *s != 0.0), "expected audio output");
    }
}
