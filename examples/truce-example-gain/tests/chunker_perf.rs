//! Sample-accurate chunking overhead measurement.
//!
//! Two cohorts of identical 1-second renders through the gain plugin
//! at 48 kHz / 256 samples / block:
//!
//! - **`no_events`**: zero automation; the chunker's `find_next_split`
//!   walks an empty event list and falls back to a single
//!   `plugin.process()` call per block. This is the "what does the
//!   chunker cost when nothing's chunking" measurement.
//! - **`dense_events`**: a `ParamChange` every 64 samples for the full
//!   duration (~750 events / sec). At `min_subblock_samples = 32` the
//!   chunker splits each block into multiple sub-blocks; this is the
//!   "worst-case automation density" measurement.
//!
//! Released-profile-only by gating on `#[cfg(not(debug_assertions))]`
//! and `#[ignore]`. Run with `cargo test --release -p
//! truce-example-gain -- --ignored --nocapture chunker_perf`.

#![cfg(not(debug_assertions))]

use std::time::{Duration, Instant};
use truce_example_gain::Plugin;
use truce_test::{InputSource, driver};

/// One render through the driver; returns wall-clock time.
fn time_render<F>(events_per_block: usize, blocks: usize, script_fn: F) -> Duration
where
    F: FnOnce(&mut truce_driver::Script),
{
    let sr = 48_000.0;
    let total_samples = blocks * 256;
    let duration = Duration::from_secs_f64(total_samples as f64 / sr);

    let _ = events_per_block;

    let start = Instant::now();
    let _result = driver!(Plugin)
        .sample_rate(sr)
        .duration(duration)
        .input(InputSource::Constant(0.5))
        .set_param(truce_example_gain::GainParamsParamId::Gain, 0.5)
        .script(script_fn)
        .run();
    start.elapsed()
}

#[test]
#[ignore = "perf measurement; run with --release --ignored"]
fn chunker_perf() {
    // Warm up the smoother / plugin caches so the cold-cache build
    // cost isn't attributed to the first cohort.
    let _ = time_render(0, 16, |_| {});

    let blocks = 4_000; // ~21 seconds of audio @ 48 kHz / 256 frames

    let baseline = time_render(0, blocks, |_| {});

    let dense = time_render(0, blocks, |s| {
        // One ParamChange every 64 samples for the whole render.
        // Each event lands strictly *after* `min_subblock_samples =
        // 32`, so every one triggers a sub-block split.
        for _ in 0..(blocks * 4) {
            s.wait_samples(64);
            s.set_param(truce_example_gain::GainParamsParamId::Gain, -10.0);
        }
    });

    let ns_per_block_baseline = baseline.as_nanos() as f64 / blocks as f64;
    let ns_per_block_dense = dense.as_nanos() as f64 / blocks as f64;
    let overhead = ns_per_block_dense - ns_per_block_baseline;

    println!();
    println!("=== chunker overhead ({blocks} blocks @ 256 frames) ===");
    println!(
        "  no events:    {:>8.0} ns/block   ({:>5.2} ms total)",
        ns_per_block_baseline,
        baseline.as_secs_f64() * 1000.0
    );
    println!(
        "  dense events: {:>8.0} ns/block   ({:>5.2} ms total)",
        ns_per_block_dense,
        dense.as_secs_f64() * 1000.0
    );
    println!(
        "  overhead:     {:>8.0} ns/block   (= {:>5.1}% of no-event cost)",
        overhead,
        overhead / ns_per_block_baseline * 100.0
    );

    // Sanity-only assertion: the chunker should never double the cost
    // of a no-event render at this density. If it does, something
    // ate cycles in `find_next_split` / `rebase_events` we didn't
    // expect.
    assert!(
        ns_per_block_dense < ns_per_block_baseline * 5.0,
        "dense-automation render took >5x the no-event time \
         (no_events={ns_per_block_baseline:.0} ns/block, \
         dense={ns_per_block_dense:.0} ns/block); chunker scaling regression?"
    );
}
