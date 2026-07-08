//! `mpe-synth` - a per-note-expressive MIDI 2.0 synth.
//!
//! A test vehicle for truce's 1.1.0 MIDI 2.0 **decode** surface, and the
//! render half of `mpe-spreader`: it makes the per-note expression the
//! spreader emits audible.
//!
//! - `NoteOn2` 16-bit velocity -> amplitude (finer than 7-bit steps).
//! - `PerNotePitchBend` (32-bit) -> per-voice detune (MPE-style).
//! - `PerNoteCC` 74 (brightness) -> per-voice filter cutoff.
//! - `PitchBend2` (32-bit) -> channel-wide bend.
//! - `ControlChange2` 74 -> the master-cutoff macro (drives the param).
//! - MIDI **channel** -> equal-power stereo pan (`Channel Fan` spreads a
//!   chord across the field).
//!
//! Every 2.0 arm has a 1.0 sibling, so it plays on VST2 / AU v2 / AAX /
//! LV2 too - just at 7/14-bit resolution.

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
    channel: u8,
    note: u8,
    phase: f64,
    base_freq: f64,
    /// Per-note bend, semitones.
    bend_semis: f64,
    /// Rendered frequency: `base_freq` with per-note + channel bend
    /// applied. Cached at event time - the `powf` behind it is far too
    /// hot for the per-voice per-sample render loop.
    freq: f64,
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

/// Stateless descriptor - the synth's per-block DSP state is [`SynthDspState`].
pub struct Synth;

#[derive(DspState)]
pub struct SynthDspState {
    sample_rate: f64,
    voices: [Voice; NUM_VOICES],
    /// Channel-wide pitch bend, semitones, indexed by channel.
    channel_bend: [f64; 16],
}

impl SynthDspState {
    /// Store a channel bend and retune every sounding voice on the
    /// channel so the cached `freq` tracks it.
    fn set_channel_bend(&mut self, channel: u8, semis: f64) {
        self.channel_bend[usize::from(channel)] = semis;
        for v in self
            .voices
            .iter_mut()
            .filter(|v| v.active && v.channel == channel)
        {
            Self::retune(v, semis);
        }
    }

    /// Equal-tempered bend ratio. Event-time only (`powf`).
    fn bend_ratio(semis: f64) -> f64 {
        2.0_f64.powf(semis / 12.0)
    }

    /// Re-derive a voice's cached `freq` from its base pitch and the
    /// current per-note + channel bends.
    fn retune(v: &mut Voice, channel_bend: f64) {
        v.freq = v.base_freq * Self::bend_ratio(v.bend_semis + channel_bend);
    }

    fn note_on(&mut self, channel: u8, note: u8, amp: f32) {
        // Prefer a free slot; if the pool is full, steal a voice
        // that's already fading (in release) before cutting a still-
        // held one; only then fall back to slot 0.
        let slot = self
            .voices
            .iter()
            .position(|v| !v.active)
            .or_else(|| self.voices.iter().position(|v| v.releasing))
            .unwrap_or(0);
        let (pan_l, pan_r) = pan_gains(channel);
        let base_freq = midi_note_to_freq(note);
        self.voices[slot] = Voice {
            active: true,
            releasing: false,
            channel,
            note,
            phase: 0.0,
            base_freq,
            bend_semis: 0.0,
            freq: base_freq * Self::bend_ratio(self.channel_bend[usize::from(channel)]),
            amp,
            cutoff: 1.0,
            env: 0.0,
            lp: 0.0,
            pan_l,
            pan_r,
        };
    }

    // Skip releasing voices: after a release + retrigger the pool
    // holds two voices for one (channel, note), and matching the
    // fading one would leave the fresh note's off unmatched - held
    // forever.
    fn find(&mut self, channel: u8, note: u8) -> Option<&mut Voice> {
        self.voices
            .iter_mut()
            .find(|v| v.active && !v.releasing && v.channel == channel && v.note == note)
    }

    fn handle(&mut self, params: &SynthParams, body: EventBody) {
        match body {
            EventBody::NoteOn {
                channel,
                note,
                velocity,
                ..
            } => self.note_on(channel, note, norm_7bit(velocity)),
            EventBody::NoteOn2 {
                channel,
                note,
                velocity,
                ..
            } => self.note_on(channel, note, amp16(velocity)),
            EventBody::NoteOff { channel, note, .. }
            | EventBody::NoteOff2 { channel, note, .. } => {
                if let Some(v) = self.find(channel, note) {
                    v.releasing = true;
                }
            }
            EventBody::PerNotePitchBend {
                channel,
                note,
                value,
                ..
            } => {
                let ch_bend = self.channel_bend[usize::from(channel)];
                if let Some(v) = self.find(channel, note) {
                    v.bend_semis = bend32_semis(value, PER_NOTE_BEND_SEMIS);
                    Self::retune(v, ch_bend);
                }
            }
            EventBody::PerNoteCC {
                channel,
                note,
                cc: CC_BRIGHTNESS,
                value,
                ..
            } => {
                if let Some(v) = self.find(channel, note) {
                    v.cutoff = unit32(value);
                }
            }
            EventBody::PitchBend2 { channel, value, .. } => {
                self.set_channel_bend(channel, bend32_semis(value, CHANNEL_BEND_SEMIS));
            }
            EventBody::PitchBend { channel, value, .. } => {
                self.set_channel_bend(channel, bend14_semis(value));
            }
            // 32-bit macro CC drives the master-cutoff param at full res.
            EventBody::ControlChange2 {
                cc: CC_BRIGHTNESS,
                value,
                ..
            } => params.master_cutoff.set_value(f64::from(unit32(value))),
            EventBody::ControlChange {
                cc: CC_BRIGHTNESS,
                value,
                ..
            } => params.master_cutoff.set_value(f64::from(norm_7bit(value))),
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

// Naive (not band-limited) saw oscillator.
#[allow(clippy::cast_possible_truncation)]
fn osc(phase: f64) -> f32 {
    (2.0 * phase - 1.0) as f32
}

impl PluginLogic for Synth {
    type Params = SynthParams;
    type DspState = SynthDspState;

    fn bus_layouts() -> Vec<BusLayout> {
        vec![BusLayout::new().with_output("Main", ChannelConfig::Stereo)]
    }

    fn init(_params: &SynthParams) -> SynthDspState {
        SynthDspState {
            sample_rate: 44100.0,
            voices: [Voice::default(); NUM_VOICES],
            channel_bend: [0.0; 16],
        }
    }

    fn reset(state: &mut SynthDspState, params: &SynthParams, config: &AudioConfig) {
        let sample_rate = config.sample_rate;
        params.set_sample_rate(sample_rate);
        params.snap_smoothers();
        state.sample_rate = sample_rate;
        state.voices = [Voice::default(); NUM_VOICES];
        state.channel_bend = [0.0; 16];
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn process(
        state: &mut SynthDspState,
        params: &SynthParams,
        buffer: &mut AudioBuffer,
        events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        let n = buffer.num_samples();
        // Advance the smoothers by the block (`raw_smoothed_current()`
        // alone never advances - the knobs would freeze at reset).
        let master = params.master_cutoff.read_after(n);
        let volume = db_to_linear(params.volume.read_after(n));
        let rel_s = params.release.raw_target().max(0.01);
        let atk_step = (1.0 / (0.005 * state.sample_rate)) as f32; // ~5 ms attack
        let rel_step = (1.0 / (rel_s * state.sample_rate)) as f32;

        let mut next = 0;
        // A mono or multi-mono host instance hands us a single output
        // channel; fold the stereo pan down instead of writing out of bounds.
        let out_channels = buffer.num_output_channels();
        for i in 0..n {
            while let Some(event) = events.get(next) {
                if event.sample_offset as usize > i {
                    break;
                }
                state.handle(params, event.body);
                next += 1;
            }

            let mut left = 0.0f32;
            let mut right = 0.0f32;
            for v in state.voices.iter_mut().filter(|v| v.active) {
                let freq = v.freq;
                let raw = osc(v.phase);
                v.phase = (v.phase + freq / state.sample_rate).fract();
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

            if out_channels > 1 {
                buffer.output(0)[i] = (left * volume).clamp(-1.0, 1.0);
                buffer.output(1)[i] = (right * volume).clamp(-1.0, 1.0);
            } else {
                buffer.output(0)[i] = ((left + right) * volume).clamp(-1.0, 1.0);
            }
        }

        if state.voices.iter().any(|v| v.active) {
            ProcessStatus::Normal
        } else {
            ProcessStatus::Tail(0)
        }
    }

    fn editor(params: Arc<SynthParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::MasterCutoff, "Cutoff"),
            knob(P::Release, "Release"),
            knob(P::Volume, "Volume"),
        ])])
        .with_title("MPE SYNTH")
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
                    s.set_param(P::MasterCutoff, 0.9);
                    s.wait_ms(15);
                    s.set_param(P::MasterCutoff, 0.1);
                    s.wait_ms(15);
                    s.note_off(60);
                })
                .run()
        });
    }

    fn render_full(input: &[Event]) -> (SynthDspState, Vec<f32>, Vec<f32>) {
        let params = SynthParams::new();
        let mut state = Synth::init(&params);
        Synth::reset(&mut state, &params, &AudioConfig::new(44100.0, 64));

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
        Synth::process(&mut state, &params, &mut buffer, &events, &mut ctx);
        (state, l, r)
    }

    fn render(input: &[Event]) -> (SynthDspState, Vec<f32>) {
        let (state, l, _r) = render_full(input);
        (state, l)
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
        let (_p, out) = render(&[note2(0xFFFF)]);
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
    fn per_note_pitch_bend_detunes_one_voice() {
        let (p, _) = render(&[
            note_on_ch(0, 60),
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

    fn note_off_ch(channel: u8, note: u8) -> Event {
        Event::new(
            0,
            EventBody::NoteOff2 {
                group: 0,
                channel,
                note,
                velocity: 0,
                attribute_type: 0,
                attribute: 0,
            },
        )
    }

    #[test]
    fn retriggered_note_is_not_stuck() {
        // Hold, release, then retrigger the same note before its
        // release tail dies, and release again - ordinary repeated
        // playing that leaves a decaying voice and a fresh voice for
        // one (channel, note). The second note-off must land on the
        // fresh voice; if it matched the already-releasing one, the
        // fresh note would hang forever.
        let (p, _) = render(&[
            note_on_ch(0, 69),
            note_off_ch(0, 69),
            note_on_ch(0, 69),
            note_off_ch(0, 69),
        ]);
        let held = p
            .voices
            .iter()
            .filter(|v| v.active && !v.releasing && v.note == 69)
            .count();
        assert_eq!(held, 0, "retriggered note left a voice held forever");
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
