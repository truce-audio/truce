# truce-params

Parameter system for the truce audio plugin framework.

## Overview

Provides the types and utilities for declaring, smoothing, and formatting
plugin parameters. Parameters are the primary interface between a plugin and
its host -- they drive automation, presets, and UI controls. Use this crate's
types inside your `Params` struct, then derive with `#[derive(Params)]`.

## Key types

- **`FloatParam`** -- continuous floating-point parameter
- **`IntParam`** -- discrete integer parameter
- **`BoolParam`** -- on/off toggle parameter
- **`EnumParam`** -- parameter backed by a Rust enum (via `#[derive(ParamEnum)]`)
- **`ParamRange`** -- defines value ranges and mapping curves (linear, logarithmic, discrete)
- **`Smoother` / `SmoothingStyle`** -- per-sample parameter smoothing to avoid zipper noise
- **`ParamInfo`** -- metadata (name, unit label, flags) for host communication
- **`format_param_value`** -- human-readable display formatting

## Example

```rust
#[derive(Params)]
struct MyParams {
    #[param(name = "Gain", range = linear(-60.0, 0.0), unit = "dB")]
    gain: FloatParam,

    #[param(name = "Mode")]
    mode: EnumParam<FilterMode>,
}
```

Part of [truce](https://github.com/truce-audio/truce).
