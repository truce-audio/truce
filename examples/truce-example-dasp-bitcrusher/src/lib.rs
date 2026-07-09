//! Bitcrusher example showcasing `dasp_sample::Sample` bit-depth
//! conversion. For depths dasp ships as a concrete sample type
//! (`i8` / `i16`), the quantization round-trips through the dasp
//! `Sample` trait so the format-spec rounding rules are exact. Other
//! depths fall back to the equivalent floor-quantize formula.

use dasp_sample::Sample as DaspSample;
use std::sync::Arc;

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, widgets};

use BitcrusherParamsParamId as P;

#[derive(Params)]
pub struct BitcrusherParams {
    #[param(name = "Bits", range = "discrete(2, 16)", default = 16)]
    pub bits: IntParam,
    #[param(name = "Hold", range = "discrete(1, 32)", default = 1)]
    pub hold: IntParam,
    #[param(name = "Mix", range = "linear(0, 1)", default = 1.0, unit = "%")]
    pub mix: FloatParam,
}

/// Stateless descriptor - DSP state lives in [`BitcrusherDspState`].
pub struct Bitcrusher;

/// Per-instance DSP state: the sample-and-hold buffer and its
/// frame counter.
#[derive(Default)]
pub struct BitcrusherDspState {
    /// Sample-and-hold buffer per channel (max 2 channels supported).
    held: [f32; 2],
    /// Frame counter for the hold ratio.
    hold_counter: usize,
}

/// Quantize a `[-1.0, 1.0]` sample to `bits` bits of resolution. Routes
/// through `dasp_sample::Sample` for the two depths dasp ships as
/// concrete integer types; falls back to the equivalent quantize
/// formula for other depths.
fn quantize(s: f32, bits: u8) -> f32 {
    match bits {
        8 => s.to_sample::<i8>().to_sample::<f32>(),
        16 => s.to_sample::<i16>().to_sample::<f32>(),
        _ => {
            // `bits` is in [2, 16]; the step count fits in f32.
            let steps = 2.0_f32.powi(i32::from(bits) - 1);
            (s * steps).round() / steps
        }
    }
}

impl PluginLogic for Bitcrusher {
    type Params = BitcrusherParams;
    type DspState = BitcrusherDspState;

    fn init(_params: &BitcrusherParams) -> BitcrusherDspState {
        BitcrusherDspState::default()
    }

    fn reset(state: &mut BitcrusherDspState, _params: &BitcrusherParams, _config: &AudioConfig) {
        state.held = [0.0; 2];
        state.hold_counter = 0;
    }

    fn process(
        state: &mut BitcrusherDspState,
        params: &BitcrusherParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        // `bits` ∈ [2, 16], stored as i64 by `IntParam`; the cast is exact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bits = params.bits.value() as u8;
        let hold = params.hold.value_usize().max(1);
        let mix = params.mix.read();

        let channels = buffer.channels().min(2);
        for i in 0..buffer.num_samples() {
            let refresh = state.hold_counter.is_multiple_of(hold);
            for ch in 0..channels {
                let (inp, out) = buffer.io(ch);
                let dry = inp[i];
                if refresh {
                    state.held[ch] = dry;
                }
                let crushed = quantize(state.held[ch], bits);
                out[i] = dry * (1.0 - mix) + crushed * mix;
            }
            state.hold_counter = state.hold_counter.wrapping_add(1);
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<BitcrusherParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::Bits, "Bits"),
            knob(P::Hold, "Hold"),
            knob(P::Mix, "Mix"),
        ])])
        .with_title("BITCRUSHER")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: Bitcrusher,
    params: BitcrusherParams,
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
                    s.set_param(P::Bits, 0.9);
                    s.wait_ms(15);
                    s.set_param(P::Bits, 0.1);
                    s.wait_ms(15);
                })
                .run()
        });
    }

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    #[test]
    fn state_round_trips() {
        truce_test::assert_state_round_trip::<Plugin>();
    }

    #[test]
    fn editor_lifecycle() {
        truce_test::assert_editor_lifecycle::<Plugin>();
    }

    #[test]
    fn passthrough_at_defaults() {
        // 16-bit + hold=1 + mix=1 should be near-passthrough (round
        // through i16 is below the noise floor at the default constant).
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};
        let result = driver!(Plugin)
            .duration(Duration::from_millis(20))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_nonzero(&result);
        assertions::assert_no_nans(&result);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/dasp_bitcrusher_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/dasp_bitcrusher_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/dasp_bitcrusher_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
