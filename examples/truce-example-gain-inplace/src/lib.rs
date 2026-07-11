//! Gain + pan demonstrating truce's zero-copy in-place I/O.
//!
//! Same DSP as `truce-example-gain`, but this one opts into
//! `supports_in_place()`. When the host hands the plugin a single buffer
//! for both the input and output of a channel, truce skips its safety
//! copy and the plugin reads and writes that shared buffer directly
//! through `AudioBuffer::in_out_mut(ch)`.
//!
//! # In-place I/O is EXPERIMENTAL and NOT RECOMMENDED
//!
//! Leave `supports_in_place()` at its default `false` unless you have a
//! measured reason not to. The default path copies each host-aliased
//! input into scratch so `input(ch)` and `output(ch)` are always disjoint
//! and your `process` stays format-agnostic. That copy is one memcpy per
//! aliased channel per block - a few hundred KB/sec at audio rates, which
//! is negligible for essentially every plugin.
//!
//! Opting in trades that safety for a micro-optimization and makes the
//! contract fussier: every channel read must branch on `is_in_place(ch)`,
//! because for an in-place channel `input(ch)` is empty and the data
//! lives only in the shared buffer. Forget the branch on one channel and
//! you read silence (or, before the buffer hardening, crashed). Only
//! reach for this if profiling proves the copy is a real bottleneck.
//!
//! This example exists to document the path, not to endorse it.

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, meter, widgets, xy_pad};

use GainInPlaceParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct GainInPlaceParams {
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

/// Per-sample linear gain for one channel under the pan law: channel 0
/// (left) attenuates as pan moves right, channel 1 (right) as it moves
/// left. Channels past a stereo pair follow the right-channel law.
fn channel_gain(ch: usize, gain_lin: f32, pan: f32) -> f32 {
    if ch == 0 {
        gain_lin * (1.0 - pan.max(0.0))
    } else {
        gain_lin * (1.0 + pan.min(0.0))
    }
}

/// Stateless descriptor - gain carries no DSP state, only params.
pub struct GainInPlace;

impl PurePluginLogic for GainInPlace {
    type Params = GainInPlaceParams;

    /// Opt into zero-copy in-place I/O. EXPERIMENTAL and NOT RECOMMENDED
    /// (see the module docs); the default `false` is the right choice for
    /// almost every plugin. With this `true`, host-aliased channels arrive
    /// with an empty `input(ch)` and must be processed via `in_out_mut`.
    fn supports_in_place() -> bool {
        true
    }

    fn process(
        params: &GainInPlaceParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // In-place forces channel-major processing: each `in_out_mut(ch)`
        // borrows the buffer mutably, so only one channel's slice is live
        // at a time. That rules out the plain gain example's per-sample
        // "read the smoother once, apply across every channel" loop. To
        // keep per-sample smoothing anyway, read each smoother's ramp into
        // a small stack scratch once per chunk and reuse it for every
        // channel. A `PurePluginLogic` leaf has no DspState for a
        // block-length buffer, so long blocks are sliced into `CHUNK`s.
        const CHUNK: usize = 64;
        let mut gain_lin = [0.0f32; CHUNK];
        let mut pan = [0.0f32; CHUNK];

        let n = buffer.num_samples();
        let channels = buffer.channels();
        let mut pos = 0;
        while pos < n {
            let len = (n - pos).min(CHUNK);
            // Advance both smoothers by `len` exactly once, then reuse the
            // ramps for every channel below - the smoother must not be
            // re-advanced per channel.
            params.gain.read_into(&mut gain_lin[..len]);
            for g in &mut gain_lin[..len] {
                *g = db_to_linear(*g);
            }
            params.pan.read_into(&mut pan[..len]);

            for ch in 0..channels {
                // The in-place contract: branch on `is_in_place(ch)`. For a
                // host-aliased channel `input(ch)` is empty and the samples
                // live only in the shared buffer, reached through
                // `in_out_mut` (read the current value, write in place).
                if buffer.is_in_place(ch) {
                    let io = &mut buffer.in_out_mut(ch)[pos..pos + len];
                    for i in 0..len {
                        io[i] *= channel_gain(ch, gain_lin[i], pan[i]);
                    }
                } else {
                    // Not aliased (or a host that passes disjoint buffers):
                    // the ordinary input -> output path. A plugin that opts
                    // into in-place still has to handle this case.
                    let (inp, out) = buffer.io(ch);
                    let out = &mut out[pos..pos + len];
                    let inp = &inp[pos..pos + len];
                    for i in 0..len {
                        out[i] = inp[i] * channel_gain(ch, gain_lin[i], pan[i]);
                    }
                }
            }
            pos += len;
        }

        // Meters read the output buffer - the shared buffer post-write for
        // in-place channels, the discrete output otherwise.
        if buffer.num_output_channels() >= 1 {
            context.set_meter(P::MeterLeft, buffer.output_peak(0));
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MeterRight, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<GainInPlaceParams>) -> Box<dyn Editor> {
        // Same layout as `truce-example-gain`: knob row top-left, XY pad
        // below, meter pinned to column 2 spanning three rows; resizable
        // and snapping to whole cells.
        GridLayout::build(vec![widgets(vec![
            knob(P::Gain, "Gain").at(0, 0),
            knob(P::Pan, "Pan").at(1, 0),
            xy_pad(P::Pan, P::Gain, "XY").at(0, 1),
            meter(&[P::MeterLeft, P::MeterRight], "Level")
                .at(2, 0)
                .rows(3),
        ])])
        .with_title("GAIN (IN-PLACE)")
        .with_cols(3)
        .resizable(true)
        .min_size((3, 3))
        .max_size((8, 6))
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: GainInPlace,
    params: GainInPlaceParams,
}

truce::enable_rt_paranoid!();

#[cfg(test)]
mod tests {
    use super::*;

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
    fn bus_config_effect() {
        truce_test::assert_bus_config_effect::<Plugin>();
    }

    /// Correctness of the gain DSP through the offline driver. The driver
    /// passes disjoint in/out buffers, so this exercises the non-aliased
    /// branch of `process`; the true in-place (host-aliased) path is
    /// covered by `truce_core`'s buffer unit tests.
    #[test]
    fn applies_gain() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .duration(Duration::from_millis(50))
            .input(InputSource::Constant(0.5))
            .set_param(P::Gain, 1.0) // +6 dB, centered pan
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_nonzero(&result);
        assertions::assert_peak_below(&result, 1.0);
    }
}
