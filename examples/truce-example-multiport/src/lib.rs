//! `multiport` - a multi-port MIDI **input** instrument.
//!
//! One instance exposes two MIDI input ports; the host routes a separate
//! track to each and both play from the same plugin. This is the
//! multi-timbral paradigm of Kontakt / Vienna Ensemble Pro: a single
//! instance addressed by port so it can hold more than 16 channels' worth
//! of parts, each with its own sound.
//!
//! Each port is a full control lane (wave / cutoff / release / volume),
//! so two routed tracks make two independently-shaped sounds. The
//! defaults keep the classic split:
//!
//! - **Port 0** -> a pure sine pad.
//! - **Port 1** -> a buzzy saw lead.
//!
//! Voices are keyed by `(port, channel, note)`, so the same pitch on both
//! ports is two independent voices. VST3 (event input buses) and CLAP
//! (note ports) carry the port to the plugin; formats without a multi-port
//! MIDI transport clamp to one port, and everything plays lane 0.

use std::f64::consts::TAU;
use std::sync::Arc;

use truce::prelude::*;
use truce_core::midi::norm_7bit;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, dropdown, knob, section};

const NUM_VOICES: usize = 16;

/// Oscillator waveform per lane. `ParamEnum` derives `Clone` / `Copy` /
/// `PartialEq`.
#[derive(ParamEnum)]
pub enum Wave {
    #[name = "Sine"]
    Sine,
    #[name = "Saw"]
    Saw,
    #[name = "Square"]
    Square,
    #[name = "Triangle"]
    Triangle,
}

// One lane per input port. Two distinct structs rather than one reused
// type because a param's display name is fixed on the type, and hosts
// list both lanes' params in one flat namespace.
#[derive(Params)]
pub struct PortZeroLane {
    // Default Sine: port 0 is the pad lane.
    #[param(name = "P0 Wave", short_name = "P0Wave", group = "Port 0", default = 0)]
    pub wave: EnumParam<Wave>,
    #[param(
        name = "P0 Cutoff",
        short_name = "P0Cut",
        group = "Port 0",
        range = "linear(0, 1)",
        default = 0.7,
        unit = "%"
    )]
    pub cutoff: FloatParam,
    #[param(
        name = "P0 Release",
        short_name = "P0Rel",
        group = "Port 0",
        range = "linear(0.01, 4)",
        default = 0.3,
        unit = "s"
    )]
    pub release: FloatParam,
    #[param(
        name = "P0 Volume",
        short_name = "P0Vol",
        group = "Port 0",
        range = "linear(-60, 6)",
        default = -6.0,
        unit = "dB"
    )]
    pub volume: FloatParam,
}

#[derive(Params)]
pub struct PortOneLane {
    // Default Saw: port 1 is the lead lane.
    #[param(name = "P1 Wave", short_name = "P1Wave", group = "Port 1", default = 1)]
    pub wave: EnumParam<Wave>,
    #[param(
        name = "P1 Cutoff",
        short_name = "P1Cut",
        group = "Port 1",
        range = "linear(0, 1)",
        default = 0.7,
        unit = "%"
    )]
    pub cutoff: FloatParam,
    #[param(
        name = "P1 Release",
        short_name = "P1Rel",
        group = "Port 1",
        range = "linear(0.01, 4)",
        default = 0.3,
        unit = "s"
    )]
    pub release: FloatParam,
    #[param(
        name = "P1 Volume",
        short_name = "P1Vol",
        group = "Port 1",
        range = "linear(-60, 6)",
        default = -6.0,
        unit = "dB"
    )]
    pub volume: FloatParam,
}

// Bases pinned (0 / 4) so the flattened ids survive adding a param to
// lane 0 without shifting lane 1 - the stability saved state and host
// automation depend on.
#[derive(Params)]
pub struct MultiportParams {
    #[nested(base = 0)]
    pub port0: PortZeroLane,
    #[nested(base = 4)]
    pub port1: PortOneLane,
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

/// Stateless descriptor. The per-block DSP state lives in [`MultiportDspState`].
pub struct Multiport;

#[derive(DspState)]
pub struct MultiportDspState {
    sample_rate: f64,
    voices: [Voice; NUM_VOICES],
}

impl MultiportDspState {
    fn note_on(&mut self, port: u8, channel: u8, note: u8, amp: f32) {
        // Prefer a free slot; if the pool is full, steal a voice
        // that's already fading (in release) before cutting a still-
        // held one; only then fall back to slot 0.
        let slot = self
            .voices
            .iter()
            .position(|v| !v.active)
            .or_else(|| self.voices.iter().position(|v| v.releasing))
            .unwrap_or(0);
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
        // Target a still-sounding voice, never one already in
        // release: retriggering a note before its tail dies leaves a
        // decaying voice and a fresh one for the same note, and a
        // note-off must release the fresh one - matching the decaying
        // voice first would leave the fresh note held forever.
        self.voices.iter_mut().find(|v| {
            v.active && !v.releasing && v.port == port && v.channel == channel && v.note == note
        })
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

// Naive (not band-limited) oscillator; the waveform comes from the
// lane of the input port the note arrived on.
#[allow(clippy::cast_possible_truncation)]
fn osc(wave: Wave, phase: f64) -> f32 {
    let s = match wave {
        Wave::Sine => (phase * TAU).sin(),
        Wave::Saw => 2.0 * phase - 1.0,
        Wave::Square => f64::from(i8::from(phase >= 0.5)) * 2.0 - 1.0,
        Wave::Triangle => 4.0 * (phase - 0.5).abs() - 1.0,
    };
    s as f32
}

/// Per-block snapshot of one lane's controls, in render-ready units.
#[derive(Clone, Copy)]
struct Lane {
    wave: Wave,
    cutoff: f32,
    volume: f32,
    /// Per-sample release-envelope decrement.
    rel_step: f32,
}

impl PluginLogic for Multiport {
    type Params = MultiportParams;
    type DspState = MultiportDspState;

    fn bus_layouts() -> Vec<BusLayout> {
        vec![BusLayout::new().with_output("Main", ChannelConfig::Stereo)]
    }

    fn init(_params: &MultiportParams) -> MultiportDspState {
        MultiportDspState {
            sample_rate: 44100.0,
            voices: [Voice::default(); NUM_VOICES],
        }
    }

    fn reset(state: &mut MultiportDspState, params: &MultiportParams, config: &AudioConfig) {
        let sample_rate = config.sample_rate;
        params.set_sample_rate(sample_rate);
        params.snap_smoothers();
        state.sample_rate = sample_rate;
        state.voices = [Voice::default(); NUM_VOICES];
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn process(
        state: &mut MultiportDspState,
        params: &MultiportParams,
        buffer: &mut AudioBuffer,
        events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        let n = buffer.num_samples();
        let sample_rate = state.sample_rate;
        let atk_step = (1.0 / (0.005 * sample_rate)) as f32; // ~5 ms attack
        let p0 = &params.port0;
        let p1 = &params.port1;
        // `read_after(n)` advances the smoother by the block and hands
        // back the settled value - `raw_smoothed_current()` alone never
        // advances, freezing the knob at its reset-time snapshot.
        let lanes = [
            Lane {
                wave: p0.wave.value(),
                cutoff: p0.cutoff.read_after(n).clamp(0.002, 1.0),
                volume: db_to_linear(p0.volume.read_after(n)),
                rel_step: (1.0 / (p0.release.raw_target().max(0.01) * sample_rate)) as f32,
            },
            Lane {
                wave: p1.wave.value(),
                cutoff: p1.cutoff.read_after(n).clamp(0.002, 1.0),
                volume: db_to_linear(p1.volume.read_after(n)),
                rel_step: (1.0 / (p1.release.raw_target().max(0.01) * sample_rate)) as f32,
            },
        ];

        let mut next = 0;
        for i in 0..n {
            while let Some(event) = events.get(next) {
                if event.sample_offset as usize > i {
                    break;
                }
                state.handle(event.port, event.body);
                next += 1;
            }

            let mut mix = 0.0f32;
            for v in state.voices.iter_mut().filter(|v| v.active) {
                let lane = &lanes[usize::from(v.port.min(1))];
                let raw = osc(lane.wave, v.phase);
                v.phase = (v.phase + v.freq / sample_rate).fract();
                v.lp += lane.cutoff * (raw - v.lp);
                if v.releasing {
                    v.env -= lane.rel_step;
                    if v.env <= 0.0 {
                        v.env = 0.0;
                        v.active = false;
                    }
                } else if v.env < 1.0 {
                    v.env = (v.env + atk_step).min(1.0);
                }
                mix += v.lp * v.env * v.amp * lane.volume;
            }

            let s = mix.clamp(-1.0, 1.0);
            buffer.output(0)[i] = s;
            buffer.output(1)[i] = s;
        }

        if state.voices.iter().any(|v| v.active) {
            ProcessStatus::Normal
        } else {
            ProcessStatus::Tail(0)
        }
    }

    fn editor(params: Arc<MultiportParams>) -> Box<dyn Editor> {
        // Nested-group params are addressed by their flattened id
        // (`base + local`), read off each lane.
        GridLayout::build(vec![
            section(
                "PORT 0",
                vec![
                    dropdown(params.port0.wave.id(), "Wave"),
                    knob(params.port0.cutoff.id(), "Cutoff"),
                    knob(params.port0.release.id(), "Release"),
                    knob(params.port0.volume.id(), "Volume"),
                ],
            ),
            section(
                "PORT 1",
                vec![
                    dropdown(params.port1.wave.id(), "Wave"),
                    knob(params.port1.cutoff.id(), "Cutoff"),
                    knob(params.port1.release.id(), "Release"),
                    knob(params.port1.volume.id(), "Volume"),
                ],
            ),
        ])
        .with_title("MULTIPORT")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: Multiport,
    params: MultiportParams,
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
                    s.set_param(PortZeroLaneParamId::Wave, 0.9);
                    s.wait_ms(15);
                    s.set_param(PortZeroLaneParamId::Wave, 0.1);
                    s.wait_ms(15);
                    s.note_off(60);
                })
                .run()
        });
    }

    fn render(input: &[Event]) -> (MultiportDspState, Vec<f32>) {
        render_with(|_| {}, input)
    }

    #[test]
    fn knob_turned_mid_session_changes_the_sound() {
        // Regression: cutoff/volume were read with
        // `raw_smoothed_current()`, which never advances the smoother -
        // a knob turned after `reset()` (i.e. any turn ever made in a
        // running host) moved the target but the audible value stayed
        // frozen at its reset-time snapshot. Only `render_with`'s
        // pre-reset tweaks (snapped by `reset`) worked, which is why
        // no other test caught it.
        let params = MultiportParams::new();
        let mut state = Multiport::init(&params);
        Multiport::reset(&mut state, &params, &AudioConfig::new(44100.0, 64));

        let mut block = |events: &EventList| -> f32 {
            let in_refs: Vec<&[f32]> = Vec::new();
            let mut l = vec![0.0f32; 64];
            let mut r = vec![0.0f32; 64];
            let (a, b) = (&mut l[..], &mut r[..]);
            let mut out_refs: Vec<&mut [f32]> = vec![a, b];
            let mut buffer = unsafe { AudioBuffer::from_slices(&in_refs, &mut out_refs, 64) };
            let transport = TransportInfo::default();
            let mut out_ev = EventList::default();
            let mut ctx = ProcessContext::new(&transport, 44100.0, 64, &mut out_ev);
            Multiport::process(&mut state, &params, &mut buffer, events, &mut ctx);
            l.iter().fold(0.0f32, |m, s| m.max(s.abs()))
        };

        // Hold a note and let the attack settle.
        let mut on = EventList::default();
        on.push(note_on_port(0, 60));
        let silent = EventList::default();
        block(&on);
        for _ in 0..10 {
            block(&silent);
        }
        let loud = block(&silent);
        assert!(loud > 0.01, "held note should be audible");

        // Turn the volume down mid-session and let the smoother settle
        // (~30 ms of blocks against a 5 ms time constant).
        params.port0.volume.set_value(-60.0);
        for _ in 0..30 {
            block(&silent);
        }
        let quiet = block(&silent);
        assert!(
            quiet < loud * 0.1,
            "volume knob had no effect mid-session: {loud} -> {quiet}"
        );
    }

    // Render one block with the given param tweaks applied before
    // `reset` (which snaps smoothers onto the tweaked values).
    fn render_with(
        setup: impl Fn(&MultiportParams),
        input: &[Event],
    ) -> (MultiportDspState, Vec<f32>) {
        let params = MultiportParams::new();
        setup(&params);
        let mut state = Multiport::init(&params);
        Multiport::reset(&mut state, &params, &AudioConfig::new(44100.0, 64));

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
        Multiport::process(&mut state, &params, &mut buffer, &events, &mut ctx);
        (state, l)
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

    fn note_off_port(port: u8, note: u8) -> Event {
        Event::on_port(
            0,
            port,
            EventBody::NoteOff {
                group: 0,
                channel: 0,
                note,
                velocity: 0,
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
    fn retriggered_note_is_not_stuck() {
        // Hold, release, then retrigger the same note before its
        // release tail dies, and release again - the legato / repeated
        // playing that leaves a decaying voice and a fresh voice for
        // one note. The second note-off must land on the fresh voice;
        // if it matched the already-releasing one, the fresh note
        // would hang forever.
        let (p, _) = render(&[
            note_on_port(0, 60),
            note_off_port(0, 60),
            note_on_port(0, 60),
            note_off_port(0, 60),
        ]);
        let held = p
            .voices
            .iter()
            .filter(|v| v.active && !v.releasing && v.note == 60)
            .count();
        assert_eq!(held, 0, "retriggered note left a voice held forever");
    }

    #[test]
    fn ports_key_distinct_voices() {
        // The same pitch on both ports is two independent voices.
        let (p, _) = render(&[note_on_port(0, 60), note_on_port(1, 60)]);
        assert_eq!(p.voices.iter().filter(|v| v.active).count(), 2);
    }

    #[test]
    fn port_selects_distinct_patch() {
        // Default lanes: port 0 sine, port 1 saw - the same note renders
        // different waveforms, so the two output blocks differ.
        let (_p0, a) = render(&[note_on_port(0, 60)]);
        let (_p1, b) = render(&[note_on_port(1, 60)]);
        assert!(
            a.iter().zip(&b).any(|(x, y)| (x - y).abs() > 1e-4),
            "sine and saw patches should render differently"
        );
    }

    #[test]
    fn lane_volume_is_per_port() {
        // Duck lane 1's volume only: a port-1 note gets quiet while the
        // same note on port 0 stays at full level.
        let duck = |p: &MultiportParams| p.port1.volume.set_value(-60.0);
        let (_p, loud) = render_with(duck, &[note_on_port(0, 60)]);
        let (_p, quiet) = render_with(duck, &[note_on_port(1, 60)]);
        assert!(
            peak(&quiet) < peak(&loud) * 0.01,
            "lane 1's volume must not affect lane 0 (loud={}, quiet={})",
            peak(&loud),
            peak(&quiet)
        );
    }

    #[test]
    fn lane_wave_is_selectable() {
        // Switch lane 0 to Saw: both ports now render the same waveform
        // (all other lane controls at defaults), so the blocks match.
        let (_p, a) = render_with(
            |p| p.port0.wave.set_value(Wave::Saw),
            &[note_on_port(0, 60)],
        );
        let (_p, b) = render(&[note_on_port(1, 60)]);
        assert!(
            a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-6),
            "lane 0 set to Saw should match lane 1's default Saw"
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
