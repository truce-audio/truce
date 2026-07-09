//! 3-band parametric EQ using the upstream
//! [`biquad`](https://crates.io/crates/biquad) crate. Each band
//! has frequency, gain, and Q controls.
//!
//! Each filter is `DirectForm2Transposed::<f64, StereoSample>`,
//! where `StereoSample` is a local newtype over `wide::f64x2`
//! (the glue lives in this crate's `src/biquad.rs`). The biquad
//! crate's generic-T trait split lets us swap a scalar f64
//! sample for a packed f64x2 without forking the crate; the
//! filter inner loop then advances both channels through one
//! SIMD register pair, which on Apple Silicon NEON / x86 AVX2
//! is roughly half the cycles of two scalar biquads.

mod biquad;

use ::biquad::{Biquad, DirectForm2Transposed};
use biquad::{StereoSample, high_shelf, low_shelf, peaking};
use truce::prelude64::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, section, widgets};

type StereoBiquad = DirectForm2Transposed<f64, StereoSample>;

// --- Parameters ---

use EqParamsParamId as P;
use std::sync::Arc;

// The three bands share a shape but not their tuning: each wants its
// own frequency range and default, so they can't be one reused type
// (a Params struct's `default` / `range` are fixed on the type). They
// are three distinct `#[derive(Params)]` structs - each keeps its own
// range, default, and labels - composed in `EqParams` with `#[nested]`.
#[derive(Params)]
pub struct LowBand {
    #[param(
        name = "Low Freq",
        short_name = "LFreq",
        group = "Low",
        range = "log(20, 1000)",
        default = 200.0,
        unit = "Hz",
        smooth = "exp(10)"
    )]
    pub freq: FloatParam,
    #[param(
        name = "Low Gain",
        short_name = "LGain",
        group = "Low",
        range = "linear(-18, 18)",
        unit = "dB",
        smooth = "exp(10)"
    )]
    pub gain: FloatParam,
    #[param(
        name = "Low Q",
        short_name = "LQ",
        group = "Low",
        range = "log(0.1, 10)",
        default = 0.707,
        smooth = "exp(10)"
    )]
    pub q: FloatParam,
}

#[derive(Params)]
pub struct MidBand {
    #[param(
        name = "Mid Freq",
        short_name = "MFreq",
        group = "Mid",
        range = "log(200, 8000)",
        default = 1000.0,
        unit = "Hz",
        smooth = "exp(10)"
    )]
    pub freq: FloatParam,
    #[param(
        name = "Mid Gain",
        short_name = "MGain",
        group = "Mid",
        range = "linear(-18, 18)",
        unit = "dB",
        smooth = "exp(10)"
    )]
    pub gain: FloatParam,
    #[param(
        name = "Mid Q",
        short_name = "MQ",
        group = "Mid",
        range = "log(0.1, 10)",
        default = 0.707,
        smooth = "exp(10)"
    )]
    pub q: FloatParam,
}

#[derive(Params)]
pub struct HighBand {
    #[param(
        name = "High Freq",
        short_name = "HFreq",
        group = "High",
        range = "log(1000, 20000)",
        default = 5000.0,
        unit = "Hz",
        smooth = "exp(10)"
    )]
    pub freq: FloatParam,
    #[param(
        name = "High Gain",
        short_name = "HGain",
        group = "High",
        range = "linear(-18, 18)",
        unit = "dB",
        smooth = "exp(10)"
    )]
    pub gain: FloatParam,
    #[param(
        name = "High Q",
        short_name = "HQ",
        group = "High",
        range = "log(0.1, 10)",
        default = 0.707,
        smooth = "exp(10)"
    )]
    pub q: FloatParam,
}

// Bases are optional - bare `#[nested]` auto-packs each group after the
// previous one. They're pinned here (0 / 3 / 6, with output at 9) to fix
// the flattened ids: a pinned id survives reordering the fields or adding
// a param to an earlier group, which is the stability a shipped plugin's
// saved state and host automation depend on.
#[derive(Params)]
pub struct EqParams {
    #[nested(base = 0)]
    pub low: LowBand,
    #[nested(base = 3)]
    pub mid: MidBand,
    #[nested(base = 6)]
    pub high: HighBand,

    #[param(
        id = 9,
        name = "Output",
        short_name = "Out",
        range = "linear(-18, 18)",
        unit = "dB",
        smooth = "exp(5)"
    )]
    pub output: FloatParam,
}

// --- Plugin ---

const NUM_BANDS: usize = 3;

/// Upper bound on the audio block size we'll service. Each of the
/// 10 smoothed shape params gets a `[f64; MAX_BLOCK]` scratch
/// (~40 KB total) so the per-sample inner loop indexes into stack
/// arrays instead of calling 10 atomic loads per sample.
const MAX_BLOCK: usize = 512;

/// Stateless descriptor - DSP state lives in [`EqDspState`].
pub struct Eq;

/// Per-instance DSP state: one stereo filter per band plus the
/// sample rate cached at `reset()`.
pub struct EqDspState {
    /// One stereo filter per band; L and R advance through the
    /// same coefficients in parallel `f64x2` lanes. Mono input
    /// feeds both lanes and the L lane is the sole output.
    bands: [StereoBiquad; NUM_BANDS],
    sample_rate: f64,
}

/// Identity coefficients for a fresh biquad (passes input through
/// unchanged). `reset()` writes the real coefficients before the
/// first audio block.
fn identity_coeffs() -> ::biquad::Coefficients<f64> {
    ::biquad::Coefficients {
        a1: 0.0,
        a2: 0.0,
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
    }
}

impl PluginLogic for Eq {
    type Params = EqParams;
    type DspState = EqDspState;

    fn init(_params: &EqParams) -> EqDspState {
        let id = identity_coeffs();
        EqDspState {
            bands: [
                StereoBiquad::new(id),
                StereoBiquad::new(id),
                StereoBiquad::new(id),
            ],
            sample_rate: 44100.0,
        }
    }

    // A hypothetical pre-truce build of this EQ saved its sessions as
    // `EQS1` + ten little-endian f64s (low/mid/high x freq/gain/q,
    // then output) - translating that here keeps users' old sessions
    // and presets loading after a port to truce. The next save writes
    // a normal truce envelope, so this runs once per old session.
    // Runs on the host thread, so constructing a params instance to
    // resolve the flattened ids is fine.
    fn migrate_state(foreign: &ForeignState) -> Option<MigratedState> {
        let ForeignState::Raw { bytes, .. } = foreign else {
            return None;
        };
        let payload = bytes.strip_prefix(b"EQS1")?;
        if payload.len() != 10 * 8 {
            return None;
        }
        let mut values = payload
            .chunks_exact(8)
            .map(|chunk| f64::from_le_bytes(chunk.try_into().expect("chunks_exact(8)")));
        let mut next = || values.next().expect("length checked above");
        let p = EqParams::new();
        let params = vec![
            (p.low.freq.id(), next()),
            (p.low.gain.id(), next()),
            (p.low.q.id(), next()),
            (p.mid.freq.id(), next()),
            (p.mid.gain.id(), next()),
            (p.mid.q.id(), next()),
            (p.high.freq.id(), next()),
            (p.high.gain.id(), next()),
            (p.high.q.id(), next()),
            (P::Output.into(), next()),
        ];
        Some(MigratedState {
            params,
            extra: None,
        })
    }

    fn reset(state: &mut EqDspState, _params: &EqParams, config: &AudioConfig) {
        let sample_rate = config.sample_rate;
        state.sample_rate = sample_rate;
        for band in &mut state.bands {
            band.reset_state();
        }
    }

    fn process(
        state: &mut EqDspState,
        params: &EqParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        let sr = state.sample_rate;
        let num_ch = buffer.channels();
        let stereo = num_ch >= 2;
        let total = buffer.num_samples();

        // Walk the buffer in `MAX_BLOCK`-sized chunks. Hosts can hand
        // us blocks larger than `MAX_BLOCK` (1024 / 2048 are common
        // DAW buffer sizes), so a single fill wouldn't cover the
        // whole buffer. Scratch arrays hoist out of the loop so we
        // pay one stack-allocation rather than ten per iteration.
        let mut low_freq = [0.0_f64; MAX_BLOCK];
        let mut low_gain = [0.0_f64; MAX_BLOCK];
        let mut low_q = [0.0_f64; MAX_BLOCK];
        let mut mid_freq = [0.0_f64; MAX_BLOCK];
        let mut mid_gain = [0.0_f64; MAX_BLOCK];
        let mut mid_q = [0.0_f64; MAX_BLOCK];
        let mut high_freq = [0.0_f64; MAX_BLOCK];
        let mut high_gain = [0.0_f64; MAX_BLOCK];
        let mut high_q = [0.0_f64; MAX_BLOCK];
        let mut output = [0.0_f64; MAX_BLOCK];
        let mut out_lin = [0.0_f64; MAX_BLOCK];

        let mut offset = 0;
        while offset < total {
            let n = (total - offset).min(MAX_BLOCK);

            // Slice-based read advances each smoother by exactly `n`
            // - one atomic-pair per param (10 pairs total) regardless
            // of chunk length, vs one pair per param per sample if we
            // called `.read()` in the inner loop. Coefficient updates
            // still happen per sample so a fast knob sweep stays
            // click-free; only the smoother traffic moves out of the
            // inner loop.
            params.low.freq.read_into(&mut low_freq[..n]);
            params.low.gain.read_into(&mut low_gain[..n]);
            params.low.q.read_into(&mut low_q[..n]);
            params.mid.freq.read_into(&mut mid_freq[..n]);
            params.mid.gain.read_into(&mut mid_gain[..n]);
            params.mid.q.read_into(&mut mid_q[..n]);
            params.high.freq.read_into(&mut high_freq[..n]);
            params.high.gain.read_into(&mut high_gain[..n]);
            params.high.q.read_into(&mut high_q[..n]);
            params.output.read_into(&mut output[..n]);

            // Vectorize the output dB → linear conversion once per
            // chunk. f64::powf is opaque to LLVM's autovectorizer;
            // truce_simd::math64::db_to_linear_block routes through
            // wide::f64x4's native `exp`, so the per-sample inner
            // loop just reads a precomputed linear gain.
            truce_simd::math64::db_to_linear_block(&mut out_lin[..n], &output[..n]);

            for i in 0..n {
                let idx = offset + i;

                // Per-sample coefficient updates because every shape
                // param is smoothed; one coefficient set serves both
                // channels.
                state.bands[0].update_coefficients(low_shelf(
                    low_freq[i],
                    low_gain[i],
                    low_q[i],
                    sr,
                ));
                state.bands[1].update_coefficients(peaking(mid_freq[i], mid_gain[i], mid_q[i], sr));
                state.bands[2].update_coefficients(high_shelf(
                    high_freq[i],
                    high_gain[i],
                    high_q[i],
                    sr,
                ));

                // Read both channels (or duplicate the mono channel
                // into the second lane) before advancing any filter,
                // so the L and R lanes step in lockstep.
                let in_l = buffer.io(0).0[idx];
                let in_r = if stereo { buffer.io(1).0[idx] } else { in_l };

                let mut sample = StereoSample::from_lr(in_l, in_r);
                for band in &mut state.bands {
                    sample = band.run(sample);
                }
                let (l, r) = sample.to_lr();
                let out_gain = out_lin[i];
                buffer.io(0).1[idx] = l * out_gain;
                if stereo {
                    buffer.io(1).1[idx] = r * out_gain;
                }
            }

            offset += n;
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<EqParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![
            // Reused-group params are addressed by their flattened id
            // (`base + local`), read off each band so the three `low` /
            // `mid` / `high` instances resolve to distinct controls.
            section(
                "LOW",
                vec![
                    knob(params.low.freq.id(), "Freq"),
                    knob(params.low.gain.id(), "Gain"),
                    knob(params.low.q.id(), "Q"),
                ],
            ),
            section(
                "MID",
                vec![
                    knob(params.mid.freq.id(), "Freq"),
                    knob(params.mid.gain.id(), "Gain"),
                    knob(params.mid.q.id(), "Q"),
                ],
            ),
            section(
                "HIGH",
                vec![
                    knob(params.high.freq.id(), "Freq"),
                    knob(params.high.gain.id(), "Gain"),
                    knob(params.high.q.id(), "Q"),
                ],
            ),
            widgets(vec![knob(P::Output, "Output")]),
        ])
        .with_title("EQ")
        .resizable(true)
        // Cell-count bounds on (cols, rows) - the grid snaps to whole
        // cells, so bounds are in cells, not pixels. Floor of 3 cols
        // keeps each section's three knobs on one row (dropping below
        // would wrap them); 9 cols lets the three sections widen out
        // side by side. 3..8 rows caps the vertical stretch where the
        // layout still reads as tight.
        .min_size((3, 3))
        .max_size((9, 8))
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: Eq,
    params: EqParams,
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
                    // The nested band params aren't in `EqParamsParamId`;
                    // resolve the first band's freq id off a params
                    // instance to sweep it.
                    let p = EqParams::new();
                    s.set_param(p.low.freq.id(), 0.9);
                    s.wait_ms(15);
                    s.set_param(p.low.freq.id(), 0.1);
                    s.wait_ms(15);
                })
                .run()
        });
    }

    /// A legacy `EQS1` blob (the pre-truce format `migrate_state`
    /// translates) with recognizable per-band values.
    fn legacy_blob() -> Vec<u8> {
        let values = [
            100.0, -3.0, 1.0, // low: freq / gain / q
            1000.0, 2.0, 0.5, // mid
            8000.0, 4.5, 2.0,  // high
            -1.5, // output
        ];
        let mut blob = b"EQS1".to_vec();
        for v in values {
            blob.extend_from_slice(&f64::to_le_bytes(v));
        }
        blob
    }

    /// The LV2 wrapper's per-block glue (control-port change detection,
    /// atom decode, `process`, meter copy-out, MIDI + notify atom writes)
    /// is allocation-free on the audio thread. Drives the real `run`
    /// callback under the checker, not just the plugin. Compiled only
    /// with `lv2` on, which `rt-paranoid` pulls in.
    #[cfg(feature = "lv2")]
    #[test]
    fn lv2_wrapper_glue_is_allocation_free() {
        assert_eq!(
            truce_lv2::rt_paranoid_smoke::<Plugin>(),
            0,
            "the LV2 wrapper's per-block glue must not allocate on the audio thread"
        );
    }

    #[test]
    fn legacy_state_migrates() {
        let migrated =
            truce_test::assert_state_migration::<Plugin>(PluginFormat::Clap, None, &legacy_blob());
        assert_eq!(migrated.params.len(), 10);
        let p = EqParams::new();
        assert_eq!(migrated.params[3], (p.mid.freq.id(), 1000.0));
        assert_eq!(migrated.params[9], (P::Output.into(), -1.5));
        assert!(migrated.extra.is_none());
    }

    #[test]
    fn truncated_legacy_state_is_refused() {
        truce_test::assert_state_migration_rejected::<Plugin>(
            PluginFormat::Clap,
            None,
            &legacy_blob()[..40],
        );
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
    fn flat_eq_passes_audio() {
        use std::time::Duration;
        use truce_test::{InputSource, driver};
        // Default EQ (0dB gain on all bands) should pass audio ~unchanged
        let result = driver!(Plugin)
            .duration(Duration::from_millis(12))
            .input(InputSource::Constant(0.5))
            .run();
        let max = result.output[0]
            .iter()
            .map(|s| s.abs())
            .fold(0.0f32, f32::max);
        assert!(max > 0.4, "Flat EQ should pass audio near unity, got {max}");
    }

    #[test]
    fn large_block_fills_whole_output() {
        use std::time::Duration;
        use truce_test::{InputSource, driver};
        // Hosts can hand us blocks larger than the plugin's internal
        // `MAX_BLOCK`; `process` must walk the whole buffer. A 2048
        // block (common DAW setting) used to leave samples 512..2048
        // pass-through stale, producing periodic noise.
        let result = driver!(Plugin)
            .block_size(2048)
            .duration(Duration::from_millis(100))
            .input(InputSource::Constant(0.5))
            .run();
        let min = result.output[0]
            .iter()
            .map(|s| s.abs())
            .fold(f32::INFINITY, f32::min);
        assert!(
            min > 0.4,
            "Every sample of a flat-EQ pass-through should be near unity; \
             min was {min} (tail of large block was dropped on the floor)"
        );
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    #[test]
    fn state_round_trips() {
        truce_test::assert_state_round_trip::<Plugin>();
    }

    // --- AU metadata ---

    #[test]
    fn au_type_codes_ascii() {
        truce_test::assert_au_type_codes_ascii::<Plugin>();
    }

    #[test]
    fn fourcc_roundtrip() {
        truce_test::assert_fourcc_roundtrip::<Plugin>();
    }

    #[test]
    fn bus_config_effect() {
        truce_test::assert_bus_config_effect::<Plugin>();
    }

    // --- GUI lifecycle ---

    #[test]
    fn editor_lifecycle() {
        truce_test::assert_editor_lifecycle::<Plugin>();
    }

    #[test]
    fn editor_size_consistent() {
        truce_test::assert_editor_size_consistent::<Plugin>();
    }

    // --- Parameters ---

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
    fn param_count_matches() {
        truce_test::assert_param_count_matches::<Plugin>();
    }

    #[test]
    fn no_duplicate_param_ids() {
        truce_test::assert_no_duplicate_param_ids::<Plugin>();
    }

    // --- State resilience ---

    #[test]
    fn corrupt_state_no_crash() {
        truce_test::assert_corrupt_state_no_crash::<Plugin>();
    }

    #[test]
    fn empty_state_no_crash() {
        truce_test::assert_empty_state_no_crash::<Plugin>();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/eq_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/eq_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/eq_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
