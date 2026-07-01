//! Chord instrument: play one note, hear a triad (or 7th), and the
//! chord's notes are emitted to the host as MIDI - so the generated
//! chord can drive a second instrument (a pad, a sampler) while this
//! one sounds it.
//!
//! A practical "instrument that emits MIDI": it both produces audio and
//! sends notes back to the host, which AU v3, VST3, and LV2 only allow
//! for a plugin that sets `midi_output = true` in truce.toml.

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, dropdown, knob, widgets};

use ChordParamsParamId as P;
use std::sync::Arc;

/// Largest chord we voice (a 7th chord is four notes).
const MAX_VOICES: usize = 4;

/// Per-voice amplitude so a four-note chord can't clip. `1 / MAX_VOICES`.
const VOICE_MIX: f32 = 0.25;

/// Chord quality, as semitone offsets from the played root.
#[derive(ParamEnum)]
pub enum ChordType {
    Major,
    Minor,
    #[name = "Maj7"]
    Maj7,
    #[name = "Min7"]
    Min7,
    Sus4,
}

impl ChordType {
    fn intervals(self) -> &'static [i8] {
        match self {
            ChordType::Major => &[0, 4, 7],
            ChordType::Minor => &[0, 3, 7],
            ChordType::Maj7 => &[0, 4, 7, 11],
            ChordType::Min7 => &[0, 3, 7, 10],
            ChordType::Sus4 => &[0, 5, 7],
        }
    }
}

#[derive(Params)]
pub struct ChordParams {
    #[param(name = "Chord")]
    pub chord: EnumParam<ChordType>,

    #[param(
        name = "Gain",
        range = "linear(0, 1)",
        default = 0.8,
        unit = "%",
        smooth = "exp(5)"
    )]
    pub gain: FloatParam,
}

pub struct Chord {
    params: Arc<ChordParams>,
    sample_rate: f64,
    /// Notes currently sounding (and emitted), one per voice.
    notes: [Option<u8>; MAX_VOICES],
    phases: [f64; MAX_VOICES],
    /// Played root note, for matching its `NoteOff`.
    root: Option<u8>,
}

impl Chord {
    pub fn new(params: Arc<ChordParams>) -> Self {
        Self {
            params,
            sample_rate: 44100.0,
            notes: [None; MAX_VOICES],
            phases: [0.0; MAX_VOICES],
            root: None,
        }
    }

    /// Silence every sounding voice and emit a matching `NoteOff` for
    /// each, on `channel` / `group`.
    fn stop(&mut self, out: &mut EventList, offset: u32, group: u8, channel: u8) {
        for note in &mut self.notes {
            if let Some(n) = note.take() {
                out.push(Event::new(
                    offset,
                    EventBody::NoteOff {
                        group,
                        channel,
                        note: n,
                        velocity: 0,
                    },
                ));
            }
        }
        self.root = None;
    }
}

/// Frequency in Hz for a MIDI note number, A4 (note 69) = 440 Hz.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn note_hz(note: u8) -> f64 {
    440.0 * 2.0_f64.powf((f64::from(note) - 69.0) / 12.0)
}

/// Root note plus a signed interval, clamped to the MIDI range.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn chord_note(root: u8, interval: i8) -> u8 {
    (i32::from(root) + i32::from(interval)).clamp(0, 127) as u8
}

/// f64 sine sample as f32 - the DSP output boundary.
#[allow(clippy::cast_possible_truncation)]
fn sine_f32(phase: f64) -> f32 {
    (phase * std::f64::consts::TAU).sin() as f32
}

impl PluginLogic for Chord {
    fn bus_layouts() -> Vec<BusLayout> {
        // Instrument: audio output only.
        vec![BusLayout::new().with_output("Main", ChannelConfig::Stereo)]
    }

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.sample_rate = sample_rate;
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
        self.notes = [None; MAX_VOICES];
        self.phases = [0.0; MAX_VOICES];
        self.root = None;
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        for event in events.iter() {
            match &event.body {
                EventBody::NoteOn {
                    group,
                    channel,
                    note,
                    velocity,
                } => {
                    // Last-note priority: drop the previous chord first.
                    self.stop(context.output_events, event.sample_offset, *group, *channel);
                    self.root = Some(*note);
                    for (voice, interval) in
                        self.params.chord.value().intervals().iter().enumerate()
                    {
                        let chord_note = chord_note(*note, *interval);
                        self.notes[voice] = Some(chord_note);
                        self.phases[voice] = 0.0;
                        // Emit the chord note so a downstream instrument
                        // can play the same harmony.
                        context.output_events.push(Event::new(
                            event.sample_offset,
                            EventBody::NoteOn {
                                group: *group,
                                channel: *channel,
                                note: chord_note,
                                velocity: *velocity,
                            },
                        ));
                    }
                }
                EventBody::NoteOff {
                    group,
                    channel,
                    note,
                    ..
                } if self.root == Some(*note) => {
                    self.stop(context.output_events, event.sample_offset, *group, *channel);
                }
                _ => {}
            }
        }

        let gain = self.params.gain.read() * VOICE_MIX;
        for i in 0..buffer.num_samples() {
            let mut sample = 0.0f32;
            for voice in 0..MAX_VOICES {
                if let Some(note) = self.notes[voice] {
                    sample += sine_f32(self.phases[voice]);
                    let inc = note_hz(note) / self.sample_rate;
                    self.phases[voice] = (self.phases[voice] + inc).rem_euclid(1.0);
                }
            }
            sample *= gain;
            buffer.output(0)[i] = sample;
            buffer.output(1)[i] = sample;
        }

        ProcessStatus::Normal
    }

    fn editor(&self) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            dropdown(P::Chord, "Chord"),
            knob(P::Gain, "Gain"),
        ])])
        .with_title("CHORD")
        .into_editor(&self.params)
    }
}

truce::plugin! {
    logic: Chord,
    params: ChordParams,
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
    fn emits_triad_and_plays_audio() {
        let params = Arc::new(ChordParams::new());
        let mut plugin = Chord::new(Arc::clone(&params));
        plugin.params.gain.set_value(1.0);
        plugin.reset(44100.0, 256);

        let input_refs: Vec<&[f32]> = Vec::new();
        let mut output = vec![vec![0.0f32; 256]; 2];
        let mut output_refs: Vec<&mut [f32]> =
            output.iter_mut().map(std::vec::Vec::as_mut_slice).collect();
        let mut buffer = unsafe { AudioBuffer::from_slices(&input_refs, &mut output_refs, 256) };

        let mut events = EventList::default();
        events.push(Event::new(
            0,
            EventBody::NoteOn {
                group: 0,
                channel: 0,
                note: 60, // C4, default chord = Major
                velocity: 100,
            },
        ));

        let transport = TransportInfo::default();
        let mut output_events = EventList::default();
        let mut context = ProcessContext::new(&transport, 44100.0, 256, &mut output_events);
        plugin.process(&mut buffer, &events, &mut context);

        // C major triad emitted: C4, E4, G4 (60, 64, 67).
        let notes: Vec<u8> = output_events
            .iter()
            .filter_map(|e| match e.body {
                EventBody::NoteOn { note, .. } => Some(note),
                _ => None,
            })
            .collect();
        assert_eq!(notes, vec![60, 64, 67]);
        // ...and the chord is audible.
        assert!(output[0].iter().any(|s| *s != 0.0), "expected audio output");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/chord_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/chord_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/chord_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
