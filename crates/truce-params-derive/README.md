# truce-params-derive

Derive macros for truce parameter structs.

## Overview

Provides `#[derive(Params)]` and `#[derive(ParamEnum)]` to generate the
boilerplate needed to expose parameter structs to the host. The generated code
handles parameter enumeration, get/set by index, display formatting, and state
serialization. Plugin authors rarely depend on this crate directly -- it is
re-exported through `truce::prelude`.

## Macros

### `#[derive(Params)]`

Applied to a struct whose fields are `FloatParam`, `IntParam`, `BoolParam`, or
`EnumParam`. Generates trait implementations for parameter discovery, indexed
access, and state round-tripping.

### `#[derive(ParamEnum)]`

Applied to an enum to make it usable as an `EnumParam` value. Generates
variant-to-index mapping and display names.

## Example

```rust
use truce::prelude::*;

#[derive(ParamEnum)]
enum FilterMode { LowPass, HighPass, BandPass }

#[derive(Params)]
struct MyParams {
    #[param(name = "Cutoff", range = log(20.0, 20000.0), unit = "Hz")]
    cutoff: FloatParam,

    #[param(name = "Mode")]
    mode: EnumParam<FilterMode>,
}
```

Part of [truce](https://github.com/truce-audio/truce).
