//! Ratio-based regression guard for the gain plugin shape.
//!
//! Doesn't assert absolute timings (shared CI runners vary 3-5x
//! in single-thread speed between runs and between fleet hardware
//! generations). Instead measures **two** implementations on the
//! same runner and asserts the ratio between them - same CPU, same
//! frequency state, same cache pressure, so the noise mostly
//! cancels.
//!
//! What we check: `gain_simd_fast` (converged-smoother fast path,
//! `is_smoothing` returns false, the typical case) must run at
//! least `MIN_RATIO` times faster than the naive per-sample
//! shape `truce-example-gain` ships. Local measurement on Apple
//! M-series is ~13.5x; the gate threshold is conservative at 5x
//! so noisy / slow CI runners don't false-fail, but a real
//! regression (smoother fast path stops short-circuiting,
//! `chunks_mut` gets a memory copy added, `gain_block` loses its
//! SIMD path) will absolutely drop below it.
//!
//! Marked `#[ignore]` so `cargo test` locally doesn't pay the
//! ~30 seconds. CI runs it explicitly via
//! `cargo test --release -p truce-simd --test gain_perf_regression -- --ignored --nocapture`.

#![allow(clippy::cast_precision_loss)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use truce_core::buffer::AudioBuffer;
use truce_params::{
    FloatParam, FloatParamReadF32, ParamFlags, ParamInfo, ParamRange, ParamUnit, ParamValueKind,
    SmoothingStyle,
};
use truce_simd::ops;

const FRAMES: usize = 512;
const CHANS: usize = 2;
const ITERS: usize = 5_000;
const SR: f64 = 48_000.0;
const MIN_RATIO: f64 = 5.0;

// Local copy of `db_to_linear` to keep this test self-contained
// (no dep on truce-core's full surface beyond AudioBuffer).
#[inline]
fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn make_param() -> FloatParam {
    let info = ParamInfo {
        id: 0,
        name: "test",
        short_name: "test",
        unit: ParamUnit::None,
        group: "",
        range: ParamRange::Linear {
            min: -60.0,
            max: 6.0,
        },
        default_plain: 0.0,
        flags: ParamFlags::AUTOMATABLE,
        kind: ParamValueKind::Float,
        midi_map: None,
        midi_channel: None,
    };
    let p = FloatParam::new(info, SmoothingStyle::Exponential(5.0));
    p.smoother.set_sample_rate(SR);
    p.set_value(3.0);
    p
}

fn make_converged_param() -> FloatParam {
    let p = make_param();
    p.smoother.snap(f64::from(p.value()));
    p
}

/// Mirrors `examples/truce-example-gain/src/lib.rs`'s process body.
fn naive_gain(inp: &[Vec<f32>], out: &mut [Vec<f32>], gain_p: &FloatParam, pan_p: &FloatParam) {
    let input_refs: Vec<&[f32]> = inp.iter().map(Vec::as_slice).collect();
    let mut output_refs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    let mut buf = AudioBuffer::from_slices_checked(&input_refs[..], &mut output_refs[..], FRAMES);
    for i in 0..buf.num_samples() {
        let gain_db = gain_p.read();
        let pan = pan_p.read();
        let gain_linear = db_to_linear(gain_db);
        let gain_l = gain_linear * (1.0 - pan.max(0.0));
        let gain_r = gain_linear * (1.0 + pan.min(0.0));
        for ch in 0..buf.channels() {
            let (inp_ch, out_ch) = buf.io(ch);
            let g = if ch == 0 { gain_l } else { gain_r };
            out_ch[i] = inp_ch[i] * g;
        }
    }
}

/// Mirrors the converged-smoother fast path of
/// `examples/truce-example-block-gain/src/lib.rs`.
fn simd_fast_gain(inp: &[Vec<f32>], out: &mut [Vec<f32>], gain_p: &FloatParam, pan_p: &FloatParam) {
    let input_refs: Vec<&[f32]> = inp.iter().map(Vec::as_slice).collect();
    let mut output_refs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    let mut buf = AudioBuffer::from_slices_checked(&input_refs[..], &mut output_refs[..], FRAMES);
    let gain_db = gain_p.value();
    let pan = pan_p.value();
    let lin = db_to_linear(gain_db);
    let gl = lin * (1.0 - pan.max(0.0));
    let gr = lin * (1.0 + pan.min(0.0));
    for ch in 0..buf.channels() {
        let g = if ch == 0 { gl } else { gr };
        let (inp_ch, out_ch) = buf.io(ch);
        ops::scale_block(out_ch, inp_ch, g);
    }
}

fn time_iters<F: FnMut()>(iters: usize, mut f: F) -> Duration {
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    start.elapsed()
}

#[test]
#[ignore = "perf-sensitive; run with --release --ignored from CI"]
fn gain_simd_fast_beats_naive() {
    let inp: Vec<Vec<f32>> = (0..CHANS)
        .map(|_| (0..FRAMES).map(|i| (i as f32) * 1e-4).collect())
        .collect();
    let mut out: Vec<Vec<f32>> = (0..CHANS).map(|_| vec![0.0; FRAMES]).collect();

    // Warmup so the first few iterations don't dominate timing
    // (icache cold, branch predictor untrained, CPU at low
    // frequency state).
    let warm_gain = make_param();
    let warm_pan = make_param();
    for _ in 0..200 {
        naive_gain(&inp, &mut out, &warm_gain, &warm_pan);
    }

    // Time the naive shape. Re-create smoothers between
    // measurements so neither run advantages the other via
    // smoother state coincidence.
    let gain_p = make_param();
    let pan_p = make_param();
    let naive = time_iters(ITERS, || {
        naive_gain(&inp, &mut out, &gain_p, &pan_p);
        black_box(&out);
    });

    let warm_gain = make_converged_param();
    let warm_pan = make_converged_param();
    for _ in 0..200 {
        simd_fast_gain(&inp, &mut out, &warm_gain, &warm_pan);
    }

    let gain_p = make_converged_param();
    let pan_p = make_converged_param();
    let fast = time_iters(ITERS, || {
        simd_fast_gain(&inp, &mut out, &gain_p, &pan_p);
        black_box(&out);
    });

    let ratio = naive.as_secs_f64() / fast.as_secs_f64();
    eprintln!(
        "gain_perf_regression: naive={:?} ({:.1} ns/iter), fast={:?} ({:.1} ns/iter), ratio={:.2}x",
        naive,
        naive.as_nanos() as f64 / ITERS as f64,
        fast,
        fast.as_nanos() as f64 / ITERS as f64,
        ratio,
    );
    assert!(
        ratio >= MIN_RATIO,
        "gain_simd_fast expected to be ≥ {MIN_RATIO}x faster than naive gain; measured {ratio:.2}x. \
         Likely a regression in is_smoothing fast-path, chunks_mut, or truce_simd::ops::gain_block."
    );
}
