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

/// Stateless descriptor - the follower's per-block DSP state is [`EnvelopeDspState`].
pub struct Envelope;

pub struct EnvelopeDspState {
    sample_rate: f64,
    /// Follower state, peak-tracked with instant attack and a
    /// parameterised release.
    env: f32,
    /// Last CC value sent, so identical values aren't re-sent every
    /// block (which would flood the host).
    last_sent: Option<u8>,
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
    type Params = EnvelopeParams;
    type DspState = EnvelopeDspState;

    fn bus_layouts() -> Vec<BusLayout> {
        vec![BusLayout::stereo()]
    }

    fn init(_params: &EnvelopeParams) -> EnvelopeDspState {
        EnvelopeDspState {
            sample_rate: 44100.0,
            env: 0.0,
            last_sent: None,
        }
    }

    fn reset(state: &mut EnvelopeDspState, _params: &EnvelopeParams, config: &AudioConfig) {
        let sample_rate = config.sample_rate;
        state.sample_rate = sample_rate;
        state.env = 0.0;
        state.last_sent = None;
    }

    fn process(
        state: &mut EnvelopeDspState,
        params: &EnvelopeParams,
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
        let coeff = release_coeff(params.release.read(), state.sample_rate);
        let cc = params.cc.value_u8();
        let nch = buffer.channels();
        for i in 0..buffer.num_samples() {
            let mut peak = 0.0f32;
            for ch in 0..nch {
                peak = peak.max(buffer.input(ch)[i].abs());
            }
            state.env = if peak > state.env {
                peak
            } else {
                peak + (state.env - peak) * coeff
            };

            // De-duplicated: identical consecutive values aren't re-sent,
            // so a steady level stays quiet instead of flooding the host.
            let value = level_to_cc(state.env);
            if state.last_sent != Some(value) {
                state.last_sent = Some(value);
                context.output_events.push(Event::new(
                    len_u32(i),
                    EventBody::ControlChange {
                        group: 0,
                        channel: 0,
                        cc,
                        value,
                    },
                ));
            }
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<EnvelopeParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::Cc, "CC"),
            knob(P::Release, "Release"),
        ])])
        .with_title("ENVELOPE")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: Envelope,
    params: EnvelopeParams,
}

truce::enable_rt_paranoid!();

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
    fn process_is_allocation_free() {
        use std::time::Duration;
        use truce_test::{InputSource, assert_no_audio_alloc, driver};
        assert_no_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.25))
                .script(|s| {
                    s.set_param(P::Cc, 0.9);
                    s.wait_ms(15);
                    s.set_param(P::Cc, 0.1);
                    s.wait_ms(15);
                })
                .run()
        });
    }

    #[test]
    fn follows_level_to_cc() {
        let params = EnvelopeParams::new();
        let mut state = Envelope::init(&params);
        Envelope::reset(&mut state, &params, &AudioConfig::new(44100.0, 256));

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
        Envelope::process(&mut state, &params, &mut buffer, &events, &mut context);

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
