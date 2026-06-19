//! Tempo-synced LFO that emits a MIDI CC stream while passing audio
//! through untouched.
//!
//! Exercises plugin-to-host control-change output (the VST3
//! `LegacyMIDICCOutEvent` path and the CLAP raw-MIDI dialect) and an
//! audio effect that opts into MIDI output with `midi_output = true`,
//! so the host declares an event-output bus on a non-note-effect.

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, dropdown, knob, widgets};

use LfoCcParamsParamId as P;
use std::sync::Arc;

/// LFO-cycle length as a note value, in quarter-note beats per cycle.
#[derive(ParamEnum)]
pub enum Rate {
    Whole,
    Half,
    Quarter,
    #[name = "1/8"]
    Eighth,
    #[name = "1/16"]
    Sixteenth,
}

impl Rate {
    fn beats_per_cycle(self) -> f64 {
        match self {
            Rate::Whole => 4.0,
            Rate::Half => 2.0,
            Rate::Quarter => 1.0,
            Rate::Eighth => 0.5,
            Rate::Sixteenth => 0.25,
        }
    }
}

#[derive(Params)]
pub struct LfoCcParams {
    #[param(
        name = "CC",
        short_name = "CC",
        range = "discrete(0, 127)",
        default = 1
    )]
    pub cc: IntParam,

    #[param(name = "Rate")]
    pub rate: EnumParam<Rate>,

    #[param(name = "Depth", range = "linear(0, 1)", unit = "%", smooth = "exp(5)")]
    pub depth: FloatParam,
}

pub struct LfoCc {
    params: Arc<LfoCcParams>,
    sample_rate: f64,
    /// Free-running phase for hosts that report no transport.
    free_phase: f64,
    /// Last CC value emitted, so identical values aren't re-sent every
    /// block (which would flood the host).
    last_sent: Option<u8>,
}

impl LfoCc {
    pub fn new(params: Arc<LfoCcParams>) -> Self {
        Self {
            params,
            sample_rate: 44100.0,
            free_phase: 0.0,
            last_sent: None,
        }
    }
}

/// Free-run rate when the host reports no tempo.
const FREE_LFO_HZ: f64 = 1.0;

/// Block size as `f64` for phase math. Block sizes are small integers,
/// exact in `f64`.
#[allow(clippy::cast_precision_loss)]
fn block_len_f64(n: usize) -> f64 {
    n as f64
}

/// Unipolar `[0, 1]` level to a 7-bit CC value.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn level_to_cc(level: f64) -> u8 {
    (level.clamp(0.0, 1.0) * 127.0).round() as u8
}

impl PluginLogic for LfoCc {
    fn bus_layouts() -> Vec<BusLayout> {
        vec![BusLayout::stereo()]
    }

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.sample_rate = sample_rate;
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
        self.free_phase = 0.0;
        self.last_sent = None;
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Audio is untouched - this plugin only generates MIDI.
        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            out.copy_from_slice(inp);
        }

        let transport = context.transport;
        let beats_per_cycle = self.params.rate.value().beats_per_cycle();
        let phase = if transport.playing && transport.tempo > 0.0 {
            (transport.position_beats / beats_per_cycle).rem_euclid(1.0)
        } else {
            self.free_phase
        };

        let lfo = 0.5 - 0.5 * (phase * std::f64::consts::TAU).cos();
        let value = level_to_cc(lfo * f64::from(self.params.depth.read()));

        // One CC per block, only when the value actually changes.
        if self.last_sent != Some(value) {
            self.last_sent = Some(value);
            context.output_events.push(Event {
                sample_offset: 0,
                body: EventBody::ControlChange {
                    group: 0,
                    channel: 0,
                    cc: self.params.cc.value_u8(),
                    value,
                },
            });
        }

        let inc = FREE_LFO_HZ * block_len_f64(buffer.num_samples()) / self.sample_rate;
        self.free_phase = (self.free_phase + inc).rem_euclid(1.0);
        ProcessStatus::Normal
    }

    fn editor(&self) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::Cc, "CC"),
            dropdown(P::Rate, "Rate"),
            knob(P::Depth, "Depth"),
        ])])
        .with_title("LFO \u{2192} CC")
        .into_editor(&self.params)
    }
}

truce::plugin! {
    logic: LfoCc,
    params: LfoCcParams,
}

#[cfg(test)]
mod tests {
    // Passthrough is bit-exact, so an exact float compare is the contract.
    #![allow(clippy::float_cmp)]

    use super::*;

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn emits_midi_capability() {
        use truce_core::plugin::PluginRuntime;
        assert!(
            <Plugin as PluginRuntime>::info().emits_midi,
            "midi_output = true should set emits_midi"
        );
    }

    #[test]
    fn emits_cc_on_first_block() {
        let params = Arc::new(LfoCcParams::new());
        let mut plugin = LfoCc::new(Arc::clone(&params));
        plugin.params.depth.set_value(1.0);
        plugin.reset(44100.0, 512);

        let input = vec![vec![0.25f32; 512]; 2];
        let input_refs: Vec<&[f32]> = input.iter().map(std::vec::Vec::as_slice).collect();
        let mut output = vec![vec![0.0f32; 512]; 2];
        let mut output_refs: Vec<&mut [f32]> =
            output.iter_mut().map(std::vec::Vec::as_mut_slice).collect();
        let mut buffer = unsafe { AudioBuffer::from_slices(&input_refs, &mut output_refs, 512) };

        let events = EventList::default();
        let transport = TransportInfo::default();
        let mut output_events = EventList::default();
        let mut context = ProcessContext::new(&transport, 44100.0, 512, &mut output_events);

        plugin.process(&mut buffer, &events, &mut context);

        // Audio passed through unchanged.
        assert_eq!(output[0][0], 0.25);
        // A control-change event was emitted.
        assert!(
            output_events
                .iter()
                .any(|e| matches!(e.body, EventBody::ControlChange { .. })),
            "expected a ControlChange in the output"
        );
    }
}
