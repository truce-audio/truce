//! 5.1 surround pass-through with per-channel dB-scale meters.
//!
//! Exists to demo [`math::linear_to_db_block`] in its natural
//! habitat: an array of per-channel peaks converted all at once.
//! For one or two peaks, a scalar `20 * x.log10()` is cheaper; the
//! SIMD form starts winning around 4+ elements, which 5.1 surround
//! gives us exactly.
//!
//! Also one of the only examples in the tree with a non-stereo bus
//! layout. The plugin advertises both stereo and 5.1 in its
//! `bus_layouts()` override so it loads in DAWs without surround
//! support; channels beyond the host-selected layout's count are
//! simply not touched.
//!
//! Per block:
//!
//! 1. `gain_block(out, trim_lin)` per channel after `copy_block`
//!    (apply the trim).
//! 2. `abs_max_block` per channel → fill a `[f32; CHANS]` of
//!    linear peaks.
//! 3. `linear_to_db_block` on that array → `[f32; CHANS]` of dB
//!    peaks, in one SIMD call.
//! 4. Clamp each dB value to `[-60, 0]` and re-normalise to
//!    `[0, 1]` for the meter widget's linear-scale display, which
//!    in dB space is the perceptually correct meter.

use truce::prelude::*;
use truce_core::bus::{BusLayout, ChannelConfig};
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, meter, widgets};
use truce_simd::{math, ops};

use SurroundMeterParamsParamId as P;
use std::sync::Arc;

/// Channels covered by the meter array. 5.1 = 6 (L, R, C, LFE,
/// Ls, Rs). Plugin loads as stereo too; channels beyond
/// `buffer.channels()` keep their last meter value (which decays
/// to zero on the next silent block).
const CHANS: usize = 6;

#[derive(Params)]
pub struct SurroundMeterParams {
    #[param(
        name = "Trim",
        range = "linear(-24, 24)",
        default = 0.0,
        unit = "dB",
        smooth = "exp(10)"
    )]
    pub trim: FloatParam,

    #[meter]
    pub ch_l: MeterSlot,
    #[meter]
    pub ch_r: MeterSlot,
    #[meter]
    pub ch_c: MeterSlot,
    #[meter]
    pub ch_lfe: MeterSlot,
    #[meter]
    pub ch_ls: MeterSlot,
    #[meter]
    pub ch_rs: MeterSlot,
}

/// Stateless descriptor - the meter carries no DSP state, only params.
pub struct SurroundMeter;

const METER_IDS: [P; CHANS] = [P::ChL, P::ChR, P::ChC, P::ChLfe, P::ChLs, P::ChRs];

/// Bottom of the meter scale in dB. Anything quieter renders as
/// "empty" on the widget.
const METER_FLOOR_DB: f32 = -60.0;

impl PluginLogic for SurroundMeter {
    type Params = SurroundMeterParams;
    type DspState = ();

    fn init(_params: &SurroundMeterParams) {}

    fn bus_layouts() -> Vec<BusLayout> {
        // CHANS is a compile-time constant (6) that fits trivially
        // in u32; the cast is the cleanest way to feed it to
        // ChannelConfig::Custom.
        #[allow(clippy::cast_possible_truncation)]
        let n = CHANS as u32;
        vec![
            BusLayout::stereo(),
            BusLayout::new()
                .with_input("Surround", ChannelConfig::Custom(n))
                .with_output("Surround", ChannelConfig::Custom(n)),
        ]
    }

    fn process(
        _state: &mut (),
        params: &SurroundMeterParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Trim is applied block-constant via `scale_block`;
        // `read_after(n)` advances the smoother by the whole block
        // so the declared exp(10) settling actually completes in
        // ~10 ms wall-clock instead of ~10 blocks.
        let trim_lin = db_to_linear(params.trim.read_after(buffer.num_samples()));
        let nch = buffer.channels().min(CHANS);

        // Apply trim and collect per-channel linear peaks. One
        // scale_block per channel (out = in * trim) then a peak
        // scan; same shape the gain plugin's fast path ships.
        let mut peaks_lin = [0.0_f32; CHANS];
        for (ch, slot) in peaks_lin.iter_mut().take(nch).enumerate() {
            let (inp, out) = buffer.io(ch);
            ops::scale_block(out, inp, trim_lin);
            *slot = ops::abs_max_block(out);
        }

        // Convert all channels' peaks to dB in one SIMD call.
        // For 6 channels this is one f32x8 chunk (with 2 unused
        // lanes); for 2 channels it's a scalar-tail call. Either
        // way, the same op handles both layouts without a branch
        // on channel count.
        let mut peaks_db = [0.0_f32; CHANS];
        math::linear_to_db_block(&mut peaks_db, &peaks_lin);

        // Map clamped dB into [0, 1] for the meter widget. Linear
        // meter on a logarithmic scale is the correct visual.
        for (ch, &db_raw) in peaks_db.iter().take(nch).enumerate() {
            let db = db_raw.clamp(METER_FLOOR_DB, 0.0);
            let norm = (db - METER_FLOOR_DB) / -METER_FLOOR_DB;
            context.set_meter(METER_IDS[ch], norm);
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<SurroundMeterParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            meter(&METER_IDS, "5.1").at(0, 0).rows(2),
            knob(P::Trim, "Trim").at(0, 2),
        ])])
        .with_title("5.1 MTR")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: SurroundMeter,
    params: SurroundMeterParams,
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
                    s.set_param(P::Trim, 0.9);
                    s.wait_ms(15);
                    s.set_param(P::Trim, 0.1);
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
            .input(InputSource::Constant(0.3))
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

    /// At default trim = 0 dB, output equals input. Sanity check
    /// for the per-channel `copy_block` + `gain_block(1.0)` pair.
    #[test]
    fn unity_at_default_trim() {
        use std::time::Duration;
        use truce_test::{InputSource, driver};
        let result = driver!(Plugin)
            .duration(Duration::from_millis(50))
            .input(InputSource::Constant(0.4))
            .run();
        let max = result.output[0]
            .iter()
            .map(|s| s.abs())
            .fold(0.0_f32, f32::max);
        assert!((max - 0.4).abs() < 0.01, "expected ~0.4, got {max}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/block_surround_meter_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/block_surround_meter_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(
            Plugin,
            "screenshots/block_surround_meter_default_windows.png"
        )
        .pixel_threshold(2)
        .run();
    }
}
