use truce::prelude::*;
use truce_gui::layout::{GridLayout, dropdown, knob, widgets};

// --- Arp pattern enum ---

#[derive(ParamEnum)]
pub enum ArpPattern {
    Up,
    Down,
    #[name = "Up/Down"]
    UpDown,
    Random,
}

// --- Parameters ---

use ArpParamsParamId as P;

#[derive(Params)]
pub struct ArpParams {
    #[param(name = "Rate", range = "discrete(1, 8)", default = 4)]
    pub rate: FloatParam,

    #[param(name = "Octaves", short_name = "Oct",
            range = "discrete(1, 4)", default = 1)]
    pub octaves: FloatParam,

    #[param(name = "Pattern", short_name = "Pat")]
    pub pattern: EnumParam<ArpPattern>,

    #[param(name = "Gate", range = "linear(0.1, 1.0)",
            default = 0.8, unit = "%")]
    pub gate: FloatParam,
}

// --- Plugin ---

pub struct Arpeggio {
    pub params: std::sync::Arc<ArpParams>,
    held_notes: Vec<u8>,
    sample_rate: f64,
    /// Samples since the last arp trigger
    sample_counter: u64,
    /// Current step index in the arp sequence
    step_index: usize,
    /// Whether the current arp note is still sounding
    current_note: Option<u8>,
    /// Direction for up/down pattern
    going_up: bool,
    /// Simple RNG state for random pattern
    rng: u32,
}

impl Arpeggio {
    pub fn new(params: std::sync::Arc<ArpParams>) -> Self {
        Self {
            params,
            held_notes: Vec::new(),
            sample_rate: 44100.0,
            sample_counter: 0,
            step_index: 0,
            current_note: None,
            going_up: true,
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
            ArpPattern::Down => { seq.reverse(); }
            ArpPattern::UpDown => {
                if seq.len() > 2 {
                    let mut down = seq[1..seq.len() - 1].to_vec();
                    down.reverse();
                    seq.extend(down);
                }
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
        self.sample_counter = 0;
        self.step_index = 0;
        self.current_note = None;
        self.going_up = true;
    }

    fn process(&mut self, buffer: &mut AudioBuffer, events: &EventList, context: &mut ProcessContext) -> ProcessStatus {
        // Process input MIDI -- track held notes
        for event in events.iter() {
            match &event.body {
                EventBody::NoteOn { note, .. } => {
                    if !self.held_notes.contains(note) {
                        self.held_notes.push(*note);
                    }
                }
                EventBody::NoteOff { note, .. } => {
                    self.held_notes.retain(|n| n != note);
                    if self.held_notes.is_empty() {
                        // Release current arp note
                        if let Some(cn) = self.current_note.take() {
                            context.output_events.push(Event {
                                sample_offset: event.sample_offset,
                                body: EventBody::NoteOff { channel: 0, note: cn, velocity: 0.0 },
                            });
                        }
                    }
                }
                _ => {}
            }
        }

        if self.held_notes.is_empty() {
            self.sample_counter = 0;
            self.step_index = 0;
            return ProcessStatus::Normal;
        }

        // Calculate step duration from tempo and rate
        let tempo = if context.transport.tempo > 0.0 { context.transport.tempo } else { 120.0 };
        let rate_div = self.params.rate.value() as f64; // 1=whole, 2=half, 4=quarter, 8=eighth
        let beats_per_step = 4.0 / rate_div; // e.g., rate=4 -> 1 beat per step
        let samples_per_step = (beats_per_step * 60.0 / tempo * self.sample_rate) as u64;
        let gate_frac = self.params.gate.value() as f64;
        let gate_samples = (samples_per_step as f64 * gate_frac) as u64;

        let block_size = buffer.num_samples();
        let seq = self.build_sequence();
        if seq.is_empty() {
            return ProcessStatus::Normal;
        }

        for i in 0..block_size {
            // Check if it's time for a new step
            if self.sample_counter % samples_per_step == 0 {
                // Note off for previous arp note
                if let Some(cn) = self.current_note.take() {
                    context.output_events.push(Event {
                        sample_offset: i as u32,
                        body: EventBody::NoteOff { channel: 0, note: cn, velocity: 0.0 },
                    });
                }

                // Pick next note
                let note = if self.params.pattern.value() == ArpPattern::Random {
                    let idx = self.next_random() as usize % seq.len();
                    seq[idx]
                } else {
                    let idx = self.step_index % seq.len();
                    self.step_index += 1;
                    seq[idx]
                };

                context.output_events.push(Event {
                    sample_offset: i as u32,
                    body: EventBody::NoteOn { channel: 0, note, velocity: 0.8 },
                });
                self.current_note = Some(note);
            }

            // Gate off -- release note before next step
            if self.sample_counter % samples_per_step == gate_samples {
                if let Some(cn) = self.current_note.take() {
                    context.output_events.push(Event {
                        sample_offset: i as u32,
                        body: EventBody::NoteOff { channel: 0, note: cn, velocity: 0.0 },
                    });
                }
            }

            self.sample_counter += 1;
        }

        ProcessStatus::Normal
    }

    fn layout(&self) -> truce_gui::layout::GridLayout {
        GridLayout::build("ARPEGGIO", "V0.1", 2, 80.0, vec![widgets(vec![
            knob(P::Rate, "Rate"),
            knob(P::Gate, "Gate"),
            knob(P::Octaves, "Octaves"),
            dropdown(P::Pattern, "Pattern"),
        ])])
    }
}

truce::plugin! {
    logic: Arpeggio,
    params: ArpParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_snapshot() {
        let params = std::sync::Arc::new(ArpParams::new());
        let arp = Arpeggio::new(std::sync::Arc::clone(&params));
        let layout = arp.layout();
        truce_test::assert_gui_snapshot_grid::<ArpParams>(
            "arpeggio_default", params, layout, 0,
        );
    }
}
