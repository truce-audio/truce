//! Fully SIMD-optimized gain plugin: the block-rate variant plus a
//! vectorized envelope precompute.
//!
//! Fast path on converged smoothers (`is_smoothing()` returns
//! `false` for both params), slow path on active smoothing. The
//! slow path's `dB → linear` envelope uses
//! [`truce_simd::math::db_to_linear_block`] instead of a scalar
//! `db_to_linear` loop. With many smoothed gain knobs in flight
//! (mixer with N stems, send/return rack, channel strip), the
//! cumulative cost of the transcendental precompute would be the
//! residual gap between this design and the pre-vectorization
//! baseline; routing through the vectorized math closes it.
//!
//! Trade-off vs. a scalar precompute: one extra stack allocation
//! (`lin: [f32; MAX_BLOCK]`) and one extra pass over the envelope.
//! Net win measurable when N smoothers ≥ ~4.

use truce::prelude::*;
use truce_core::buffer::ChunkItem;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, meter, widgets, xy_pad};
use truce_simd::{math, ops};

use GainParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct GainParams {
    #[param(
        name = "Gain",
        range = "linear(-60, 6)",
        unit = "dB",
        smooth = "exp(5)"
    )]
    pub gain: FloatParam,

    #[param(name = "Pan", range = "linear(-1, 1)", unit = "pan", smooth = "exp(5)")]
    pub pan: FloatParam,

    #[meter]
    pub meter_left: MeterSlot,

    #[meter]
    pub meter_right: MeterSlot,
}

/// Stateless descriptor - gain carries no DSP state, only params.
pub struct Gain;

const N: usize = 32;
const MAX_BLOCK: usize = 1024;

impl PluginLogic for Gain {
    type Params = GainParams;
    type DspState = ();

    fn init(_params: &GainParams) {}

    fn process(
        _state: &mut (),
        params: &GainParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        if !params.gain.is_smoothing() && !params.pan.is_smoothing() {
            // Fast path: gain constant for the whole block.
            let gain_db = params.gain.value();
            let pan = params.pan.value();
            let lin = db_to_linear(gain_db);
            let gl = lin * (1.0 - pan.max(0.0));
            let gr = lin * (1.0 + pan.min(0.0));

            let nch = buffer.channels();
            for ch in 0..nch {
                let g = if ch == 0 { gl } else { gr };
                let (inp, out) = buffer.io(ch);
                ops::scale_block(out, inp, g);
            }
        } else {
            // Slow path: vectorized envelope precompute, then SIMD
            // apply via chunks_mut. `read_into` advances each
            // smoother by exactly `n` (matching what we consume), so
            // the gain doesn't step at the next block edge.
            let n = buffer.num_samples().min(MAX_BLOCK);
            let mut gain_db = [0.0_f32; MAX_BLOCK];
            let mut pan = [0.0_f32; MAX_BLOCK];
            params.gain.read_into(&mut gain_db[..n]);
            params.pan.read_into(&mut pan[..n]);

            // Vectorize the transcendental into `lin`. This is the
            // only step in the slow path that doesn't autovectorize
            // - libm's `10.0_f32.powf(x)` is opaque to LLVM.
            // db_to_linear_block routes through wide's native
            // `exp`, so the dB → linear conversion runs in f32x8
            // chunks (or NEON on aarch64).
            let mut lin = [0.0_f32; MAX_BLOCK];
            math::db_to_linear_block(&mut lin[..n], &gain_db[..n]);

            // The pan split (max/min/sub/mul) autovectorizes
            // cleanly under -O; no explicit SIMD needed.
            let mut g_l = [0.0_f32; MAX_BLOCK];
            let mut g_r = [0.0_f32; MAX_BLOCK];
            for i in 0..n {
                g_l[i] = lin[i] * (1.0 - pan[i].max(0.0));
                g_r[i] = lin[i] * (1.0 + pan[i].min(0.0));
            }

            let mut chunks = buffer.chunks_mut::<N>();
            while let Some(chunk) = chunks.next() {
                let (ch, sample, inp, out): (usize, usize, &[f32], &mut [f32]) = match chunk {
                    ChunkItem::Full {
                        ch,
                        sample,
                        inp,
                        out,
                    } => (ch, sample, &inp[..], &mut out[..]),
                    ChunkItem::Tail {
                        ch,
                        sample,
                        inp,
                        out,
                    } => (ch, sample, inp, out),
                };
                let env = if ch == 0 { &g_l } else { &g_r };
                ops::mul_block(out, inp, &env[sample..sample + inp.len()]);
            }
        }

        if buffer.num_output_channels() >= 1 {
            context.set_meter(P::MeterLeft, buffer.output_peak(0));
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MeterRight, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<GainParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::Gain, "Gain"),
            knob(P::Pan, "Pan"),
            meter(&[P::MeterLeft, P::MeterRight], "Level")
                .at(2, 0)
                .rows(3),
            xy_pad(P::Pan, P::Gain, "XY"),
        ])])
        .with_title("GAIN SIMD")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: Gain,
    params: GainParams,
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
                    s.set_param(P::Gain, 0.9);
                    s.wait_ms(15);
                    s.set_param(P::Gain, 0.1);
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
    fn renders_nonzero_output() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .duration(Duration::from_millis(12))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_nonzero(&result);
        assertions::assert_no_nans(&result);
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
    fn param_defaults_match() {
        truce_test::assert_param_defaults_match::<Plugin>();
    }

    #[test]
    fn param_normalized_clamped() {
        truce_test::assert_param_normalized_clamped::<Plugin>();
    }

    #[test]
    fn param_normalized_roundtrip() {
        truce_test::assert_param_normalized_roundtrip::<Plugin>();
    }

    #[test]
    fn no_duplicate_param_ids() {
        truce_test::assert_no_duplicate_param_ids::<Plugin>();
    }

    #[test]
    fn driver_passthrough() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .sample_rate(44_100.0)
            .channels(2)
            .block_size(256)
            .duration(Duration::from_secs(1))
            .input(InputSource::Constant(0.5))
            .run();

        assertions::assert_no_nans(&result);
        assertions::assert_nonzero(&result);
        assertions::assert_peak_below(&result, 1.0);
    }

    /// At default gain (0 dB) + default pan (0), output equals
    /// input. Exercises the converged-smoother fast path.
    #[test]
    fn unity_at_defaults() {
        use std::time::Duration;
        use truce_test::{InputSource, driver};

        let result = driver!(Plugin)
            .duration(Duration::from_millis(50))
            .input(InputSource::Constant(0.5))
            .run();

        let max = result.output[0]
            .iter()
            .map(|s| s.abs())
            .fold(0.0_f32, f32::max);
        assert!((max - 0.5).abs() < 0.01, "expected ~0.5, got {max}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/block_gain_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/block_gain_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/block_gain_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
