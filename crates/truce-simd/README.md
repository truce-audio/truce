# truce-simd

Portable SIMD primitives + canonical block-rate audio ops for
truce plugins.

## Overview

Two backends, feature-gated behind a stable public API:

- **`wide-backend`** (default) - stable Rust, uses the
  [`wide`](https://crates.io/crates/wide) crate. Maps
  `f32x4` / `f32x8` / `f64x2` / `f64x4` onto SSE2 / AVX2 on x86,
  NEON on AArch64, scalar fallback elsewhere.
- **`portable-simd-backend`** (planned) - nightly-only, swaps in
  `core::simd` when it stabilizes. Same public type aliases, so
  consumers don't have to rewrite.

```rust
use truce_simd::{ops, math};

// In `process()`:
ops::scale_block(out, src, gain);                // out = src * gain
ops::mac_block(out, src, gain);                  // out += src * gain
ops::mix_block(out, a, gain_a, b, gain_b);       // dry/wet workhorse
math::tanh_block(out, src);                      // vectorized tanh
math::db_to_linear_block(out, src);              // vectorized dB -> lin
```

All ops are pure math - no atomics, no parameter reads, no
audio-thread allocation. They're the inner-loop complement to a
`truce_params::FloatParam::read_into(&mut buf)` /
`truce_core::AudioBuffer::chunks_mut::<N>()` driver. See the
[block-gain example](https://github.com/truce-audio/truce/tree/main/examples/truce-example-block-gain)
for the canonical fast-path / slow-path shape.

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
