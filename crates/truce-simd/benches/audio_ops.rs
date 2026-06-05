//! Pre-vectorization vs. post-vectorization benchmarks for the
//! truce SIMD-friendly DSP work.
//!
//! Three benchmark groups:
//!
//! 1. **`smoother_traffic`** - measures the atomic-load/store cost of
//!    reading a smoothed parameter the old way (per-sample
//!    [`FloatParam::read`]) vs. the new way
//!    ([`FloatParam::read_into`]) across N ∈ {8, 16, 32, 64, 128}.
//!    Doubles as the empirical input that picks the default `N` in
//!    plugin examples.
//!
//! 2. **`gain_inner_loop`** - end-to-end "single audio block of a gain
//!    plugin". Pre-vec walks samples in the outer loop with
//!    per-sample atomics and a per-channel scalar multiply (the
//!    shape today's examples ship). Post-vec uses
//!    [`AudioBuffer::chunks_mut`] + [`FloatParam::read_into`] +
//!    [`truce_simd::ops::mul_block`].
//!
//! 3. **`simd_ops_scalar_vs_wide`** - apples-to-apples for the
//!    `truce_simd` ops themselves. Compares the `*_scalar`
//!    variants (single-instruction scalar) to the `wide` SIMD
//!    variants on identical input. Shows the lower bound on what
//!    the `gain_inner_loop` comparison can recover.
//!
//! Run with `cargo bench -p truce-simd` (default features keep the
//! `wide` backend on). To compare against a no-SIMD build:
//! `cargo bench -p truce-simd --no-default-features` rebuilds the
//! `_block` variants as scalar loops; the bench then measures the
//! scalar inner loop alone.

#![allow(clippy::cast_precision_loss)]

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};

use truce_core::buffer::{AudioBuffer, ChunkItem};
use truce_params::{
    FloatParam, FloatParamReadF32, ParamFlags, ParamInfo, ParamRange, ParamUnit, ParamValueKind,
    SmoothingStyle,
};
use truce_simd::{math, ops};

// ---------------------------------------------------------------------------
// Helpers shared across groups.
// ---------------------------------------------------------------------------

const SR: f64 = 48_000.0;

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
    };
    let p = FloatParam::new(info, SmoothingStyle::Exponential(5.0));
    p.smoother.set_sample_rate(SR);
    // Walk the smoother off the default so reads aren't all
    // returning the snap value.
    p.set_value(3.0);
    p
}

// ---------------------------------------------------------------------------
// Group 1: smoother traffic (per-sample vs per-block).
// ---------------------------------------------------------------------------

fn bench_smoother_traffic(c: &mut Criterion) {
    let mut g = c.benchmark_group("smoother_traffic");

    // Per-sample baseline: how the existing examples drain a
    // smoother in their outer loop (one atomic load + one atomic
    // store per sample).
    for &block in &[8_usize, 16, 32, 64, 128] {
        g.throughput(Throughput::Elements(block as u64));
        g.bench_with_input(
            BenchmarkId::new("per_sample_read", block),
            &block,
            |b, &n| {
                let p = make_param();
                b.iter(|| {
                    let mut acc = 0.0_f32;
                    for _ in 0..n {
                        acc += p.read();
                    }
                    black_box(acc);
                });
            },
        );
    }

    // Per-block path (one atomic load + one atomic store regardless
    // of N). `read_into` is the runtime-length form on the same
    // underlying primitive as the now-deprecated `read_block::<N>`;
    // numbers transfer 1:1.
    g.throughput(Throughput::Elements(8));
    g.bench_function(BenchmarkId::new("read_into", 8), |b| {
        let p = make_param();
        let mut buf = [0.0_f32; 8];
        b.iter(|| {
            p.read_into(&mut buf);
            black_box(&buf);
        });
    });
    g.throughput(Throughput::Elements(16));
    g.bench_function(BenchmarkId::new("read_into", 16), |b| {
        let p = make_param();
        let mut buf = [0.0_f32; 16];
        b.iter(|| {
            p.read_into(&mut buf);
            black_box(&buf);
        });
    });
    g.throughput(Throughput::Elements(32));
    g.bench_function(BenchmarkId::new("read_into", 32), |b| {
        let p = make_param();
        let mut buf = [0.0_f32; 32];
        b.iter(|| {
            p.read_into(&mut buf);
            black_box(&buf);
        });
    });
    g.throughput(Throughput::Elements(64));
    g.bench_function(BenchmarkId::new("read_into", 64), |b| {
        let p = make_param();
        let mut buf = [0.0_f32; 64];
        b.iter(|| {
            p.read_into(&mut buf);
            black_box(&buf);
        });
    });
    g.throughput(Throughput::Elements(128));
    g.bench_function(BenchmarkId::new("read_into", 128), |b| {
        let p = make_param();
        let mut buf = [0.0_f32; 128];
        b.iter(|| {
            p.read_into(&mut buf);
            black_box(&buf);
        });
    });

    g.finish();
}

// ---------------------------------------------------------------------------
// Group 2: gain inner loop (pre-vec vs post-vec).
// ---------------------------------------------------------------------------

const FRAMES: usize = 512;
const CHANS: usize = 2;

/// Plain `10 ^ (db / 20)`. Vectorized variants live behind feature
/// gates and are out of scope for this bench; what we're measuring
/// is whether the surrounding code lets the multiply itself
/// vectorize.
#[inline]
fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn pre_vec_gain(inp: &[Vec<f32>], out: &mut [Vec<f32>], gain_p: &FloatParam, pan_p: &FloatParam) {
    let input_refs: Vec<&[f32]> = inp.iter().map(Vec::as_slice).collect();
    let mut output_refs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    let mut buf = AudioBuffer::from_slices_checked(&input_refs[..], &mut output_refs[..], FRAMES);
    // Imitate today's example: outer-sample / inner-channel,
    // per-sample atomic reads, scalar multiply.
    for i in 0..buf.num_samples() {
        let gain_db = gain_p.read();
        let pan = pan_p.read();
        let lin = db_to_linear(gain_db);
        let gl = lin * (1.0 - pan.max(0.0));
        let gr = lin * (1.0 + pan.min(0.0));
        for ch in 0..buf.channels() {
            let (inp, out) = buf.io(ch);
            let g = if ch == 0 { gl } else { gr };
            out[i] = inp[i] * g;
        }
    }
}

const POST_VEC_N: usize = 32;

fn post_vec_gain(inp: &[Vec<f32>], out: &mut [Vec<f32>], gain_p: &FloatParam, pan_p: &FloatParam) {
    let input_refs: Vec<&[f32]> = inp.iter().map(Vec::as_slice).collect();
    let mut output_refs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    let mut buf = AudioBuffer::from_slices_checked(&input_refs[..], &mut output_refs[..], FRAMES);

    // Read smoothers ONCE per audio block (not per chunk per
    // channel). For a 512-frame block this collapses 1024
    // per-sample atomic round-trips (pre_vec) into 2 atomic pairs
    // total. The per-block envelope arrays live on the stack -
    // 4 KB for FRAMES=512, fine.
    let mut gain_db = [0.0_f32; FRAMES];
    let mut pan = [0.0_f32; FRAMES];
    gain_p.read_into(&mut gain_db);
    pan_p.read_into(&mut pan);

    // Pre-compute per-channel gain envelopes once. db_to_linear is
    // a transcendental and stays scalar; the per-channel split is
    // hoisted out of the chunk loop so the multiply itself has no
    // branches.
    let mut g_l = [0.0_f32; FRAMES];
    let mut g_r = [0.0_f32; FRAMES];
    for i in 0..FRAMES {
        let lin = db_to_linear(gain_db[i]);
        g_l[i] = lin * (1.0 - pan[i].max(0.0));
        g_r[i] = lin * (1.0 + pan[i].min(0.0));
    }

    // Now the multiply: every chunk picks its N-sample slice of
    // the precomputed envelope via `sample`, no branches, hands
    // it to truce-simd's SIMD multiply.
    let mut chunks = buf.chunks_mut::<POST_VEC_N>();
    while let Some(chunk) = chunks.next() {
        match chunk {
            ChunkItem::Full {
                ch,
                sample,
                inp,
                out,
            } => {
                let g = if ch == 0 {
                    &g_l[sample..sample + POST_VEC_N]
                } else {
                    &g_r[sample..sample + POST_VEC_N]
                };
                ops::mul_block(out, inp, g);
            }
            ChunkItem::Tail {
                ch,
                sample,
                inp,
                out,
            } => {
                let len = inp.len();
                let g = if ch == 0 {
                    &g_l[sample..sample + len]
                } else {
                    &g_r[sample..sample + len]
                };
                ops::mul_block(out, inp, g);
            }
        }
    }
}

fn bench_gain_inner_loop(c: &mut Criterion) {
    let mut g = c.benchmark_group("gain_inner_loop");
    g.throughput(Throughput::Elements((FRAMES * CHANS) as u64));

    g.bench_function("pre_vec", |b| {
        let gain_p = make_param();
        let pan_p = make_param();
        let inp: Vec<Vec<f32>> = (0..CHANS)
            .map(|_| (0..FRAMES).map(|i| (i as f32) * 1e-4).collect())
            .collect();
        let mut out: Vec<Vec<f32>> = (0..CHANS).map(|_| vec![0.0; FRAMES]).collect();
        b.iter(|| {
            pre_vec_gain(&inp, &mut out, &gain_p, &pan_p);
            black_box(&out);
        });
    });

    g.bench_function("post_vec", |b| {
        let gain_p = make_param();
        let pan_p = make_param();
        let inp: Vec<Vec<f32>> = (0..CHANS)
            .map(|_| (0..FRAMES).map(|i| (i as f32) * 1e-4).collect())
            .collect();
        let mut out: Vec<Vec<f32>> = (0..CHANS).map(|_| vec![0.0; FRAMES]).collect();
        b.iter(|| {
            post_vec_gain(&inp, &mut out, &gain_p, &pan_p);
            black_box(&out);
        });
    });

    g.finish();
}

// ---------------------------------------------------------------------------
// Group 3: truce-simd ops, scalar vs wide.
// ---------------------------------------------------------------------------

fn bench_simd_ops_scalar_vs_wide(c: &mut Criterion) {
    let mut g = c.benchmark_group("simd_ops_scalar_vs_wide");
    for &n in &[32_usize, 128, 512] {
        g.throughput(Throughput::Elements(n as u64));

        let a: Vec<f32> = (0..n).map(|i| (i as f32) * 0.001).collect();
        let b: Vec<f32> = (0..n).map(|i| (i as f32) * -0.002).collect();

        g.bench_with_input(BenchmarkId::new("gain_block_scalar", n), &n, |bn, &n| {
            let mut buf = vec![0.0_f32; n];
            bn.iter(|| {
                buf.copy_from_slice(&a[..n]);
                ops::gain_block_scalar(&mut buf, 0.75);
                black_box(&buf);
            });
        });
        g.bench_with_input(BenchmarkId::new("gain_block_wide", n), &n, |bn, &n| {
            let mut buf = vec![0.0_f32; n];
            bn.iter(|| {
                buf.copy_from_slice(&a[..n]);
                ops::gain_block(&mut buf, 0.75);
                black_box(&buf);
            });
        });

        g.bench_with_input(BenchmarkId::new("mul_block_scalar", n), &n, |bn, &n| {
            let mut out = vec![0.0_f32; n];
            bn.iter(|| {
                ops::mul_block_scalar(&mut out, &a[..n], &b[..n]);
                black_box(&out);
            });
        });
        g.bench_with_input(BenchmarkId::new("mul_block_wide", n), &n, |bn, &n| {
            let mut out = vec![0.0_f32; n];
            bn.iter(|| {
                ops::mul_block(&mut out, &a[..n], &b[..n]);
                black_box(&out);
            });
        });

        g.bench_with_input(BenchmarkId::new("mix_block_scalar", n), &n, |bn, &n| {
            let mut out = vec![0.0_f32; n];
            bn.iter(|| {
                ops::mix_block_scalar(&mut out, &a[..n], 0.5, &b[..n], 0.25);
                black_box(&out);
            });
        });
        g.bench_with_input(BenchmarkId::new("mix_block_wide", n), &n, |bn, &n| {
            let mut out = vec![0.0_f32; n];
            bn.iter(|| {
                ops::mix_block(&mut out, &a[..n], 0.5, &b[..n], 0.25);
                black_box(&out);
            });
        });
    }
    g.finish();
}

// ---------------------------------------------------------------------------
// Group 4: pure-trim gain (single smoothed scalar across a block).
//
// The gain_inner_loop bench has db_to_linear and a per-sample pan
// branch that gate vectorization; this group strips those down to
// the cleanest possible "block multiply" so the SIMD win shows up
// without being masked by transcendental + branch cost. Mirrors
// the real-world hot path inside the EQ plugin's trailing trim
// multiply (`output * sample` after the biquad cascade).
// ---------------------------------------------------------------------------

fn pre_vec_trim(inp: &[Vec<f32>], out: &mut [Vec<f32>], trim_p: &FloatParam) {
    let input_refs: Vec<&[f32]> = inp.iter().map(Vec::as_slice).collect();
    let mut output_refs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    let mut buf = AudioBuffer::from_slices_checked(&input_refs[..], &mut output_refs[..], FRAMES);
    for i in 0..buf.num_samples() {
        let g = trim_p.read();
        for ch in 0..buf.channels() {
            let (inp, out) = buf.io(ch);
            out[i] = inp[i] * g;
        }
    }
}

fn post_vec_trim(inp: &[Vec<f32>], out: &mut [Vec<f32>], trim_p: &FloatParam) {
    let input_refs: Vec<&[f32]> = inp.iter().map(Vec::as_slice).collect();
    let mut output_refs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    let mut buf = AudioBuffer::from_slices_checked(&input_refs[..], &mut output_refs[..], FRAMES);
    // One block read of the smoother. Same envelope feeds every channel.
    let mut g_env = [0.0_f32; FRAMES];
    trim_p.read_into(&mut g_env);
    let mut chunks = buf.chunks_mut::<POST_VEC_N>();
    while let Some(chunk) = chunks.next() {
        match chunk {
            ChunkItem::Full {
                sample, inp, out, ..
            } => {
                ops::mul_block(out, inp, &g_env[sample..sample + POST_VEC_N]);
            }
            ChunkItem::Tail {
                sample, inp, out, ..
            } => {
                let len = inp.len();
                ops::mul_block(out, inp, &g_env[sample..sample + len]);
            }
        }
    }
}

fn bench_trim_block(c: &mut Criterion) {
    let mut g = c.benchmark_group("trim_block");
    g.throughput(Throughput::Elements((FRAMES * CHANS) as u64));

    g.bench_function("pre_vec", |b| {
        let trim = make_param();
        let inp: Vec<Vec<f32>> = (0..CHANS)
            .map(|_| (0..FRAMES).map(|i| (i as f32) * 1e-4).collect())
            .collect();
        let mut out: Vec<Vec<f32>> = (0..CHANS).map(|_| vec![0.0; FRAMES]).collect();
        b.iter(|| {
            pre_vec_trim(&inp, &mut out, &trim);
            black_box(&out);
        });
    });

    g.bench_function("post_vec", |b| {
        let trim = make_param();
        let inp: Vec<Vec<f32>> = (0..CHANS)
            .map(|_| (0..FRAMES).map(|i| (i as f32) * 1e-4).collect())
            .collect();
        let mut out: Vec<Vec<f32>> = (0..CHANS).map(|_| vec![0.0; FRAMES]).collect();
        b.iter(|| {
            post_vec_trim(&inp, &mut out, &trim);
            black_box(&out);
        });
    });

    g.finish();
}

// ---------------------------------------------------------------------------
// Group 5: gain vs gain-simd, mirrored from the actual plugin code.
//
// Three points of comparison, one per process-body shape that ships
// in the example tree:
//
//   - `gain`              - examples/truce-example-gain (naive, per-sample
//                           atomic + scalar multiply, the "before"
//                           shape every plugin author has written).
//   - `gain_simd_fast`    - examples/truce-example-block-gain's
//                           converged-smoother fast path (constant
//                           gain across the block; scale_block per
//                           channel). The typical real-world
//                           case (user dials in -6 dB and walks
//                           away).
//   - `gain_simd_slow`    - examples/truce-example-block-gain's
//                           slow path (active smoother;
//                           math::db_to_linear_block precompute,
//                           chunks_mut + mul_block apply). The
//                           worst case for the new architecture,
//                           still the one we'd want to win.
//
// All three operate on the same input buffer shape (FRAMES × CHANS)
// and the same parameter state; the only difference is the process
// body. Criterion's `Throughput::Elements` lets you read the bench
// as Gelem/s - directly comparable across the three rows.
// ---------------------------------------------------------------------------

fn naive_gain_process(
    inp: &[Vec<f32>],
    out: &mut [Vec<f32>],
    gain_p: &FloatParam,
    pan_p: &FloatParam,
) {
    let input_refs: Vec<&[f32]> = inp.iter().map(Vec::as_slice).collect();
    let mut output_refs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    let mut buf = AudioBuffer::from_slices_checked(&input_refs[..], &mut output_refs[..], FRAMES);
    // Mirrors examples/truce-example-gain/src/lib.rs verbatim.
    for i in 0..buf.num_samples() {
        let gain_db = gain_p.read();
        let pan = pan_p.read();
        let gain_linear = db_to_linear(gain_db);
        let gain_l = gain_linear * (1.0 - pan.max(0.0));
        let gain_r = gain_linear * (1.0 + pan.min(0.0));
        for ch in 0..buf.channels() {
            let (inp, out) = buf.io(ch);
            let g = if ch == 0 { gain_l } else { gain_r };
            out[i] = inp[i] * g;
        }
    }
}

fn simd_fast_gain_process(
    inp: &[Vec<f32>],
    out: &mut [Vec<f32>],
    gain_p: &FloatParam,
    pan_p: &FloatParam,
) {
    let input_refs: Vec<&[f32]> = inp.iter().map(Vec::as_slice).collect();
    let mut output_refs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    let mut buf = AudioBuffer::from_slices_checked(&input_refs[..], &mut output_refs[..], FRAMES);
    // Mirrors the converged-smoother branch of
    // examples/truce-example-block-gain. Caller is responsible for
    // ensuring the smoothers are at target (this bench's
    // make_converged_param helper handles that).
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

fn simd_slow_gain_process(
    inp: &[Vec<f32>],
    out: &mut [Vec<f32>],
    gain_p: &FloatParam,
    pan_p: &FloatParam,
) {
    let input_refs: Vec<&[f32]> = inp.iter().map(Vec::as_slice).collect();
    let mut output_refs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    let mut buf = AudioBuffer::from_slices_checked(&input_refs[..], &mut output_refs[..], FRAMES);
    // Mirrors the active-smoother branch of
    // examples/truce-example-block-gain, including the
    // math::db_to_linear_block precompute (Phase 9).
    let n = buf.num_samples();
    let mut gain_db = [0.0_f32; FRAMES];
    let mut pan = [0.0_f32; FRAMES];
    gain_p.read_into(&mut gain_db);
    pan_p.read_into(&mut pan);

    let mut lin = [0.0_f32; FRAMES];
    math::db_to_linear_block(&mut lin[..n], &gain_db[..n]);

    let mut g_l = [0.0_f32; FRAMES];
    let mut g_r = [0.0_f32; FRAMES];
    for i in 0..n {
        g_l[i] = lin[i] * (1.0 - pan[i].max(0.0));
        g_r[i] = lin[i] * (1.0 + pan[i].min(0.0));
    }

    let mut chunks = buf.chunks_mut::<POST_VEC_N>();
    while let Some(chunk) = chunks.next() {
        match chunk {
            ChunkItem::Full {
                ch,
                sample,
                inp,
                out,
            } => {
                let g = if ch == 0 {
                    &g_l[sample..sample + POST_VEC_N]
                } else {
                    &g_r[sample..sample + POST_VEC_N]
                };
                ops::mul_block(out, inp, g);
            }
            ChunkItem::Tail {
                ch,
                sample,
                inp,
                out,
            } => {
                let len = inp.len();
                let g = if ch == 0 {
                    &g_l[sample..sample + len]
                } else {
                    &g_r[sample..sample + len]
                };
                ops::mul_block(out, inp, g);
            }
        }
    }
}

/// Build a converged param: same setup as `make_param` but with a
/// `snap()` after `set_value` so `is_smoothing()` returns false.
/// Required for the fast-path bench - the fast path's contract is
/// "smoother at target", and we want the bench to actually exercise
/// that path, not the slow path masquerading as fast.
fn make_converged_param() -> FloatParam {
    let p = make_param();
    p.smoother.snap(f64::from(p.value()));
    p
}

fn bench_gain_vs_gain_simd(c: &mut Criterion) {
    let mut g = c.benchmark_group("gain_vs_gain_simd");
    g.throughput(Throughput::Elements((FRAMES * CHANS) as u64));

    let inp: Vec<Vec<f32>> = (0..CHANS)
        .map(|_| (0..FRAMES).map(|i| (i as f32) * 1e-4).collect())
        .collect();

    g.bench_function("gain", |b| {
        let gain_p = make_param();
        let pan_p = make_param();
        let mut out: Vec<Vec<f32>> = (0..CHANS).map(|_| vec![0.0; FRAMES]).collect();
        b.iter(|| {
            naive_gain_process(&inp, &mut out, &gain_p, &pan_p);
            black_box(&out);
        });
    });

    g.bench_function("gain_simd_fast", |b| {
        let gain_p = make_converged_param();
        let pan_p = make_converged_param();
        let mut out: Vec<Vec<f32>> = (0..CHANS).map(|_| vec![0.0; FRAMES]).collect();
        b.iter(|| {
            simd_fast_gain_process(&inp, &mut out, &gain_p, &pan_p);
            black_box(&out);
        });
    });

    g.bench_function("gain_simd_slow", |b| {
        let gain_p = make_param();
        let pan_p = make_param();
        let mut out: Vec<Vec<f32>> = (0..CHANS).map(|_| vec![0.0; FRAMES]).collect();
        b.iter(|| {
            simd_slow_gain_process(&inp, &mut out, &gain_p, &pan_p);
            black_box(&out);
        });
    });

    g.finish();
}

criterion_group!(
    benches,
    bench_smoother_traffic,
    bench_gain_inner_loop,
    bench_trim_block,
    bench_simd_ops_scalar_vs_wide,
    bench_gain_vs_gain_simd,
);
criterion_main!(benches);
