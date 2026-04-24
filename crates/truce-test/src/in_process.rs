//! Integration-test helpers on top of
//! [`truce_standalone::in_process`].
//!
//! The standalone crate provides the runner that instantiates a
//! plugin, feeds it scripted MIDI, and captures output audio +
//! meter readings into a [`RunResult`]. This module adds the
//! assertion style test authors already know from the sibling
//! `assert_nonzero` / `assert_silence` helpers — extended to cover
//! time-windowed claims ("silent after 500 ms", "audio during the
//! first block"), meter slots, and clipping guards.
//!
//! Gated behind the `in-process` feature so tests that only touch
//! render / state / params / GUI don't inherit the cpal / midir
//! transitive dependency chain.
//!
//! ```ignore
//! use std::time::Duration;
//! use truce_test::in_process::{assert_nonzero, assert_silence_after, run, InProcessOpts};
//!
//! #[test]
//! fn long_tail_goes_silent() {
//!     let result = run::<Plugin>(
//!         InProcessOpts::default()
//!             .sample_rate(48_000.0)
//!             .midi(|m| { m.note_on(60, 0.8); m.wait_ms(100); m.note_off(60); })
//!             .duration(Duration::from_secs(3)),
//!     );
//!
//!     assert_nonzero(&result);
//!     assert_silence_after(&result, Duration::from_millis(2_500));
//! }
//! ```

use std::time::Duration;

pub use truce_standalone::in_process::{
    run, InProcessOpts, MidiScript, RunResult,
};

const AUDIBLE_THRESHOLD: f32 = 0.001;

/// Convert a `Duration` into the matching sample offset for the
/// run's sample rate. Saturates at the run's length.
fn duration_to_frames(result: &RunResult, d: Duration) -> usize {
    let frames = (d.as_secs_f64() * result.sample_rate) as usize;
    frames.min(result.total_frames)
}

/// Peak absolute sample across every channel in `[start, end)`.
/// Returns `0.0` for empty ranges.
fn peak_in_range(result: &RunResult, start: usize, end: usize) -> f32 {
    if start >= end {
        return 0.0;
    }
    result
        .output
        .iter()
        .flat_map(|ch| ch[start..end.min(ch.len())].iter())
        .map(|s| s.abs())
        .fold(0.0f32, f32::max)
}

// ---------------------------------------------------------------------------
// Whole-run assertions
// ---------------------------------------------------------------------------

/// Assert that at least one sample in the output is above the
/// audible threshold somewhere in the run.
pub fn assert_nonzero(result: &RunResult) {
    let peak = peak_in_range(result, 0, result.total_frames);
    assert!(
        peak > AUDIBLE_THRESHOLD,
        "Expected non-zero output over the full {:.3} s run, \
         but peak sample was {peak}",
        result.total_frames as f64 / result.sample_rate
    );
}

/// Assert that every sample in the output is below the audible
/// threshold (plugin produced silence).
pub fn assert_silence(result: &RunResult) {
    let peak = peak_in_range(result, 0, result.total_frames);
    assert!(
        peak < AUDIBLE_THRESHOLD,
        "Expected silence over the full run, but peak sample was {peak}"
    );
}

/// Assert that no sample is NaN or infinite. Runs anywhere in the
/// buffer — if this fails, the DSP went divergent.
pub fn assert_no_nans(result: &RunResult) {
    for (ch, data) in result.output.iter().enumerate() {
        for (i, &s) in data.iter().enumerate() {
            assert!(
                s.is_finite(),
                "NaN/Inf at channel {ch} sample {i} (t = {:.3} ms): {s}",
                (i as f64 / result.sample_rate) * 1000.0
            );
        }
    }
}

/// Assert that no sample exceeds `threshold` in absolute value.
/// Typical use: `assert_peak_below(&result, 1.0)` to catch clipping.
pub fn assert_peak_below(result: &RunResult, threshold: f32) {
    let peak = peak_in_range(result, 0, result.total_frames);
    assert!(
        peak <= threshold,
        "Peak sample {peak} exceeded threshold {threshold}"
    );
}

// ---------------------------------------------------------------------------
// Time-windowed assertions
// ---------------------------------------------------------------------------

/// Assert that every sample after `t` is below the audible
/// threshold. Use for reverb / delay tail decay tests.
pub fn assert_silence_after(result: &RunResult, t: Duration) {
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

/// Assert that at least one sample after `t` is above the audible
/// threshold. Use for "release phase still audible" or "plugin is
/// still generating output late in the run" claims.
pub fn assert_nonzero_after(result: &RunResult, t: Duration) {
    let start = duration_to_frames(result, t);
    let peak = peak_in_range(result, start, result.total_frames);
    assert!(
        peak > AUDIBLE_THRESHOLD,
        "Expected non-zero audio after {:.3} ms but peak was {peak}",
        t.as_secs_f64() * 1000.0
    );
}

/// Assert silence across `[start, end)`. More precise than
/// `assert_silence_after` when both endpoints matter (e.g. testing
/// that a note gate shuts output off between notes).
pub fn assert_silence_between(result: &RunResult, start: Duration, end: Duration) {
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
pub fn assert_nonzero_between(result: &RunResult, start: Duration, end: Duration) {
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

/// Assert that the meter identified by `id` read above `threshold`
/// at the end of the run. 0.0 is the floor; 1.0 is full-scale.
pub fn assert_meter_above(result: &RunResult, id: u32, threshold: f32) {
    match result.meters.iter().find(|(mid, _)| *mid == id) {
        Some((_, value)) => assert!(
            *value > threshold,
            "Meter {id} read {value} at end-of-run, expected > {threshold}"
        ),
        None => panic!(
            "Meter id {id} not found in RunResult. Available ids: {:?}",
            result.meters.iter().map(|(i, _)| i).collect::<Vec<_>>()
        ),
    }
}

/// Assert that the meter identified by `id` read below `threshold`
/// at the end of the run.
pub fn assert_meter_below(result: &RunResult, id: u32, threshold: f32) {
    match result.meters.iter().find(|(mid, _)| *mid == id) {
        Some((_, value)) => assert!(
            *value < threshold,
            "Meter {id} read {value} at end-of-run, expected < {threshold}"
        ),
        None => panic!(
            "Meter id {id} not found in RunResult. Available ids: {:?}",
            result.meters.iter().map(|(i, _)| i).collect::<Vec<_>>()
        ),
    }
}
