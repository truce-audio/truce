//! `mpe-synth` - a per-note-expressive MIDI 2.0 synth.
//!
//! A test vehicle for truce's 1.1.0 MIDI 2.0 **decode** and **multi-port
//! input** surface, and the render half of the `mpe-spreader` pair: it
//! makes every address dimension the spreader fans over audible.
//!
//! - `NoteOn2` 16-bit velocity -> amplitude (finer than 7-bit steps).
//! - `PerNotePitchBend` (32-bit) -> per-voice detune (MPE-style).
//! - `PerNoteCC` 74 (brightness) -> per-voice filter cutoff.
//! - `PitchBend2` (32-bit) -> channel-wide bend.
//! - `ControlChange2` 74 -> the master-cutoff macro (drives the param).
//! - UMP **group** -> oscillator waveform (multi-timbral).
//! - MIDI **channel** -> equal-power stereo pan (`Channel Fan` spreads a
//!   chord across the field).
//! - `Event.port` -> octave, and part of the voice key: a note on port 1
//!   is a distinct voice an octave above the same note on port 0.
//!
//! Every 2.0 arm has a 1.0 sibling, so it plays on VST2 / AU v2 / AAX /
//! LV2 too - just at 7/14-bit resolution and group 0.

use std::f64::consts::TAU;
use std::sync::Arc;

use truce::prelude::*;
use truce_core::midi::norm_7bit;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, widgets};

use SynthParamsParamId as P;

const NUM_VOICES: usize = 32;
/// Per-note pitch-bend range (MPE convention), in semitones each way.
const PER_NOTE_BEND_SEMIS: f64 = 48.0;
/// Channel pitch-bend range, in semitones each way.
const CHANNEL_BEND_SEMIS: f64 = 2.0;
/// Input port -> octave: port 1 plays an octave above port 0, so a
/// `Port Fan` note stream splits into octaves.
const PORT_OCTAVE_SEMIS: f64 = 12.0;
/// MIDI 2.0 per-note brightness controller number.
const CC_BRIGHTNESS: u8 = 74;
/// `0x8000_0000` as f64 - the centre of a 32-bit signed-ish control.
const HALF_U32: f64 = 2_147_483_648.0;

#[derive(Params)]
pub struct SynthParams {
    #[param(
        name = "Master Cutoff",
        range = "linear(0, 1)",
        default = 0.7,
        unit = "%"
    )]
    pub master_cutoff: FloatParam,
    #[param(name = "Release", range = "linear(0.01, 4)", default = 0.4, unit = "s")]
    pub release: FloatParam,
    #[param(name = "Volume", range = "linear(-60, 6)", default = -6.0, unit = "dB")]
    pub volume: FloatParam,
}

#[derive(Clone, Copy, Default)]
struct Voice {
    active: bool,
    releasing: bool,
    port: u8,
    group: u8,
    channel: u8,
    note: u8,
    phase: f64,
    base_freq: f64,
    /// Per-note bend, semitones.
    bend_semis: f64,
    /// Velocity amplitude, 0..1.
    amp: f32,
    /// Per-note brightness, 0..1 (default full).
    cutoff: f32,
    /// Amp envelope level, 0..1.
    env: f32,
    /// One-pole low-pass state.
    lp: f32,
    /// Equal-power stereo gains, derived from the MIDI channel at note-on.
    pan_l: f32,
    pan_r: f32,
}

pub struct Synth {
    params: Arc<SynthParams>,
    sample_rate: f64,
    voices: [Voice; NUM_VOICES],
    /// Channel-wide pitch bend, semitones, indexed by channel.
    channel_bend: [f64; 16],
}

impl Synth {
    #[must_use]
    pub fn new(params: Arc<SynthParams>) -> Self {
        Self {
            params,
            sample_rate: 44100.0,
            voices: [Voice::default(); NUM_VOICES],
            channel_bend: [0.0; 16],
        }
    }

    fn note_on(&mut self, port: u8, group: u8, channel: u8, note: u8, amp: f32) {
        let slot = self.voices.iter().position(|v| !v.active).unwrap_or(0); // steal voice 0 if the pool is full
        let (pan_l, pan_r) = pan_gains(channel);
        self.voices[slot] = Voice {
            active: true,
            releasing: false,
            port,
            group,
            channel,
            note,
            phase: 0.0,
            base_freq: midi_note_to_freq(note),
            bend_semis: 0.0,
            amp,
            cutoff: 1.0,
            env: 0.0,
            lp: 0.0,
            pan_l,
            pan_r,
        };
    }

    fn find(&mut self, port: u8, group: u8, channel: u8, note: u8) -> Option<&mut Voice> {
        self.voices.iter_mut().find(|v| {
            v.active && v.port == port && v.group == group && v.channel == channel && v.note == note
        })
    }

    fn handle(&mut self, port: u8, body: EventBody) {
        match body {
            EventBody::NoteOn {
                group,
                channel,
                note,
                velocity,
            } => self.note_on(port, group, channel, note, norm_7bit(velocity)),
            EventBody::NoteOn2 {
                group,
                channel,
                note,
                velocity,
                ..
            } => self.note_on(port, group, channel, note, amp16(velocity)),
            EventBody::NoteOff {
                group,
                channel,
                note,
                ..
            }
            | EventBody::NoteOff2 {
                group,
                channel,
                note,
                ..
            } => {
                if let Some(v) = self.find(port, group, channel, note) {
                    v.releasing = true;
                }
            }
            EventBody::PerNotePitchBend {
                group,
                channel,
                note,
                value,
            } => {
                if let Some(v) = self.find(port, group, channel, note) {
                    v.bend_semis = bend32_semis(value, PER_NOTE_BEND_SEMIS);
                }
            }
            EventBody::PerNoteCC {
                group,
                channel,
                note,
                cc: CC_BRIGHTNESS,
                value,
                ..
            } => {
                if let Some(v) = self.find(port, group, channel, note) {
                    v.cutoff = unit32(value);
                }
            }
            EventBody::PitchBend2 { channel, value, .. } => {
                self.channel_bend[usize::from(channel)] = bend32_semis(value, CHANNEL_BEND_SEMIS);
            }
            EventBody::PitchBend { channel, value, .. } => {
                self.channel_bend[usize::from(channel)] = bend14_semis(value);
            }
            // 32-bit macro CC drives the master-cutoff param at full res.
            EventBody::ControlChange2 {
                cc: CC_BRIGHTNESS,
                value,
                ..
            } => self
                .params
                .master_cutoff
                .set_value(f64::from(unit32(value))),
            EventBody::ControlChange {
                cc: CC_BRIGHTNESS,
                value,
                ..
            } => self
                .params
                .master_cutoff
                .set_value(f64::from(norm_7bit(value))),
            _ => {}
        }
    }
}

// 16-bit velocity -> 0..1 amplitude.
fn amp16(v: u16) -> f32 {
    f32::from(v) / f32::from(u16::MAX)
}

// 32-bit control value -> 0..1.
#[allow(clippy::cast_possible_truncation)]
fn unit32(v: u32) -> f32 {
    (f64::from(v) / f64::from(u32::MAX)) as f32
}

// 32-bit pitch bend (centre `0x8000_0000`) -> +-`range` semitones.
fn bend32_semis(v: u32, range: f64) -> f64 {
    ((f64::from(v) - HALF_U32) / HALF_U32) * range
}

// 14-bit pitch bend (centre 8192) -> +-CHANNEL_BEND_SEMIS.
fn bend14_semis(v: u16) -> f64 {
    ((f64::from(v) - 8192.0) / 8192.0) * CHANNEL_BEND_SEMIS
}

// MIDI channel -> equal-power stereo gains: channel 0 hard left, channel
// 15 hard right, so a `Channel Fan` note stream spreads across the field.
fn pan_gains(channel: u8) -> (f32, f32) {
    let pan = f32::from(channel.min(15)) / 15.0;
    let angle = pan * std::f32::consts::FRAC_PI_2;
    (angle.cos(), angle.sin())
}

// Naive (not band-limited) oscillator; waveform picked by UMP group.
#[allow(clippy::cast_possible_truncation)]
fn osc(group: u8, phase: f64) -> f32 {
    let s = match group % 4 {
        0 => 2.0 * phase - 1.0,                             // saw
        1 => f64::from(i8::from(phase >= 0.5)) * 2.0 - 1.0, // square
        2 => 4.0 * (phase - 0.5).abs() - 1.0,               // triangle
        _ => (phase * TAU).sin(),                           // sine
    };
    s as f32
}

impl PluginLogic for Synth {
    fn bus_layouts() -> Vec<BusLayout> {
        vec![BusLayout::new().with_output("Main", ChannelConfig::Stereo)]
    }

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
        self.sample_rate = sample_rate;
        self.voices = [Voice::default(); NUM_VOICES];
        self.channel_bend = [0.0; 16];
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        let n = buffer.num_samples();
        let master = self.params.master_cutoff.raw_smoothed_current();
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

            let mut left = 0.0f32;
            let mut right = 0.0f32;
            for v in self.voices.iter_mut().filter(|v| v.active) {
                let semis = v.bend_semis
                    + self.channel_bend[usize::from(v.channel)]
                    + f64::from(v.port) * PORT_OCTAVE_SEMIS;
                let freq = v.base_freq * 2.0_f64.powf(semis / 12.0);
                let raw = osc(v.group, v.phase);
                v.phase = (v.phase + freq / self.sample_rate).fract();
                // Per-voice cutoff scaled by the master macro.
                let co = (v.cutoff * master).clamp(0.002, 1.0);
                v.lp += co * (raw - v.lp);
                if v.releasing {
                    v.env -= rel_step;
                    if v.env <= 0.0 {
                        v.env = 0.0;
                        v.active = false;
                    }
                } else if v.env < 1.0 {
                    v.env = (v.env + atk_step).min(1.0);
                }
                let s = v.lp * v.env * v.amp;
                left += s * v.pan_l;
                right += s * v.pan_r;
            }

            buffer.output(0)[i] = (left * volume).clamp(-1.0, 1.0);
            buffer.output(1)[i] = (right * volume).clamp(-1.0, 1.0);
        }

        if self.voices.iter().any(|v| v.active) {
            ProcessStatus::Normal
        } else {
            ProcessStatus::Tail(0)
        }
    }

    fn editor(&self) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::MasterCutoff, "Cutoff"),
            knob(P::Release, "Release"),
            knob(P::Volume, "Volume"),
        ])])
        .with_title("MPE")
        .into_editor(&self.params)
    }
}

truce::plugin! {
    logic: Synth,
    params: SynthParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_full(input: &[Event]) -> (Synth, Vec<f32>, Vec<f32>) {
        let params = Arc::new(SynthParams::new());
        let mut plugin = Synth::new(Arc::clone(&params));
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
        (plugin, l, r)
    }

    fn render(input: &[Event]) -> (Synth, Vec<f32>) {
        let (plugin, l, _r) = render_full(input);
        (plugin, l)
    }

    fn render_stereo(input: &[Event]) -> (Vec<f32>, Vec<f32>) {
        let (_p, l, r) = render_full(input);
        (l, r)
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0, |m, s| m.max(s.abs()))
    }

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn note_on_2_makes_sound() {
        let (_p, out) = render(&[Event::new(
            0,
            EventBody::NoteOn2 {
                group: 0,
                channel: 0,
                note: 69,
                velocity: 0xFFFF,
                attribute_type: 0,
                attribute: 0,
            },
        )]);
        assert!(peak(&out) > 0.0, "a full-velocity note should be audible");
    }

    #[test]
    fn velocity_16_bit_scales_amplitude() {
        // A quarter-scale 16-bit velocity is quieter than full-scale, at
        // a resolution 7-bit velocity can't express.
        let full = peak(&render(&[note2(0xFFFF)]).1);
        let quiet = peak(&render(&[note2(0x4000)]).1);
        assert!(quiet < full * 0.5, "16-bit velocity should scale amp");
        assert!(quiet > 0.0);
    }

    #[test]
    fn port_and_group_key_distinct_voices() {
        // Same pitch on two ports and two groups = up to four voices.
        let (p, _) = render(&[note_on(0, 0, 60), note_on(1, 0, 60), note_on(0, 1, 60)]);
        let active = p.voices.iter().filter(|v| v.active).count();
        assert_eq!(active, 3);
    }

    #[test]
    fn per_note_pitch_bend_detunes_one_voice() {
        let (p, _) = render(&[
            note_on(0, 0, 60),
            Event::new(
                0,
                EventBody::PerNotePitchBend {
                    group: 0,
                    channel: 0,
                    note: 60,
                    value: 0xC000_0000, // above centre -> bend up
                },
            ),
        ]);
        let bent = p.voices.iter().find(|v| v.active).unwrap().bend_semis;
        assert!(bent > 0.0, "up-bend should raise the voice");
    }

    #[test]
    fn port_shifts_an_octave() {
        // Same note on port 0 and port 1; port 1 renders an octave up, so
        // its oscillator phase advances ~twice as far over the block.
        let (p, _) = render(&[note_on(0, 0, 60), note_on(1, 0, 60)]);
        let v0 = p.voices.iter().find(|v| v.active && v.port == 0).unwrap();
        let v1 = p.voices.iter().find(|v| v.active && v.port == 1).unwrap();
        let ratio = v1.phase / v0.phase;
        assert!(
            (ratio - 2.0).abs() < 0.05,
            "port 1 should be an octave up (got {ratio}x)"
        );
    }

    #[test]
    fn channel_pans_across_stereo_field() {
        // Channel 0 -> hard left, channel 15 -> hard right.
        let (l0, r0) = render_stereo(&[note_on_ch(0, 60)]);
        assert!(
            peak(&l0) > 0.0 && peak(&r0) < 1e-6,
            "channel 0 is hard left"
        );
        let (l15, r15) = render_stereo(&[note_on_ch(15, 60)]);
        assert!(
            peak(&r15) > 0.0 && peak(&l15) < 1e-6,
            "channel 15 is hard right"
        );
    }

    fn note_on_ch(channel: u8, note: u8) -> Event {
        Event::new(
            0,
            EventBody::NoteOn2 {
                group: 0,
                channel,
                note,
                velocity: 0x8000,
                attribute_type: 0,
                attribute: 0,
            },
        )
    }

    fn note2(velocity: u16) -> Event {
        Event::new(
            0,
            EventBody::NoteOn2 {
                group: 0,
                channel: 0,
                note: 69,
                velocity,
                attribute_type: 0,
                attribute: 0,
            },
        )
    }

    fn note_on(port: u8, group: u8, note: u8) -> Event {
        Event::on_port(
            0,
            port,
            EventBody::NoteOn2 {
                group,
                channel: 0,
                note,
                velocity: 0x8000,
                attribute_type: 0,
                attribute: 0,
            },
        )
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/mpe_synth_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/mpe_synth_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/mpe_synth_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
