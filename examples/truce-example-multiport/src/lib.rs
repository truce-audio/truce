//! `multiport` - a multi-port MIDI **input** instrument.
//!
//! One instance exposes two MIDI input ports; the host routes a separate
//! track to each and both play from the same plugin. This is the
//! multi-timbral paradigm of Kontakt / Vienna Ensemble Pro: a single
//! instance addressed by port so it can hold more than 16 channels' worth
//! of parts, each with its own sound.
//!
//! Each port is a distinct patch, so two routed tracks make two different
//! sounds:
//!
//! - **Port 0** -> a pure sine pad.
//! - **Port 1** -> a buzzy saw lead.
//!
//! Voices are keyed by `(port, channel, note)`, so the same pitch on both
//! ports is two independent voices. VST3 (event input buses) and CLAP
//! (note ports) carry the port to the plugin; formats without a multi-port
//! MIDI transport clamp to one port, and everything plays patch 0.

use std::f64::consts::TAU;
use std::sync::Arc;

use truce::prelude::*;
use truce_core::midi::norm_7bit;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, widgets};

use MultiportParamsParamId as P;

const NUM_VOICES: usize = 16;

#[derive(Params)]
pub struct MultiportParams {
    #[param(name = "Cutoff", range = "linear(0, 1)", default = 0.7, unit = "%")]
    pub cutoff: FloatParam,
    #[param(name = "Release", range = "linear(0.01, 4)", default = 0.3, unit = "s")]
    pub release: FloatParam,
    #[param(name = "Volume", range = "linear(-60, 6)", default = -6.0, unit = "dB")]
    pub volume: FloatParam,
}

#[derive(Clone, Copy, Default)]
struct Voice {
    active: bool,
    releasing: bool,
    /// Input port the note arrived on - selects the patch and keys the voice.
    port: u8,
    channel: u8,
    note: u8,
    phase: f64,
    freq: f64,
    /// Velocity amplitude, 0..1.
    amp: f32,
    /// Amp envelope level, 0..1.
    env: f32,
    /// One-pole low-pass state.
    lp: f32,
}

pub struct Multiport {
    params: Arc<MultiportParams>,
    sample_rate: f64,
    voices: [Voice; NUM_VOICES],
}

impl Multiport {
    #[must_use]
    pub fn new(params: Arc<MultiportParams>) -> Self {
        Self {
            params,
            sample_rate: 44100.0,
            voices: [Voice::default(); NUM_VOICES],
        }
    }

    fn note_on(&mut self, port: u8, channel: u8, note: u8, amp: f32) {
        let slot = self.voices.iter().position(|v| !v.active).unwrap_or(0); // steal voice 0 if the pool is full
        self.voices[slot] = Voice {
            active: true,
            releasing: false,
            port,
            channel,
            note,
            phase: 0.0,
            freq: midi_note_to_freq(note),
            amp,
            env: 0.0,
            lp: 0.0,
        };
    }

    fn find(&mut self, port: u8, channel: u8, note: u8) -> Option<&mut Voice> {
        self.voices
            .iter_mut()
            .find(|v| v.active && v.port == port && v.channel == channel && v.note == note)
    }

    fn handle(&mut self, port: u8, body: EventBody) {
        match body {
            // Velocity-0 NoteOn is the MIDI 1.0 running-status note-off.
            EventBody::NoteOn {
                channel,
                note,
                velocity: 0,
                ..
            }
            | EventBody::NoteOff { channel, note, .. } => {
                if let Some(v) = self.find(port, channel, note) {
                    v.releasing = true;
                }
            }
            EventBody::NoteOn {
                channel,
                note,
                velocity,
                ..
            } => self.note_on(port, channel, note, norm_7bit(velocity)),
            _ => {}
        }
    }
}

// Naive (not band-limited) oscillator; the waveform is the patch, chosen
// by the input port the note arrived on.
#[allow(clippy::cast_possible_truncation)]
fn osc(port: u8, phase: f64) -> f32 {
    let s = if port == 0 {
        (phase * TAU).sin() // port 0: sine pad
    } else {
        2.0 * phase - 1.0 // port 1: saw lead
    };
    s as f32
}

impl PluginLogic for Multiport {
    fn bus_layouts() -> Vec<BusLayout> {
        vec![BusLayout::new().with_output("Main", ChannelConfig::Stereo)]
    }

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
        self.sample_rate = sample_rate;
        self.voices = [Voice::default(); NUM_VOICES];
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        let n = buffer.num_samples();
        let cutoff = self.params.cutoff.raw_smoothed_current().clamp(0.002, 1.0);
        let volume = db_to_linear(self.params.volume.raw_smoothed_current());
        let rel_s = self.params.release.raw_target().max(0.01);
        let atk_step = (1.0 / (0.005 * self.sample_rate)) as f32; // ~5 ms attack
        let rel_step = (1.0 / (rel_s * self.sample_rate)) as f32;

        let mut next = 0;
        for i in 0..n {
            while let Some(event) = events.get(next) {
                if event.sample_offset as usize > i {
                    break;
                }
                self.handle(event.port, event.body);
                next += 1;
            }

            let mut mix = 0.0f32;
            for v in self.voices.iter_mut().filter(|v| v.active) {
                let raw = osc(v.port, v.phase);
                v.phase = (v.phase + v.freq / self.sample_rate).fract();
                v.lp += cutoff * (raw - v.lp);
                if v.releasing {
                    v.env -= rel_step;
                    if v.env <= 0.0 {
                        v.env = 0.0;
                        v.active = false;
                    }
                } else if v.env < 1.0 {
                    v.env = (v.env + atk_step).min(1.0);
                }
                mix += v.lp * v.env * v.amp;
            }

            let s = (mix * volume).clamp(-1.0, 1.0);
            buffer.output(0)[i] = s;
            buffer.output(1)[i] = s;
        }

        if self.voices.iter().any(|v| v.active) {
            ProcessStatus::Normal
        } else {
            ProcessStatus::Tail(0)
        }
    }

    fn editor(&self) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::Cutoff, "Cutoff"),
            knob(P::Release, "Release"),
            knob(P::Volume, "Volume"),
        ])])
        .with_title("MULTIPORT")
        .into_editor(&self.params)
    }
}

truce::plugin! {
    logic: Multiport,
    params: MultiportParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(input: &[Event]) -> (Multiport, Vec<f32>) {
        let params = Arc::new(MultiportParams::new());
        let mut plugin = Multiport::new(Arc::clone(&params));
        plugin.reset(44100.0, 64);

        let in_refs: Vec<&[f32]> = Vec::new();
        let mut l = vec![0.0f32; 64];
        let mut r = vec![0.0f32; 64];
        let (a, b) = (&mut l[..], &mut r[..]);
        let mut out_refs: Vec<&mut [f32]> = vec![a, b];
        let mut buffer = unsafe { AudioBuffer::from_slices(&in_refs, &mut out_refs, 64) };

        let mut events = EventList::default();
        for e in input {
            events.push(*e);
        }
        let transport = TransportInfo::default();
        let mut out_ev = EventList::default();
        let mut ctx = ProcessContext::new(&transport, 44100.0, 64, &mut out_ev);
        plugin.process(&mut buffer, &events, &mut ctx);
        (plugin, l)
    }

    fn note_on_port(port: u8, note: u8) -> Event {
        Event::on_port(
            0,
            port,
            EventBody::NoteOn {
                group: 0,
                channel: 0,
                note,
                velocity: 100,
            },
        )
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0, |m, s| m.max(s.abs()))
    }

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn note_makes_sound() {
        let (_p, out) = render(&[note_on_port(0, 69)]);
        assert!(peak(&out) > 0.0, "a note should be audible");
    }

    #[test]
    fn ports_key_distinct_voices() {
        // The same pitch on both ports is two independent voices.
        let (p, _) = render(&[note_on_port(0, 60), note_on_port(1, 60)]);
        assert_eq!(p.voices.iter().filter(|v| v.active).count(), 2);
    }

    #[test]
    fn port_selects_distinct_patch() {
        // Port 0 is a sine, port 1 a saw: the same note renders different
        // waveforms, so the two output blocks differ sample-for-sample.
        let (_p0, a) = render(&[note_on_port(0, 60)]);
        let (_p1, b) = render(&[note_on_port(1, 60)]);
        assert!(
            a.iter().zip(&b).any(|(x, y)| (x - y).abs() > 1e-4),
            "sine and saw patches should render differently"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/multiport_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/multiport_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/multiport_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
