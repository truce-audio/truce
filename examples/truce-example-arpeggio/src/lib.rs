use truce::prelude::*;
use truce_gui::layout::{dropdown, knob, widgets, GridLayout};

// --- Arp pattern enum ---

#[derive(ParamEnum)]
pub enum ArpPattern {
    Up,
    Down,
    #[name = "Up/Down"]
    UpDown,
    Random,
}

/// Step length as a note value. Maps directly to beats per step.
/// `Quarter` is one beat at 4/4; everything else is a power-of-two
/// subdivision, down to 1/64 for fast arps.
#[derive(ParamEnum)]
pub enum ArpRate {
    #[name = "1/1"]
    Whole,
    #[name = "1/2"]
    Half,
    #[name = "1/4"]
    Quarter,
    #[name = "1/8"]
    Eighth,
    #[name = "1/16"]
    Sixteenth,
    #[name = "1/32"]
    ThirtySecond,
    #[name = "1/64"]
    SixtyFourth,
}

impl ArpRate {
    fn beats_per_step(self) -> f64 {
        match self {
            ArpRate::Whole => 4.0,
            ArpRate::Half => 2.0,
            ArpRate::Quarter => 1.0,
            ArpRate::Eighth => 0.5,
            ArpRate::Sixteenth => 0.25,
            ArpRate::ThirtySecond => 0.125,
            ArpRate::SixtyFourth => 0.0625,
        }
    }
}

// --- Parameters ---

use std::sync::Arc;
use ArpParamsParamId as P;

#[derive(Params)]
pub struct ArpParams {
    #[param(name = "Rate", default = 2)]
    pub rate: EnumParam<ArpRate>,

    #[param(
        name = "Octaves",
        short_name = "Oct",
        range = "discrete(1, 4)",
        default = 1
    )]
    pub octaves: FloatParam,

    #[param(name = "Pattern", short_name = "Pat")]
    pub pattern: EnumParam<ArpPattern>,

    #[param(name = "Gate", range = "linear(0.1, 1.0)", default = 0.8, unit = "%")]
    pub gate: FloatParam,
}

// --- Plugin ---

pub struct Arpeggio {
    pub params: Arc<ArpParams>,
    held_notes: Vec<u8>,
    sample_rate: f64,
    /// Global step index of the currently-sounding arp note.
    /// `None` after a gate-off or when no note is active.
    last_step: Option<i64>,
    /// MIDI note currently emitted to the host. We hold on to it so we
    /// can issue a matching note-off at gate time, on step boundary,
    /// or when the user releases all held notes.
    active_note: Option<u8>,
    /// Free-running beat counter used when the host does not report
    /// transport (e.g. a tester that runs the plugin without a play
    /// state). Advances at the last known tempo.
    free_beat: f64,
    /// Simple RNG state for random pattern
    rng: u32,
}

impl Arpeggio {
    pub fn new(params: Arc<ArpParams>) -> Self {
        Self {
            params,
            held_notes: Vec::new(),
            sample_rate: 44100.0,
            last_step: None,
            active_note: None,
            free_beat: 0.0,
            rng: 12345,
        }
    }

    fn build_sequence(&self) -> Vec<u8> {
        if self.held_notes.is_empty() {
            return Vec::new();
        }

        let mut base_notes = self.held_notes.clone();
        base_notes.sort();

        let octaves = self.params.octaves.value() as usize;
        let mut seq = Vec::new();
        for oct in 0..octaves {
            for &note in &base_notes {
                let n = note as u16 + (oct as u16 * 12);
                if n <= 127 {
                    seq.push(n as u8);
                }
            }
        }

        let pattern = self.params.pattern.value();
        match pattern {
            ArpPattern::Down => {
                seq.reverse();
            }
            ArpPattern::UpDown if seq.len() > 2 => {
                let mut down = seq[1..seq.len() - 1].to_vec();
                down.reverse();
                seq.extend(down);
            }
            _ => {} // Up and Random use the sequence as-is
        }

        seq
    }

    fn next_random(&mut self) -> u32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        self.rng
    }
}

impl PluginLogic for Arpeggio {
    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.sample_rate = sample_rate;
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
        self.held_notes.clear();
        self.last_step = None;
        self.active_note = None;
        self.free_beat = 0.0;
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Process input MIDI -- track held notes
        for event in events.iter() {
            match &event.body {
                EventBody::NoteOn { note, .. } if !self.held_notes.contains(note) => {
                    self.held_notes.push(*note);
                }
                EventBody::NoteOff { note, .. } => {
                    self.held_notes.retain(|n| n != note);
                    if self.held_notes.is_empty() {
                        // Release current arp note
                        if let Some(cn) = self.active_note.take() {
                            context.output_events.push(Event {
                                sample_offset: event.sample_offset,
                                body: EventBody::NoteOff {
                                    channel: 0,
                                    note: cn,
                                    velocity: 0.0,
                                },
                            });
                        }
                    }
                }
                _ => {}
            }
        }

        if self.held_notes.is_empty() {
            // Clear phase state so the next held chord re-triggers on
            // the next step boundary rather than carrying over the old
            // step index.
            self.last_step = None;
            return ProcessStatus::Normal;
        }

        let seq = self.build_sequence();
        if seq.is_empty() {
            return ProcessStatus::Normal;
        }

        let beats_per_step = self.params.rate.value().beats_per_step();
        let gate_frac = self.params.gate.value() as f64;

        // Phase-lock to the host beat grid whenever the host reports
        // transport with a real tempo. Otherwise fall back to a
        // free-running counter so standalone tests keep emitting notes
        // when no DAW is driving us.
        let transport = &context.transport;
        let host_locked = transport.playing && transport.tempo > 0.0;
        let tempo = if transport.tempo > 0.0 {
            transport.tempo
        } else {
            120.0
        };
        let beats_per_sample = tempo / 60.0 / self.sample_rate;
        let block_start_beat = if host_locked {
            transport.position_beats
        } else {
            self.free_beat
        };

        let block_size = buffer.num_samples();
        let pattern = self.params.pattern.value();
        let seq_len = seq.len() as i64;

        for i in 0..block_size {
            let beat = block_start_beat + (i as f64) * beats_per_sample;
            let step_num = (beat / beats_per_step).floor() as i64;

            if Some(step_num) != self.last_step {
                // Step boundary: release the previous note (if still
                // sounding past gate-off, this is a no-op) and trigger
                // the next step.
                if let Some(cn) = self.active_note.take() {
                    context.output_events.push(Event {
                        sample_offset: i as u32,
                        body: EventBody::NoteOff {
                            channel: 0,
                            note: cn,
                            velocity: 0.0,
                        },
                    });
                }
                let note = if pattern == ArpPattern::Random {
                    let idx = self.next_random() as usize % seq.len();
                    seq[idx]
                } else {
                    // `rem_euclid` gives a non-negative index even if
                    // `step_num` is negative (host seeking before 0).
                    seq[step_num.rem_euclid(seq_len) as usize]
                };
                context.output_events.push(Event {
                    sample_offset: i as u32,
                    body: EventBody::NoteOn {
                        channel: 0,
                        note,
                        velocity: 0.8,
                    },
                });
                self.active_note = Some(note);
                self.last_step = Some(step_num);
            } else if let Some(step) = self.last_step {
                // Same step — check whether we've crossed the gate-off
                // boundary within it.
                let gate_off_beat = (step as f64 + gate_frac) * beats_per_step;
                if beat >= gate_off_beat {
                    if let Some(cn) = self.active_note.take() {
                        context.output_events.push(Event {
                            sample_offset: i as u32,
                            body: EventBody::NoteOff {
                                channel: 0,
                                note: cn,
                                velocity: 0.0,
                            },
                        });
                    }
                }
            }
        }

        // Keep the free-running counter aligned so dropping out of host
        // mode mid-session doesn't cause a phase jump.
        let end_beat = block_start_beat + (block_size as f64) * beats_per_sample;
        self.free_beat = end_beat;

        ProcessStatus::Normal
    }

    fn layout(&self) -> GridLayout {
        GridLayout::build(
            "ARPEGGIO",
            "V0.1",
            2,
            50.0,
            vec![widgets(vec![
                dropdown(P::Rate, "Rate"),
                knob(P::Gate, "Gate"),
                knob(P::Octaves, "Octaves"),
                dropdown(P::Pattern, "Pattern"),
            ])],
        )
    }
}

truce::plugin! {
    logic: Arpeggio,
    params: ArpParams,
    // MIDI effect: no audio I/O. CLAP/VST3/AU(aumi)/LV2 honor this;
    // AAX (which has no audio-less plugin category) auto-adds a
    // stereo passthrough inside `truce-aax` so the DAW's track
    // audio flows through unchanged.
    bus_layouts: [BusLayout::new()],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_screenshot() {
        let params = Arc::new(ArpParams::new());
        let arp = Arpeggio::new(Arc::clone(&params));
        let layout = arp.layout();
        truce_test::assert_gui_screenshot_grid::<ArpParams>(
            "arpeggio_default",
            params,
            layout,
            0,
            "examples/screenshots",
        );
    }

    /// Regression guard for the 2026-04-23 LV2 MIDI bug: the
    /// `truce_derive::plugin_info` proc macro used to fall `"midi"` →
    /// `Effect` (only `"instrument"` had a match arm), which turned off
    /// the MIDI decode path in `truce-lv2::run`. Arpeggio must see
    /// `NoteEffect` here or the plugin silently ignores all host MIDI.
    #[test]
    fn category_is_note_effect() {
        use truce_core::info::PluginCategory;
        use truce_core::plugin::Plugin as PluginTrait;
        assert_eq!(
            <Plugin as PluginTrait>::info().category,
            PluginCategory::NoteEffect
        );
    }
}
