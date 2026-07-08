//! MIDI arpeggiator.
//!
//! Rebuilds the arp sequence into pre-sized, reused buffers instead of a
//! fresh `Vec` each block, so `process` never allocates on the audio thread.
//! The `rt-paranoid` test asserts it stays allocation-free.

use truce::prelude::*;
use truce_core::cast::len_u32;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, dropdown, knob, widgets};

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

use ArpParamsParamId as P;
use std::sync::Arc;

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
    pub octaves: IntParam,

    #[param(name = "Pattern", short_name = "Pat")]
    pub pattern: EnumParam<ArpPattern>,

    #[param(name = "Gate", range = "linear(0.1, 1.0)", default = 0.8, unit = "%")]
    pub gate: FloatParam,
}

// --- Numeric helpers ---

/// `i64 → f64` for step-count → beat math. Step counts stay well
/// below 2^53 in practice (a 24 h session at 1/64 notes is ~10^7).
#[inline]
#[allow(clippy::cast_precision_loss)]
fn step_as_f64(s: i64) -> f64 {
    s as f64
}

/// Floor-divide a beat position by beats-per-step to get the step
/// index. The cast saturates for absurd inputs; real arps never
/// approach that range.
#[inline]
#[allow(clippy::cast_possible_truncation)]
fn beat_to_step(beat: f64, beats_per_step: f64) -> i64 {
    (beat / beats_per_step).floor() as i64
}

/// Wrap a (possibly negative) step index into `0..seq_len`.
/// `seq_len` is bounded by held-note polyphony.
#[inline]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn step_to_seq_idx(step: i64, seq_len: usize) -> usize {
    step.rem_euclid(seq_len as i64) as usize
}

// --- Plugin ---

/// Stateless descriptor - the arp's per-block DSP state is [`ArpeggioDspState`].
pub struct Arpeggio;

pub struct ArpeggioDspState {
    held_notes: Vec<u8>,
    /// Reusable arp sequence, rebuilt in place each block (never
    /// reallocated on the audio thread). Sized for the worst case: 4
    /// octaves of full 128-note polyphony, doubled for the up-down
    /// pattern.
    sequence: Vec<u8>,
    /// Scratch for the up-down pattern's reversed middle section.
    scratch_down: Vec<u8>,
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

impl ArpeggioDspState {
    /// Rebuild `self.sequence` in place from the held notes - no audio-
    /// thread allocation, since every buffer is pre-sized in `init`.
    fn rebuild_sequence(&mut self, params: &ArpParams) {
        self.sequence.clear();
        if self.held_notes.is_empty() {
            return;
        }

        // Sort in place: `held_notes` is a set, so its order is
        // irrelevant, and this avoids cloning it every block.
        self.held_notes.sort_unstable();

        let octaves = params.octaves.value_u8();
        for oct in 0..octaves {
            for &note in &self.held_notes {
                if let Some(n) = note.checked_add(oct.saturating_mul(12))
                    && n <= 127
                {
                    self.sequence.push(n);
                }
            }
        }

        match params.pattern.value() {
            ArpPattern::Down => {
                self.sequence.reverse();
            }
            ArpPattern::UpDown if self.sequence.len() > 2 => {
                let last = self.sequence.len() - 1;
                self.scratch_down.clear();
                self.scratch_down.extend_from_slice(&self.sequence[1..last]);
                self.scratch_down.reverse();
                self.sequence.extend_from_slice(&self.scratch_down);
            }
            _ => {} // Up and Random use the sequence as-is
        }
    }

    fn next_random(&mut self) -> u32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        self.rng
    }
}

impl PluginLogic for Arpeggio {
    type Params = ArpParams;
    type DspState = ArpeggioDspState;

    /// MIDI effect: no audio I/O. CLAP/VST3/AU(aumi)/LV2 honor this;
    /// AAX (which has no audio-less plugin category) auto-adds a
    /// stereo passthrough inside `truce-aax` so the DAW's track
    /// audio flows through unchanged.
    fn bus_layouts() -> Vec<BusLayout> {
        vec![BusLayout::new()]
    }

    fn init(_params: &ArpParams) -> ArpeggioDspState {
        ArpeggioDspState {
            // Pre-size so the audio thread never reallocates: at most 128
            // distinct held notes, and a worst-case 4-octave up-down
            // sequence.
            held_notes: Vec::with_capacity(128),
            sequence: Vec::with_capacity(1024),
            scratch_down: Vec::with_capacity(512),
            sample_rate: 44100.0,
            last_step: None,
            active_note: None,
            free_beat: 0.0,
            rng: 12345,
        }
    }

    fn reset(state: &mut ArpeggioDspState, params: &ArpParams, config: &AudioConfig) {
        let sample_rate = config.sample_rate;
        state.sample_rate = sample_rate;
        params.set_sample_rate(sample_rate);
        params.snap_smoothers();
        state.held_notes.clear();
        state.last_step = None;
        state.active_note = None;
        state.free_beat = 0.0;
    }

    fn process(
        state: &mut ArpeggioDspState,
        params: &ArpParams,
        buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Process input MIDI -- track held notes
        for event in events.iter() {
            match &event.body {
                EventBody::NoteOn { note, .. } if !state.held_notes.contains(note) => {
                    state.held_notes.push(*note);
                }
                EventBody::NoteOff { note, .. } => {
                    state.held_notes.retain(|n| n != note);
                    if state.held_notes.is_empty() {
                        // Release current arp note
                        if let Some(cn) = state.active_note.take() {
                            context.output_events.push(Event::new(
                                event.sample_offset,
                                EventBody::NoteOff {
                                    group: 0,
                                    channel: 0,
                                    note: cn,
                                    velocity: 0,
                                },
                            ));
                        }
                    }
                }
                _ => {}
            }
        }

        if state.held_notes.is_empty() {
            // Clear phase state so the next held chord re-triggers on
            // the next step boundary rather than carrying over the old
            // step index.
            state.last_step = None;
            return ProcessStatus::Normal;
        }

        state.rebuild_sequence(params);
        if state.sequence.is_empty() {
            return ProcessStatus::Normal;
        }
        // Move the sequence out for the sample loop so it can call
        // `&mut state` (`next_random`) without holding a borrow of
        // `state.sequence`. Put back below - the swap reuses the buffer's
        // capacity, so no allocation.
        let seq = std::mem::take(&mut state.sequence);

        let beats_per_step = params.rate.value().beats_per_step();
        let gate_frac = f64::from(params.gate.value());

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
        let beats_per_sample = tempo / 60.0 / state.sample_rate;
        let block_start_beat = if host_locked {
            transport.position_beats
        } else {
            state.free_beat
        };

        let pattern = params.pattern.value();
        let mut beat = block_start_beat;

        for i in 0..buffer.num_samples() {
            let step_num = beat_to_step(beat, beats_per_step);

            if Some(step_num) != state.last_step {
                // Step boundary: release the previous note (if still
                // sounding past gate-off, this is a no-op) and trigger
                // the next step.
                if let Some(cn) = state.active_note.take() {
                    context.output_events.push(Event::new(
                        len_u32(i),
                        EventBody::NoteOff {
                            group: 0,
                            channel: 0,
                            note: cn,
                            velocity: 0,
                        },
                    ));
                }
                let note = if pattern == ArpPattern::Random {
                    let idx = state.next_random() as usize % seq.len();
                    seq[idx]
                } else {
                    seq[step_to_seq_idx(step_num, seq.len())]
                };
                context.output_events.push(Event::new(
                    len_u32(i),
                    EventBody::NoteOn {
                        group: 0,
                        channel: 0,
                        note,
                        velocity: 102,
                    },
                ));
                state.active_note = Some(note);
                state.last_step = Some(step_num);
            } else if let Some(step) = state.last_step {
                // Same step - check whether we've crossed the gate-off
                // boundary within it.
                let gate_off_beat = (step_as_f64(step) + gate_frac) * beats_per_step;
                if beat >= gate_off_beat
                    && let Some(cn) = state.active_note.take()
                {
                    context.output_events.push(Event::new(
                        len_u32(i),
                        EventBody::NoteOff {
                            group: 0,
                            channel: 0,
                            note: cn,
                            velocity: 0,
                        },
                    ));
                }
            }

            beat += beats_per_sample;
        }

        // Restore the sequence buffer (keeps its capacity for next block).
        state.sequence = seq;

        // Keep the free-running counter aligned so dropping out of host
        // mode mid-session doesn't cause a phase jump.
        state.free_beat = beat;

        ProcessStatus::Normal
    }

    fn editor(params: Arc<ArpParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            dropdown(P::Rate, "Rate"),
            knob(P::Gate, "Gate"),
            knob(P::Octaves, "Octaves"),
            dropdown(P::Pattern, "Pattern"),
        ])])
        .with_cols(2)
        .with_title("ARPEGGIO")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: Arpeggio,
    params: ArpParams,
}

truce::enable_rt_paranoid!();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_is_realtime_clean() {
        use std::time::Duration;
        use truce_test::{InputSource, assert_realtime_clean, driver};
        assert_realtime_clean(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.25))
                .script(|s| {
                    s.note_on(60, 0.8);
                    s.set_param(P::Rate, 0.9);
                    s.wait_ms(15);
                    s.set_param(P::Rate, 0.1);
                    s.wait_ms(15);
                    s.note_off(60);
                })
                .run()
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/arpeggio_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/arpeggio_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/arpeggio_default_windows.png")
            .pixel_threshold(2)
            .run();
    }

    /// `category = "midi"` must surface as `PluginCategory::NoteEffect`;
    /// any other category turns off the host's MIDI decode path and
    /// the plugin silently ignores host MIDI.
    #[test]
    fn category_is_note_effect() {
        use truce_core::info::PluginCategory;
        use truce_core::plugin::PluginRuntime;
        assert_eq!(
            <Plugin as PluginRuntime>::info().category,
            PluginCategory::NoteEffect
        );
    }
}
