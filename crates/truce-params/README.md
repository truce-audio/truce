# truce-params

Parameter system for the truce audio plugin framework.

## Overview

Provides the types and utilities for declaring, smoothing, and formatting
plugin parameters. Parameters are the primary interface between a plugin and
its host -- they drive automation, presets, and UI controls. Use this crate's
types inside your `Params` struct, then derive with `#[derive(Params)]`.

## Key types

- **`FloatParam`** -- continuous floating-point parameter (with smoother)
- **`IntParam`** -- discrete integer parameter. Pick this over
  `FloatParam` whenever `range = "discrete(...)"` describes the
  parameter - the type expresses intent and skips the unused
  smoother state.
- **`BoolParam`** -- on/off toggle parameter
- **`EnumParam`** -- parameter backed by a Rust enum (via `#[derive(ParamEnum)]`)
- **`ParamRange`** -- defines value ranges and mapping curves (linear, logarithmic, discrete)
- **`Smoother` / `SmoothingStyle`** -- per-sample parameter smoothing to avoid zipper noise
- **`ParamInfo`** -- metadata (name, unit label, flags) for host communication
- **`Float` / `Sample`** -- sealed traits over `f32` / `f64` that
  carry the cross-precision math methods (`to_f32`, `to_f64`,
  `from_f32`, `from_f64`, plus `exp`, `log10`, `powf`). `Sample`
  is `Float + Default + Send + Sync + 'static` - the audio buffer
  element bound.
- **`FloatParamReadF32` / `FloatParamReadF64`** -- precision-routed
  read traits. The prelude brings one of them into scope as `_`;
  `param.read()` then returns `f32` or `f64` directly without
  per-call-site annotation.

## Example

```rust
#[derive(Params)]
struct MyParams {
    #[param(name = "Gain", range = "linear(-60.0, 0.0)", unit = "dB",
            smooth = "exp(5)")]
    gain: FloatParam,

    #[param(name = "Semitones", range = "discrete(-12, 12)", unit = "st")]
    semitones: IntParam,  // discrete-integer params use IntParam, not FloatParam

    #[param(name = "Mode")]
    mode: EnumParam<FilterMode>,
}
```

Part of [truce](https://github.com/truce-audio/truce).
