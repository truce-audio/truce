//! Spreads notes across the channel address space and adds per-note
//! expression - a test vehicle for truce's 2.0 **encode** surface.
//!
//! Pure MIDI 2.0 (`midi2 = true`): the output is always native 2.0 UMP,
//! and note input is accepted in either dialect - a 1.0 `NoteOn`
//! promotes (7-bit velocity -> 16-bit), a native `NoteOn2` re-addresses
//! at full resolution. An `Algo` mode chooses how notes are addressed:
//!
//! - **Passthrough**   - promotion only, addressing untouched.
//! - **Channel Fan**   - round-robin each note across `Channels` MIDI
//!   channels (an MPE-style voice allocator).
//! - **Per-Note Vibrato** - a per-held-note pitch-bend LFO emitted as
//!   `PerNotePitchBend` (32-bit) on each note's own channel. Needs a
//!   2.0-capable path downstream (CLAP note expression, AU v3 UMP) -
//!   per-note bend has no MIDI 1.0 encoding and vanishes when a host
//!   down-converts.
//! - **MPE Vibrato**   - the same LFO as channel-level `PitchBend2`,
//!   with notes channel-fanned so each bend still lands on one note.
//!   Down-converts to a plain 14-bit bend, so it's audible on every
//!   format and host (assumes the downstream +-2 st bend range).
//! - **Mod Brightness** - the mod wheel (CC 1) routed to per-note
//!   brightness (`PerNoteCC` 74) on every held note, on its own channel.
//!
//! `Vibrato Depth` is in semitones; both modes scale it into their
//! wire's bend range.
//!
//! `midi2 = true` in `truce.toml` makes the output carry native UMP on
//! CLAP / AU v3; on formats without a UMP transport the wrapper (or host)
//! down-converts to 1.0. Non-note channel-voice input beyond the arms
//! below (e.g. a native 2.0 CC) is ignored.

use std::f64::consts::TAU;
use std::sync::Arc;

use truce::core::midi::{upscale_7_to_16, upscale_7_to_32, upscale_14_to_32};
use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, dropdown, knob, widgets};

use SpreaderParamsParamId as P;

/// MIDI mod-wheel CC number.
const CC_MOD_WHEEL: u8 = 1;
/// MIDI 2.0 per-note brightness controller number.
const CC_BRIGHTNESS: u8 = 74;
/// Centre of a 32-bit pitch bend (no detune).
const PITCH_CENTER: u32 = 0x8000_0000;
/// Downstream per-note bend range (MPE convention), semitones each way.
const PER_NOTE_BEND_RANGE_SEMIS: f64 = 48.0;
/// Downstream channel bend range (GM default), semitones each way.
const CHANNEL_BEND_RANGE_SEMIS: f64 = 2.0;

/// Note addressing. `ParamEnum` derives `Clone` / `Copy` / `PartialEq`.
#[derive(ParamEnum)]
pub enum Algo {
    #[name = "Passthrough"]
    Passthrough,
    #[name = "Channel Fan"]
    ChannelFan,
    #[name = "Per-Note Vibrato (2.0 only)"]
    PerNoteVibrato,
    #[name = "MPE Vibrato"]
    MpeVibrato,
    #[name = "Mod Brightness"]
    ModBrightness,
}

/// The wire a vibrato mode bends on, kept while bends are live so a
/// note-off or mode switch can recentre on the same wire.
#[derive(Clone, Copy, PartialEq)]
enum VibratoKind {
    /// 32-bit `PerNotePitchBend` (needs a 2.0-capable path downstream).
    PerNote,
    /// Channel-level `PitchBend2` (down-converts to 14-bit everywhere).
    Channel,
}

fn vibrato_kind(algo: Algo) -> Option<VibratoKind> {
    match algo {
        Algo::PerNoteVibrato => Some(VibratoKind::PerNote),
        Algo::MpeVibrato => Some(VibratoKind::Channel),
        Algo::Passthrough | Algo::ChannelFan | Algo::ModBrightness => None,
    }
}

/// A centre-bend event for `(group, channel, note)` on `kind`'s wire.
fn recentre_body(kind: VibratoKind, group: u8, channel: u8, note: u8) -> EventBody {
    match kind {
        VibratoKind::PerNote => EventBody::PerNotePitchBend {
            group,
            channel,
            note,
            value: PITCH_CENTER,
        },
        VibratoKind::Channel => EventBody::PitchBend2 {
            group,
            channel,
            value: PITCH_CENTER,
        },
    }
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
        range = "linear(0, 2)",
        default = 0.5,
        unit = "st"
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
    /// Next channel to hand out for `Channel Fan` / `MPE Vibrato`.
    next_channel: u8,
    /// The wire vibrato bent on last block, so a note-off or a mode
    /// change can recentre the note instead of leaving it detuned.
    vibrato: Option<VibratoKind>,
}

impl Spreader {
    #[must_use]
    pub fn new(params: Arc<SpreaderParams>) -> Self {
        Self {
            params,
            sample_rate: 44100.0,
            held: [None; 128],
            next_channel: 0,
            vibrato: None,
        }
    }
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
    type Params = SpreaderParams;

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
        self.vibrato = None;
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
                            velocity: upscale_7_to_16(velocity),
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
                    if let Some(kind) = self.vibrato
                        && held.is_some()
                    {
                        context
                            .output_events
                            .push(Event::new(off, recentre_body(kind, g, ch, note)));
                    }
                    context.output_events.push(Event::new(
                        off,
                        EventBody::NoteOff2 {
                            group: g,
                            channel: ch,
                            note,
                            velocity: upscale_7_to_16(velocity),
                            attribute_type: 0,
                            attribute: 0,
                        },
                    ));
                }
                // Native 2.0 notes (the host negotiated a 2.0 input
                // connection): re-address and re-emit at full resolution,
                // no promotion. Same routing / held-tracking as the 1.0
                // arms above.
                EventBody::NoteOn2 {
                    group,
                    channel,
                    note,
                    velocity,
                    attribute_type,
                    attribute,
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
                            velocity,
                            attribute_type,
                            attribute,
                        },
                    ));
                }
                EventBody::NoteOff2 {
                    group,
                    channel,
                    note,
                    velocity,
                    attribute_type,
                    attribute,
                } => {
                    let held = self.held[usize::from(note)].take();
                    let (g, ch) = held.map_or((group, channel), |h| (h.group, h.channel));
                    if let Some(kind) = self.vibrato
                        && held.is_some()
                    {
                        context
                            .output_events
                            .push(Event::new(off, recentre_body(kind, g, ch, note)));
                    }
                    context.output_events.push(Event::new(
                        off,
                        EventBody::NoteOff2 {
                            group: g,
                            channel: ch,
                            note,
                            velocity,
                            attribute_type,
                            attribute,
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
                                    value: upscale_7_to_32(value),
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
                            value: upscale_7_to_32(value),
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
                            value: upscale_14_to_32(value),
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
                            pressure: upscale_7_to_32(pressure),
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
                            pressure: upscale_7_to_32(pressure),
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
        // transport or during playback. Emitted at the block's last
        // sample: hosts require output events sorted by offset, and the
        // loop above pushes at real offsets, so these must not land
        // earlier in the queue with a smaller offset. Switching modes
        // recentres held notes on the old wire; a released note is
        // recentred at its note-off (in the loop above) - which also
        // covers a transport stop, since the host sends note-offs then.
        let last = u32::try_from(buffer.num_samples().saturating_sub(1)).unwrap_or(0);
        let kind = vibrato_kind(algo);
        if self.vibrato != kind {
            if let Some(old) = self.vibrato {
                self.recentre_held(old, last, context);
            }
            self.vibrato = kind;
        }
        if let Some(kind) = kind {
            self.emit_vibrato(kind, buffer.num_samples(), last, context);
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<SpreaderParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![
            widgets(vec![dropdown(P::Algo, "Algorithm").cols(3)]),
            widgets(vec![
                knob(P::Channels, "Channels"),
                knob(P::VibRate, "Rate"),
                knob(P::VibDepth, "Depth"),
            ]),
        ])
        .with_title("MPE SPREAD")
        .into_editor(&params)
    }
}

impl Spreader {
    /// Resolve the output `(group, channel)` for a fresh note and advance
    /// any round-robin counters. Only called on `NoteOn`. `MPE Vibrato`
    /// fans like `Channel Fan` so each channel bend lands on one note.
    fn route(&mut self, algo: Algo, width: u8, group: u8, channel: u8) -> (u8, u8) {
        match algo {
            Algo::ChannelFan | Algo::MpeVibrato => {
                let ch = self.next_channel % width;
                self.next_channel = (self.next_channel + 1) % width;
                (group, ch)
            }
            Algo::Passthrough | Algo::PerNoteVibrato | Algo::ModBrightness => (group, channel),
        }
    }

    /// Emit a centre bend on `kind`'s wire for every held note,
    /// cancelling residual vibrato detune when the mode switches.
    /// `offset` is the block's last sample (see the ordering note at
    /// the call site).
    fn recentre_held(&mut self, kind: VibratoKind, offset: u32, context: &mut ProcessContext) {
        for note in 0u8..128 {
            if let Some(h) = self.held[usize::from(note)].as_mut() {
                h.phase = 0.0;
                context.output_events.push(Event::new(
                    offset,
                    recentre_body(kind, h.group, h.channel, note),
                ));
            }
        }
    }

    /// Advance every held note's vibrato LFO by one block and emit its
    /// bend on `kind`'s wire at `offset` (the block's last sample). The
    /// semitone depth param scales into the wire's bend range.
    fn emit_vibrato(
        &mut self,
        kind: VibratoKind,
        block_samples: usize,
        offset: u32,
        context: &mut ProcessContext,
    ) {
        let rate = self.params.vib_rate.raw_target();
        let range = match kind {
            VibratoKind::PerNote => PER_NOTE_BEND_RANGE_SEMIS,
            VibratoKind::Channel => CHANNEL_BEND_RANGE_SEMIS,
        };
        let depth = (self.params.vib_depth.raw_target() / range).min(1.0);
        let n = u32::try_from(block_samples).unwrap_or(0);
        let inc = rate * f64::from(n) / self.sample_rate;
        for note in 0u8..128 {
            if let Some(h) = self.held[usize::from(note)].as_mut() {
                h.phase = (h.phase + inc).fract();
                let value = lfo_bend(depth, (h.phase * TAU).sin());
                let body = match kind {
                    VibratoKind::PerNote => EventBody::PerNotePitchBend {
                        group: h.group,
                        channel: h.channel,
                        note,
                        value,
                    },
                    VibratoKind::Channel => EventBody::PitchBend2 {
                        group: h.group,
                        channel: h.channel,
                        value,
                    },
                };
                context.output_events.push(Event::new(offset, body));
            }
        }
    }
}

truce::plugin! {
    logic: Spreader,
    params: SpreaderParams,
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
                    s.set_param(P::Algo, 0.9);
                    s.wait_ms(15);
                    s.set_param(P::Algo, 0.1);
                    s.wait_ms(15);
                    s.note_off(60);
                })
                .run()
        });
    }

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
    fn per_note_vibrato_emits_per_note_pitch_bend() {
        let out = run(Algo::PerNoteVibrato, 16, &[note_on(0, 60)]);
        assert!(
            out.iter()
                .any(|e| matches!(e.body, EventBody::PerNotePitchBend { .. })),
            "per-note vibrato emits a per-note pitch bend"
        );
    }

    // Hold note 60 under `first_algo` (bends it), then run one more
    // block with `second_algo` + events; returns the second block's
    // output for recentre assertions.
    fn second_block(first_algo: Algo, second_algo: Algo, second_events: &[EventBody]) -> EventList {
        let params = Arc::new(SpreaderParams::new());
        params.algo.set_value(first_algo);
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
        out2
    }

    fn per_note_bend_for_60(out: &EventList) -> Option<u32> {
        out.iter().find_map(|e| match e.body {
            EventBody::PerNotePitchBend {
                note: 60, value, ..
            } => Some(value),
            _ => None,
        })
    }

    fn channel_bend(out: &EventList) -> Option<u32> {
        out.iter().find_map(|e| match e.body {
            EventBody::PitchBend2 { value, .. } => Some(value),
            _ => None,
        })
    }

    fn note_off_60() -> EventBody {
        EventBody::NoteOff {
            group: 0,
            channel: 0,
            note: 60,
            velocity: 0,
        }
    }

    #[test]
    fn leaving_vibrato_recentres_held_notes() {
        // Switching mode away from Per-Note Vibrato recentres held notes.
        let out = second_block(Algo::PerNoteVibrato, Algo::Passthrough, &[]);
        assert_eq!(per_note_bend_for_60(&out), Some(PITCH_CENTER));
    }

    #[test]
    fn note_off_under_vibrato_recentres_the_note() {
        // A note-off while vibrato is active recentres the note - which is
        // also how a transport stop lands (the host sends note-offs).
        let out = second_block(Algo::PerNoteVibrato, Algo::PerNoteVibrato, &[note_off_60()]);
        assert_eq!(per_note_bend_for_60(&out), Some(PITCH_CENTER));
    }

    #[test]
    fn mpe_vibrato_fans_channels_and_bends_the_channel() {
        let out = run(Algo::MpeVibrato, 4, &[note_on(0, 60), note_on(0, 61)]);
        let channels: Vec<u8> = out
            .iter()
            .filter_map(|e| match e.body {
                EventBody::NoteOn2 { channel, .. } => Some(channel),
                _ => None,
            })
            .collect();
        assert_eq!(channels, vec![0, 1], "notes fan like Channel Fan");
        let bend_channels: Vec<u8> = out
            .iter()
            .filter_map(|e| match e.body {
                EventBody::PitchBend2 { channel, .. } => Some(channel),
                _ => None,
            })
            .collect();
        assert_eq!(bend_channels, vec![0, 1], "one channel bend per note");
        assert!(
            !out.iter()
                .any(|e| matches!(e.body, EventBody::PerNotePitchBend { .. })),
            "MPE Vibrato bends the channel, not the note"
        );
    }

    #[test]
    fn note_off_under_mpe_vibrato_recentres_the_channel() {
        let out = second_block(Algo::MpeVibrato, Algo::MpeVibrato, &[note_off_60()]);
        assert_eq!(channel_bend(&out), Some(PITCH_CENTER));
    }

    #[test]
    fn switching_vibrato_wires_recentres_the_old_wire() {
        // Per-Note Vibrato -> MPE Vibrato must recentre the
        // per-note wire before bending the channel wire.
        let out = second_block(Algo::PerNoteVibrato, Algo::MpeVibrato, &[]);
        assert_eq!(per_note_bend_for_60(&out), Some(PITCH_CENTER));
        assert!(channel_bend(&out).is_some(), "new wire starts bending");
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
