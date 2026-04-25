# truce-derive

Proc macros for truce plugin metadata.

## Overview

Provides the `plugin_info!()` macro, which reads `truce.toml` at compile time
and generates a `PluginInfo` struct literal containing the plugin name, unique
ID, vendor, category, and version. Removes the need for hand-written metadata
constants. (Plugin crates still need a small `build.rs` calling
`truce_build::emit_plugin_env()` — that handles format-feature check-cfg and
optional env-var overrides; see the `truce-build` crate.)

The macro is re-exported through `truce::plugin_info!()`, so plugin authors do
not need to depend on this crate directly.

## Key macro

- **`plugin_info!()`** -- expands to a `PluginInfo` struct populated from `truce.toml`

## Usage

```rust
use truce::prelude::*;

impl Plugin for MyPlugin {
    fn info() -> PluginInfo {
        truce::plugin_info!()
    }
}
```

## What it reads from `truce.toml`

- Plugin name and unique ID
- Vendor name and URL
- Plugin category (effect or instrument)
- AU type, subtype, and manufacturer codes
- Optional version override

Part of [truce](https://github.com/truce-audio/truce).
