# truce-utils

Lightweight, dependency-free utilities shared across the truce
workspace.

## Overview

Pure-function helpers with zero crate dependencies, so build-time
tools (`cargo-truce`, `truce-build`) can use them without
inheriting the audio / params / GUI dependency closure that
`truce-core` carries.

## Modules

- **`cast`** - numeric-cast helpers for the audio-plugin / host
  FFI boundary: `usize ↔ u32` length casts with overflow asserts,
  host-`f64` ↔ DSP-`i64` sample-position narrowing,
  `discrete_index` / `discrete_norm` for stepped-param math.
  Idempotent NaN/inf-safe.
- **`midi`** - value-domain normalize / denormalize between
  wire-native integers (`u7`, 14-bit pitch bend) and `f32`
  ranges, plus the MIDI spec's MIDI 1.0 ↔ MIDI 2.0 bit-replication
  bridges.
- **`slugify`** - `&str → kebab-case-string` for bundle names and
  install paths.
- **`shell_sidecar`** - resolves the `$HOME/.truce/shell/<crate>.path`
  sidecar that `cargo truce install --shell` writes, so a
  hot-reload-mode shell can find its matching logic dylib at
  runtime.

## Cross-precision sample helpers

The historical `cast::sample_f32` / `cast::param_f32` helpers were
removed in favour of the `Float` trait methods (`to_f32`,
`from_f64`) that live on `truce_params::sample`. Both did the
same NaN-debug-asserted f64 ↔ f32 narrowing; consolidating to the
trait method removed one named-function call per cast site.

Part of [truce](https://github.com/truce-audio/truce).
