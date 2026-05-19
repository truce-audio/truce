# truce-derive

All proc macros for truce plugin authoring.

## Overview

Two macro families live here:

- **Parameter / state derives** - `#[derive(Params)]`,
  `#[derive(ParamEnum)]`, `#[derive(State)]`. Generate the
  parameter-discovery, indexed access, display formatting, and
  state-roundtrip glue every plugin needs. Pure `syn` + `quote`.
- **`plugin_info!()`** - reads `truce.toml` at compile time and
  expands to a `PluginInfo` struct literal containing the plugin
  name, IDs, vendor, category, AU type / subtype / manufacturer
  codes, and version. Removes the need for hand-written metadata
  constants. Pulls in `toml` + `serde` at proc-macro compile time.
  Tracks `truce.toml` for rebuilds via `include_bytes!`, so plugin
  crates don't need a `build.rs`.

Plugin authors don't depend on this crate directly - everything is
re-exported through `truce::prelude` (or `truce::plugin_info!()` /
`truce::Params` etc. at the facade root).

## Macros

### `#[derive(Params)]`

Applied to a struct whose fields are `FloatParam`, `IntParam`,
`BoolParam`, or `EnumParam`. Generates trait impls for parameter
discovery, indexed access, and state round-tripping.

### `#[derive(ParamEnum)]`

Applied to an enum to make it usable as an `EnumParam` value.
Generates variant-to-index mapping and display names.

### `#[derive(State)]`

Generates per-field save/restore for plugin state structs.

### `plugin_info!()`

Expands to a `PluginInfo` struct populated from `truce.toml`. Reads
the `[[plugin]]` entry matching the current crate's package name and
the `[vendor]` section. Reads:

- Plugin name and unique ID
- Vendor name and URL
- Plugin category (effect / instrument / midi / analyzer / tool)
- AU type, subtype, manufacturer codes
- Optional version override

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

impl Plugin for MyPlugin {
    fn info() -> PluginInfo {
        truce::plugin_info!()
    }
}
```

Part of [truce](https://github.com/truce-audio/truce).
