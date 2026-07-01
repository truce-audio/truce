//! Promotes MIDI 1.0 to MIDI 2.0 (UMP) and spreads notes across the
//! channel address space - a test vehicle for truce's 2.0 **encode**
//! surface.
//!
//! Every incoming 1.0 channel-voice message is re-emitted as its native
//! 2.0 `EventBody` (7-bit velocity -> 16-bit, 7/14-bit controller/bend
//! values -> 32-bit). An `Algo` mode chooses how notes are addressed:
//!
//! - **Passthrough**   - promotion only, addressing untouched.
//! - **Channel Fan**   - round-robin each note across `Channels` MIDI
//!   channels (an MPE-style voice allocator).
//! - **Auto Vibrato**  - a per-held-note pitch-bend LFO emitted as
//!   `PerNotePitchBend` (32-bit) on each note's own channel.
//! - **Mod Brightness** - the mod wheel (CC 1) routed to per-note
//!   brightness (`PerNoteCC` 74) on every held note, on its own channel.
//!
//! `midi2_output = true` in `truce.toml` makes the output carry native
//! UMP on CLAP / AU v3; on formats without a UMP transport the wrapper
//! (or host) down-converts to 1.0. Non-1.0-channel-voice input is ignored.

use std::f64::consts::TAU;
use std::sync::Arc;

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, dropdown, knob, widgets};

use SpreaderParamsParamId as P;

/// MIDI mod-wheel CC number.
const CC_MOD_WHEEL: u8 = 1;
/// MIDI 2.0 per-note brightness controller number.
const CC_BRIGHTNESS: u8 = 74;
/// Centre of a 32-bit per-note pitch bend (no detune).
const PITCH_CENTER: u32 = 0x8000_0000;

/// Note addressing. `ParamEnum` derives `Clone` / `Copy` / `PartialEq`.
#[derive(ParamEnum)]
pub enum Algo {
    #[name = "Passthrough"]
    Passthrough,
    #[name = "Channel Fan"]
    ChannelFan,
    #[name = "Auto Vibrato"]
    AutoVibrato,
    #[name = "Mod Brightness"]
    ModBrightness,
}

#[derive(Params)]
pub struct SpreaderParams {
    // Default index 1 = Channel Fan, the plugin's namesake behavior.
    #[param(name = "Algorithm", short_name = "Algo", default = 1)]
    pub algo: EnumParam<Algo>,
    #[param(
        name = "Channels",
        short_name = "Chans",
        range = "discrete(1, 16)",
        default = 16
    )]
    pub channels: IntParam,
    #[param(
        name = "Vibrato Rate",
        range = "log(0.1, 12)",
        default = 5.0,
        unit = "Hz"
    )]
    pub vib_rate: FloatParam,
    #[param(
        name = "Vibrato Depth",
        range = "linear(0, 1)",
        default = 0.25,
        unit = "%"
    )]
    pub vib_depth: FloatParam,
}

/// A held input note, so `NoteOff` and per-note expression reach the same
/// voice even if the mode or fan width changes mid-hold.
#[derive(Clone, Copy)]
struct Held {
    group: u8,
    channel: u8,
    /// Vibrato LFO phase in turns (0..1), advanced per block.
    phase: f64,
}

pub struct Spreader {
    params: Arc<SpreaderParams>,
    sample_rate: f64,
    /// Held notes indexed by note number. One entry per pitch - a second
    /// `NoteOn` of the same pitch overwrites (a test tool, not a full
    /// voice allocator).
    held: [Option<Held>; 128],
    /// Next channel to hand out for `Channel Fan`.
    next_channel: u8,
    /// Whether `Auto Vibrato` emitted bends last block, so a note-off or
    /// a mode change can recentre the note instead of leaving it detuned.
    vibrato_active: bool,
}

impl Spreader {
    #[must_use]
    pub fn new(params: Arc<SpreaderParams>) -> Self {
        Self {
            params,
            sample_rate: 44100.0,
            held: [None; 128],
            next_channel: 0,
            vibrato_active: false,
        }
    }
}

// 7-bit -> 16-bit, exact endpoints (0->0, 127->65535). The masked
// division fits the narrower type; the lint can't prove it.
#[allow(clippy::cast_possible_truncation)]
fn vel7_to_16(v: u8) -> u16 {
    ((u32::from(v) * 0xFFFF) / 127) as u16
}

// 7-bit -> 32-bit controller value.
#[allow(clippy::cast_possible_truncation)]
fn cc7_to_32(v: u8) -> u32 {
    ((u64::from(v) * u64::from(u32::MAX)) / 127) as u32
}

// 14-bit -> 32-bit pitch bend; 8192 (centre) -> ~0x8000_0000.
#[allow(clippy::cast_possible_truncation)]
fn bend14_to_32(v: u16) -> u32 {
    ((u64::from(v) * u64::from(u32::MAX)) / 16383) as u32
}

// LFO sample -> 32-bit per-note pitch bend around centre.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn lfo_bend(depth: f64, s: f64) -> u32 {
    let half = f64::from(u32::MAX) / 2.0;
    (half + depth * s * half)
        .round()
        .clamp(0.0, f64::from(u32::MAX)) as u32
}

impl PluginLogic for Spreader {
    fn bus_layouts() -> Vec<BusLayout> {
        // MIDI effect: no audio I/O.
        vec![BusLayout::new()]
    }

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
        self.sample_rate = sample_rate;
        self.held = [None; 128];
        self.next_channel = 0;
        self.vibrato_active = false;
    }

    // One arm per promoted event type - long but flat.
    #[allow(clippy::too_many_lines)]
    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        let algo = self.params.algo.value();
        let width = self.params.channels.value_u8().clamp(1, 16);

        for event in events.iter() {
            let off = event.sample_offset;
            match event.body {
                EventBody::NoteOn {
                    group,
                    channel,
                    note,
                    velocity,
                } => {
                    let (g, ch) = self.route(algo, width, group, channel);
                    self.held[usize::from(note)] = Some(Held {
                        group: g,
                        channel: ch,
                        phase: 0.0,
                    });
                    context.output_events.push(Event::new(
                        off,
                        EventBody::NoteOn2 {
                            group: g,
                            channel: ch,
                            note,
                            velocity: vel7_to_16(velocity),
                            attribute_type: 0,
                            attribute: 0,
                        },
                    ));
                }
                EventBody::NoteOff {
                    group,
                    channel,
                    note,
                    velocity,
                } => {
                    let held = self.held[usize::from(note)].take();
                    let (g, ch) = held.map_or((group, channel), |h| (h.group, h.channel));
                    // Recentre a note that was being vibrato'd before it
                    // ends, so its release tail isn't stuck at the last
                    // bend. (Playback stop sends note-offs, so this covers
                    // "recentre on stop" too.)
                    if self.vibrato_active && held.is_some() {
                        context.output_events.push(Event::new(
                            off,
                            EventBody::PerNotePitchBend {
                                group: g,
                                channel: ch,
                                note,
                                value: PITCH_CENTER,
                            },
                        ));
                    }
                    context.output_events.push(Event::new(
                        off,
                        EventBody::NoteOff2 {
                            group: g,
                            channel: ch,
                            note,
                            velocity: vel7_to_16(velocity),
                            attribute_type: 0,
                            attribute: 0,
                        },
                    ));
                }
                EventBody::ControlChange { cc, value, .. }
                    if algo == Algo::ModBrightness && cc == CC_MOD_WHEEL =>
                {
                    // Route the mod wheel to per-note brightness on every
                    // held note. Per-note expression must ride the same
                    // channel as the note so a downstream voice keyed by
                    // (channel, note) actually receives it.
                    for note in 0u8..128 {
                        if let Some(h) = self.held[usize::from(note)] {
                            context.output_events.push(Event::new(
                                off,
                                EventBody::PerNoteCC {
                                    group: h.group,
                                    channel: h.channel,
                                    note,
                                    cc: CC_BRIGHTNESS,
                                    value: cc7_to_32(value),
                                    registered: true,
                                },
                            ));
                        }
                    }
                }
                EventBody::ControlChange {
                    group,
                    channel,
                    cc,
                    value,
                } => {
                    context.output_events.push(Event::new(
                        off,
                        EventBody::ControlChange2 {
                            group,
                            channel,
                            cc,
                            value: cc7_to_32(value),
                        },
                    ));
                }
                EventBody::PitchBend {
                    group,
                    channel,
                    value,
                } => {
                    context.output_events.push(Event::new(
                        off,
                        EventBody::PitchBend2 {
                            group,
                            channel,
                            value: bend14_to_32(value),
                        },
                    ));
                }
                EventBody::Aftertouch {
                    group,
                    channel,
                    note,
                    pressure,
                } => {
                    context.output_events.push(Event::new(
                        off,
                        EventBody::PolyPressure2 {
                            group,
                            channel,
                            note,
                            pressure: cc7_to_32(pressure),
                        },
                    ));
                }
                EventBody::ChannelPressure {
                    group,
                    channel,
                    pressure,
                } => {
                    context.output_events.push(Event::new(
                        off,
                        EventBody::ChannelPressure2 {
                            group,
                            channel,
                            pressure: cc7_to_32(pressure),
                        },
                    ));
                }
                EventBody::ProgramChange {
                    group,
                    channel,
                    program,
                } => {
                    context.output_events.push(Event::new(
                        off,
                        EventBody::ProgramChange2 {
                            group,
                            channel,
                            program,
                            bank: None,
                        },
                    ));
                }
                // Already-2.0, SysEx, transport, param automation: ignored
                // (this is a 1.0 -> 2.0 promoter, fed 1.0).
                _ => {}
            }
        }

        // Vibrato runs whenever notes are held - live on a stopped
        // transport or during playback. Leaving the mode recentres held
        // notes; a released note is recentred at its note-off (in the arm
        // above) so its release tail isn't left detuned - which also
        // covers a transport stop, since the host sends note-offs then.
        if algo == Algo::AutoVibrato {
            self.emit_vibrato(buffer.num_samples(), context);
            self.vibrato_active = true;
        } else if self.vibrato_active {
            self.recentre_held(context);
            self.vibrato_active = false;
        }

        ProcessStatus::Normal
    }

    fn editor(&self) -> Box<dyn Editor> {
        GridLayout::build(vec![
            widgets(vec![dropdown(P::Algo, "Algorithm").cols(3)]),
            widgets(vec![
                knob(P::Channels, "Channels"),
                knob(P::VibRate, "Rate"),
                knob(P::VibDepth, "Depth"),
            ]),
        ])
        .with_title("MPE SPREAD")
        .into_editor(&self.params)
    }
}

impl Spreader {
    /// Resolve the output `(group, channel)` for a fresh note and advance
    /// any round-robin counters. Only called on `NoteOn`.
    fn route(&mut self, algo: Algo, width: u8, group: u8, channel: u8) -> (u8, u8) {
        match algo {
            Algo::ChannelFan => {
                let ch = self.next_channel % width;
                self.next_channel = (self.next_channel + 1) % width;
                (group, ch)
            }
            Algo::Passthrough | Algo::AutoVibrato | Algo::ModBrightness => (group, channel),
        }
    }

    /// Emit a centre `PerNotePitchBend` for every held note, cancelling
    /// residual vibrato detune when the mode leaves Auto Vibrato.
    fn recentre_held(&mut self, context: &mut ProcessContext) {
        for note in 0u8..128 {
            if let Some(h) = self.held[usize::from(note)].as_mut() {
                h.phase = 0.0;
                context.output_events.push(Event::new(
                    0,
                    EventBody::PerNotePitchBend {
                        group: h.group,
                        channel: h.channel,
                        note,
                        value: PITCH_CENTER,
                    },
                ));
            }
        }
    }

    /// Advance every held note's vibrato LFO by one block and emit a
    /// `PerNotePitchBend` for it on the note's own channel.
    fn emit_vibrato(&mut self, block_samples: usize, context: &mut ProcessContext) {
        let rate = self.params.vib_rate.raw_target();
        let depth = self.params.vib_depth.raw_target();
        let n = u32::try_from(block_samples).unwrap_or(0);
        let inc = rate * f64::from(n) / self.sample_rate;
        for note in 0u8..128 {
            if let Some(h) = self.held[usize::from(note)].as_mut() {
                h.phase = (h.phase + inc).fract();
                let value = lfo_bend(depth, (h.phase * TAU).sin());
                context.output_events.push(Event::new(
                    0,
                    EventBody::PerNotePitchBend {
                        group: h.group,
                        channel: h.channel,
                        note,
                        value,
                    },
                ));
            }
        }
    }
}

truce::plugin! {
    logic: Spreader,
    params: SpreaderParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(algo: Algo, width: i64, input: &[EventBody]) -> EventList {
        let params = Arc::new(SpreaderParams::new());
        params.algo.set_value(algo);
        params.channels.set_value(width);
        let mut plugin = Spreader::new(Arc::clone(&params));
        plugin.reset(44100.0, 64);

        let in_refs: Vec<&[f32]> = Vec::new();
        let mut out_refs: Vec<&mut [f32]> = Vec::new();
        let mut buffer = unsafe { AudioBuffer::from_slices(&in_refs, &mut out_refs, 64) };

        let mut events = EventList::default();
        for body in input {
            events.push(Event::new(0, *body));
        }
        let transport = TransportInfo::default();
        let mut output = EventList::default();
        let mut ctx = ProcessContext::new(&transport, 44100.0, 64, &mut output);
        plugin.process(&mut buffer, &events, &mut ctx);
        output
    }

    fn note_on(channel: u8, note: u8) -> EventBody {
        EventBody::NoteOn {
            group: 0,
            channel,
            note,
            velocity: 100,
        }
    }

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn promotes_note_velocity_to_16_bit() {
        let out = run(
            Algo::Passthrough,
            16,
            &[EventBody::NoteOn {
                group: 0,
                channel: 0,
                note: 60,
                velocity: 127,
            }],
        );
        let ev = out.iter().next().expect("one event");
        match ev.body {
            EventBody::NoteOn2 { velocity, note, .. } => {
                assert_eq!(note, 60);
                assert_eq!(velocity, 0xFFFF); // full 7-bit -> full 16-bit
            }
            other => panic!("expected NoteOn2, got {other:?}"),
        }
    }

    #[test]
    fn channel_fan_spreads_notes_across_channels() {
        let out = run(
            Algo::ChannelFan,
            4,
            &[
                note_on(0, 60),
                note_on(0, 61),
                note_on(0, 62),
                note_on(0, 63),
            ],
        );
        let channels: Vec<u8> = out
            .iter()
            .filter_map(|e| match e.body {
                EventBody::NoteOn2 { channel, .. } => Some(channel),
                _ => None,
            })
            .collect();
        // Four notes over a width of 4 -> channels 0,1,2,3.
        assert_eq!(channels, vec![0, 1, 2, 3]);
    }

    #[test]
    fn mod_brightness_routes_mod_wheel_to_per_note() {
        let out = run(
            Algo::ModBrightness,
            16,
            &[
                note_on(0, 60),
                EventBody::ControlChange {
                    group: 0,
                    channel: 0,
                    cc: CC_MOD_WHEEL,
                    value: 127,
                },
            ],
        );
        let per_note = out
            .iter()
            .find(|e| matches!(e.body, EventBody::PerNoteCC { .. }))
            .expect("a per-note CC");
        match per_note.body {
            EventBody::PerNoteCC {
                note, cc, value, ..
            } => {
                assert_eq!(note, 60);
                assert_eq!(cc, CC_BRIGHTNESS);
                assert_eq!(value, u32::MAX);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn auto_vibrato_emits_per_note_pitch_bend() {
        let out = run(Algo::AutoVibrato, 16, &[note_on(0, 60)]);
        assert!(
            out.iter()
                .any(|e| matches!(e.body, EventBody::PerNotePitchBend { .. })),
            "auto vibrato emits a per-note pitch bend"
        );
    }

    // Hold note 60 under Auto Vibrato (bends it), then run one more block
    // with the given mode + events; returns the recentre bend for note 60
    // if one was emitted.
    fn recentre_after(second_algo: Algo, second_events: &[EventBody]) -> Option<u32> {
        let params = Arc::new(SpreaderParams::new());
        params.algo.set_value(Algo::AutoVibrato);
        let mut plugin = Spreader::new(Arc::clone(&params));
        plugin.reset(44100.0, 64);

        let in_refs: Vec<&[f32]> = Vec::new();
        let mut out_refs: Vec<&mut [f32]> = Vec::new();
        let mut buffer = unsafe { AudioBuffer::from_slices(&in_refs, &mut out_refs, 64) };
        let transport = TransportInfo::default();

        let mut on = EventList::default();
        on.push(Event::new(0, note_on(0, 60)));
        let mut out1 = EventList::default();
        let mut ctx1 = ProcessContext::new(&transport, 44100.0, 64, &mut out1);
        plugin.process(&mut buffer, &on, &mut ctx1);

        params.algo.set_value(second_algo);
        let mut ev2 = EventList::default();
        for b in second_events {
            ev2.push(Event::new(0, *b));
        }
        let mut out2 = EventList::default();
        let mut ctx2 = ProcessContext::new(&transport, 44100.0, 64, &mut out2);
        plugin.process(&mut buffer, &ev2, &mut ctx2);

        out2.iter().find_map(|e| match e.body {
            EventBody::PerNotePitchBend {
                note: 60, value, ..
            } => Some(value),
            _ => None,
        })
    }

    #[test]
    fn leaving_vibrato_recentres_held_notes() {
        // Switching mode away from Auto Vibrato recentres held notes.
        assert_eq!(recentre_after(Algo::Passthrough, &[]), Some(PITCH_CENTER));
    }

    #[test]
    fn note_off_under_vibrato_recentres_the_note() {
        // A note-off while vibrato is active recentres the note - which is
        // also how a transport stop lands (the host sends note-offs).
        let off = EventBody::NoteOff {
            group: 0,
            channel: 0,
            note: 60,
            velocity: 0,
        };
        assert_eq!(
            recentre_after(Algo::AutoVibrato, &[off]),
            Some(PITCH_CENTER)
        );
    }

    #[test]
    fn pitch_bend_upsamples_to_32_bit() {
        let out = run(
            Algo::Passthrough,
            16,
            &[EventBody::PitchBend {
                group: 0,
                channel: 0,
                value: 8192, // centre
            }],
        );
        match out.iter().next().unwrap().body {
            EventBody::PitchBend2 { value, .. } => {
                // 8192 / 16383 * u32::MAX ~ half scale.
                let half = u32::MAX / 2;
                assert!(value.abs_diff(half) < u32::MAX / 1000);
            }
            other => panic!("expected PitchBend2, got {other:?}"),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/mpe_spreader_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/mpe_spreader_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/mpe_spreader_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
