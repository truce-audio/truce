//! Envelope follower to MIDI CC: tracks the input signal's level and
//! emits it as a control-change, passing the audio through unchanged.
//!
//! A practical "audio effect that emits MIDI" - it reads the input it's
//! inserted on, so being an effect is the point: drop it on a track and
//! use that track's dynamics to modulate another instrument or effect
//! over MIDI. Needs `midi_output = true` in truce.toml; exercises the
//! plugin-to-host CC path (the VST3 `LegacyMIDICCOutEvent` path and the
//! CLAP raw-MIDI dialect).

use truce::prelude::*;
use truce_core::cast::len_u32;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, widgets};

use EnvelopeParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct EnvelopeParams {
    #[param(
        name = "CC",
        short_name = "CC",
        range = "discrete(0, 127)",
        default = 1
    )]
    pub cc: IntParam,

    #[param(
        name = "Release",
        range = "log(1, 1000)",
        default = 100.0,
        unit = "ms",
        smooth = "exp(10)"
    )]
    pub release: FloatParam,
}

pub struct Envelope {
    params: Arc<EnvelopeParams>,
    sample_rate: f64,
    /// Follower state, peak-tracked with instant attack and a
    /// parameterised release.
    env: f32,
    /// Last CC value sent, so identical values aren't re-sent every
    /// block (which would flood the host).
    last_sent: Option<u8>,
}

impl Envelope {
    pub fn new(params: Arc<EnvelopeParams>) -> Self {
        Self {
            params,
            sample_rate: 44100.0,
            env: 0.0,
            last_sent: None,
        }
    }
}

/// Per-sample release coefficient for a time constant of `ms`.
#[allow(clippy::cast_possible_truncation)]
fn release_coeff(ms: f32, sr: f64) -> f32 {
    let seconds = f64::from(ms) / 1000.0;
    (-1.0 / (seconds * sr)).exp() as f32
}

/// Unipolar `[0, 1]` level to a 7-bit CC value.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn level_to_cc(level: f32) -> u8 {
    (level.clamp(0.0, 1.0) * 127.0).round() as u8
}

impl PluginLogic for Envelope {
    fn bus_layouts() -> Vec<BusLayout> {
        vec![BusLayout::stereo()]
    }

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.sample_rate = sample_rate;
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
        self.env = 0.0;
        self.last_sent = None;
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Pass the audio through untouched.
        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            out.copy_from_slice(inp);
        }

        // Follow the peak across channels (instant attack, smoothed
        // release) and emit a control-change at the exact sample its
        // 7-bit value changes. Emitting sample-accurately on change -
        // rather than one end-of-block value stamped at `sample_offset:
        // 0` - keeps the stream smooth and tracks rises/falls within a
        // block; the old block-rate form showed up in a MIDI monitor as
        // a delayed `@0` burst once per block.
        let coeff = release_coeff(self.params.release.read(), self.sample_rate);
        let cc = self.params.cc.value_u8();
        let nch = buffer.channels();
        for i in 0..buffer.num_samples() {
            let mut peak = 0.0f32;
            for ch in 0..nch {
                peak = peak.max(buffer.input(ch)[i].abs());
            }
            self.env = if peak > self.env {
                peak
            } else {
                peak + (self.env - peak) * coeff
            };

            // De-duplicated: identical consecutive values aren't re-sent,
            // so a steady level stays quiet instead of flooding the host.
            let value = level_to_cc(self.env);
            if self.last_sent != Some(value) {
                self.last_sent = Some(value);
                context.output_events.push(Event {
                    sample_offset: len_u32(i),
                    body: EventBody::ControlChange {
                        group: 0,
                        channel: 0,
                        cc,
                        value,
                    },
                });
            }
        }

        ProcessStatus::Normal
    }

    fn editor(&self) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::Cc, "CC"),
            knob(P::Release, "Release"),
        ])])
        .with_title("ENVELOPE")
        .into_editor(&self.params)
    }
}

truce::plugin! {
    logic: Envelope,
    params: EnvelopeParams,
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
    fn follows_level_to_cc() {
        let params = Arc::new(EnvelopeParams::new());
        let mut plugin = Envelope::new(Arc::clone(&params));
        plugin.reset(44100.0, 256);

        // A half-scale constant signal → env settles at ~0.5 → CC ~63.
        let input = vec![vec![0.5f32; 256]; 2];
        let input_refs: Vec<&[f32]> = input.iter().map(std::vec::Vec::as_slice).collect();
        let mut output = vec![vec![0.0f32; 256]; 2];
        let mut output_refs: Vec<&mut [f32]> =
            output.iter_mut().map(std::vec::Vec::as_mut_slice).collect();
        let mut buffer = unsafe { AudioBuffer::from_slices(&input_refs, &mut output_refs, 256) };

        let events = EventList::default();
        let transport = TransportInfo::default();
        let mut output_events = EventList::default();
        let mut context = ProcessContext::new(&transport, 44100.0, 256, &mut output_events);
        plugin.process(&mut buffer, &events, &mut context);

        // Audio passed through.
        assert_eq!(output[0][0], 0.5);
        // A control-change reflecting the level was emitted.
        let cc = output_events.iter().find_map(|e| match e.body {
            EventBody::ControlChange { value, .. } => Some(value),
            _ => None,
        });
        assert_eq!(cc, Some(64)); // round(0.5 * 127) = 64 (instant attack)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/envelope_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/envelope_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/envelope_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
