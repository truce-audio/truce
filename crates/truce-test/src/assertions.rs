//! Assertion helpers built on top of [`crate::DriverResult`].
//!
//! Run a [`crate::PluginDriver`] (typically via the [`crate::driver!`]
//! macro), then pass the captured result into these helpers for
//! standard claims:
//!
//! - **Whole-run audio shape**: nonzero / silence / no-NaNs / peak
//!   below threshold.
//! - **Time-windowed audio shape**: silence-after, nonzero-after,
//!   silence-between, nonzero-between (for tail-decay /
//!   gate-between-notes assertions).
//! - **Meter readings** at end-of-run.
//! - **Output events** emitted by the plugin.
//!
//! ```ignore
//! use std::time::Duration;
//! use truce_test::{assertions, driver, InputSource};
//!
//! #[test]
//! fn long_tail_goes_silent() {
//!     let result = driver!(MyReverb)
//!         .duration(Duration::from_secs(3))
//!         .input(InputSource::Constant(0.5))
//!         .run();
//!     assertions::assert_nonzero(&result);
//!     assertions::assert_silence_after(&result, Duration::from_millis(2_500));
//! }
//! ```

use std::time::Duration;

use truce_core::cast::sample_count_usize;
use truce_core::export::PluginExport;
use truce_driver::{DriverResult, MeterReadings};

const AUDIBLE_THRESHOLD: f32 = 0.001;

fn duration_to_frames<P: PluginExport>(result: &DriverResult<P>, d: Duration) -> usize {
    let frames = sample_count_usize(d.as_secs_f64() * result.sample_rate);
    frames.min(result.total_frames)
}

fn peak_in_range<P: PluginExport>(result: &DriverResult<P>, start: usize, end: usize) -> f32 {
    if start >= end {
        return 0.0;
    }
    result
        .output
        .iter()
        .flat_map(|ch| {
            // Bound `start` against the channel too — a channel
            // shorter than `start` (mismatch between
            // `result.total_frames` and an individual channel) used to
            // panic via `ch[start..]` when start was past the end.
            let s = start.min(ch.len());
            let e = end.min(ch.len()).max(s);
            ch[s..e].iter()
        })
        .map(|s| s.abs())
        .fold(0.0f32, f32::max)
}

// ---------------------------------------------------------------------------
// Whole-run assertions
// ---------------------------------------------------------------------------

/// Assert that at least one sample anywhere in the output is above
/// the audible threshold.
///
/// # Panics
///
/// Panics if every sample is at or below `AUDIBLE_THRESHOLD` (1e-3).
//
// `usize as f64` for sample-count → seconds in the panic message;
// total_frames is bounded by test duration, well below 2^52.
#[allow(clippy::cast_precision_loss)]
pub fn assert_nonzero<P: PluginExport>(result: &DriverResult<P>) {
    let peak = peak_in_range(result, 0, result.total_frames);
    assert!(
        peak > AUDIBLE_THRESHOLD,
        "Expected non-zero output over the full {:.3} s run, but peak sample was {peak}",
        result.total_frames as f64 / result.sample_rate
    );
}

/// Assert every sample in the output is below the audible threshold.
///
/// # Panics
///
/// Panics if any sample's absolute value is at or above
/// `AUDIBLE_THRESHOLD` (1e-3).
pub fn assert_silence<P: PluginExport>(result: &DriverResult<P>) {
    let peak = peak_in_range(result, 0, result.total_frames);
    assert!(
        peak < AUDIBLE_THRESHOLD,
        "Expected silence over the full run, but peak sample was {peak}"
    );
}

/// Assert no sample is NaN or infinite. If this fails, the DSP went
/// divergent.
///
/// # Panics
///
/// Panics on the first non-finite sample, naming the channel,
/// frame index, and time offset.
//
// `usize as f64` for sample-index → milliseconds in the panic
// message; sample indices are bounded by test duration.
#[allow(clippy::cast_precision_loss)]
pub fn assert_no_nans<P: PluginExport>(result: &DriverResult<P>) {
    let bad = result
        .output
        .iter()
        .enumerate()
        .flat_map(|(ch, data)| data.iter().enumerate().map(move |(i, &s)| (ch, i, s)))
        .find(|&(_, _, s)| !s.is_finite());
    if let Some((ch, i, s)) = bad {
        panic!(
            "NaN/Inf at channel {ch} sample {i} (t = {:.3} ms): {s}",
            (i as f64 / result.sample_rate) * 1000.0
        );
    }
}

/// Assert no sample exceeds `threshold` in absolute value. Typical
/// use: `assert_peak_below(&result, 1.0)` to catch clipping.
///
/// # Panics
///
/// Panics if any sample's absolute value exceeds `threshold`.
pub fn assert_peak_below<P: PluginExport>(result: &DriverResult<P>, threshold: f32) {
    let peak = peak_in_range(result, 0, result.total_frames);
    assert!(
        peak <= threshold,
        "Peak sample {peak} exceeded threshold {threshold}"
    );
}

// ---------------------------------------------------------------------------
// Time-windowed assertions
// ---------------------------------------------------------------------------

/// Assert every sample after `t` is below the audible threshold.
/// Use for reverb / delay tail decay tests.
///
/// # Panics
///
/// Panics if any sample after `t` has absolute value at or above
/// `AUDIBLE_THRESHOLD`.
pub fn assert_silence_after<P: PluginExport>(result: &DriverResult<P>, t: Duration) {
    let start = duration_to_frames(result, t);
    let peak = peak_in_range(result, start, result.total_frames);
    assert!(
        peak < AUDIBLE_THRESHOLD,
        "Expected silence after {:.3} ms but peak was {peak} \
         (tail starts at sample {start}, run ends at sample {})",
        t.as_secs_f64() * 1000.0,
        result.total_frames
    );
}

/// Assert at least one sample after `t` is above the audible
/// threshold.
///
/// # Panics
///
/// Panics if every sample after `t` is at or below
/// `AUDIBLE_THRESHOLD`.
pub fn assert_nonzero_after<P: PluginExport>(result: &DriverResult<P>, t: Duration) {
    let start = duration_to_frames(result, t);
    let peak = peak_in_range(result, start, result.total_frames);
    assert!(
        peak > AUDIBLE_THRESHOLD,
        "Expected non-zero audio after {:.3} ms but peak was {peak}",
        t.as_secs_f64() * 1000.0
    );
}

/// Assert silence across `[start, end)`. More precise than
/// `assert_silence_after` when both endpoints matter.
///
/// # Panics
///
/// Panics if `start >= end`, or if any sample in the half-open
/// range has absolute value at or above `AUDIBLE_THRESHOLD`.
pub fn assert_silence_between<P: PluginExport>(
    result: &DriverResult<P>,
    start: Duration,
    end: Duration,
) {
    let s = duration_to_frames(result, start);
    let e = duration_to_frames(result, end);
    assert!(s < e, "assert_silence_between: start >= end");
    let peak = peak_in_range(result, s, e);
    assert!(
        peak < AUDIBLE_THRESHOLD,
        "Expected silence in [{:.3} ms, {:.3} ms) but peak was {peak}",
        start.as_secs_f64() * 1000.0,
        end.as_secs_f64() * 1000.0
    );
}

/// Assert non-zero audio somewhere in `[start, end)`.
///
/// # Panics
///
/// Panics if `start >= end`, or if every sample in the half-open
/// range is at or below `AUDIBLE_THRESHOLD`.
pub fn assert_nonzero_between<P: PluginExport>(
    result: &DriverResult<P>,
    start: Duration,
    end: Duration,
) {
    let s = duration_to_frames(result, start);
    let e = duration_to_frames(result, end);
    assert!(s < e, "assert_nonzero_between: start >= end");
    let peak = peak_in_range(result, s, e);
    assert!(
        peak > AUDIBLE_THRESHOLD,
        "Expected non-zero audio in [{:.3} ms, {:.3} ms) but peak was {peak}",
        start.as_secs_f64() * 1000.0,
        end.as_secs_f64() * 1000.0
    );
}

// ---------------------------------------------------------------------------
// Meter assertions
// ---------------------------------------------------------------------------

fn final_meters<P: PluginExport>(result: &DriverResult<P>) -> &[(u32, f32)] {
    match &result.meters {
        MeterReadings::Final(v) => v.as_slice(),
        MeterReadings::PerBlock(blocks) => blocks.last().map_or(&[], std::vec::Vec::as_slice),
        MeterReadings::None => panic!(
            "meter assertion called but CaptureSpec::meters was MeterCapture::None — \
             call .capture_meters(MeterCapture::Final) on the driver"
        ),
    }
}

/// Assert the meter identified by `id` read above `threshold` at
/// the end of the run.
///
/// # Panics
///
/// Panics if no meter with `id` is in the result, the meter's
/// final value is at or below `threshold`, or
/// `CaptureSpec::meters` was `MeterCapture::None` (call
/// `.capture_meters(MeterCapture::Final)` on the driver).
pub fn assert_meter_above<P: PluginExport>(result: &DriverResult<P>, id: u32, threshold: f32) {
    let meters = final_meters(result);
    match meters.iter().find(|(mid, _)| *mid == id) {
        Some((_, value)) => assert!(
            *value > threshold,
            "Meter {id} read {value} at end-of-run, expected > {threshold}"
        ),
        None => panic!(
            "Meter id {id} not found in DriverResult. Available ids: {:?}",
            meters.iter().map(|(i, _)| i).collect::<Vec<_>>()
        ),
    }
}

/// Assert the meter identified by `id` read below `threshold` at
/// the end of the run.
///
/// # Panics
///
/// Panics if no meter with `id` is in the result, the meter's
/// final value is at or above `threshold`, or
/// `CaptureSpec::meters` was `MeterCapture::None`.
pub fn assert_meter_below<P: PluginExport>(result: &DriverResult<P>, id: u32, threshold: f32) {
    let meters = final_meters(result);
    match meters.iter().find(|(mid, _)| *mid == id) {
        Some((_, value)) => assert!(
            *value < threshold,
            "Meter {id} read {value} at end-of-run, expected < {threshold}"
        ),
        None => panic!(
            "Meter id {id} not found in DriverResult. Available ids: {:?}",
            meters.iter().map(|(i, _)| i).collect::<Vec<_>>()
        ),
    }
}

// ---------------------------------------------------------------------------
// Output-event assertions
// ---------------------------------------------------------------------------

/// Assert exactly `n` output events were emitted by the plugin
/// across the run. Requires `.capture_output_events(true)` on the
/// driver.
///
/// # Panics
///
/// Panics if `result.output_events.len() != n`.
pub fn assert_output_event_count<P: PluginExport>(result: &DriverResult<P>, n: usize) {
    assert_eq!(
        result.output_events.len(),
        n,
        "Expected {n} output events, got {} ({:?})",
        result.output_events.len(),
        result
            .output_events
            .iter()
            .map(|e| (e.sample_offset, std::mem::discriminant(&e.body)))
            .collect::<Vec<_>>()
    );
}
