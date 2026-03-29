# truce-build

Build-time helper for truce plugins.

## Overview

Reads `truce.toml` and emits `cargo:rustc-env` directives so that the
`plugin_info!()` macro can derive all plugin metadata at compile time. If your
plugin uses `plugin_info!()` via the `truce` crate, include this in your
`build.rs` to make the environment variables available.

## Usage

Add to your plugin crate's `build.rs`:

```rust
fn main() {
    truce_build::emit_plugin_env();
}
```

## Emitted environment variables

| Variable | Source |
|----------|--------|
| `TRUCE_PLUGIN_NAME` | Display name |
| `TRUCE_PLUGIN_ID` | Combined vendor + plugin ID (CLAP / VST3) |
| `TRUCE_VENDOR_NAME` | Vendor name |
| `TRUCE_VENDOR_URL` | Vendor website URL |
| `TRUCE_CATEGORY` | `"Effect"` or `"Instrument"` |
| `TRUCE_AU_TYPE` | AU component type code |
| `TRUCE_AU_SUBTYPE` | AU component subtype code |
| `TRUCE_AU_MANUFACTURER` | AU manufacturer code |
| `TRUCE_PLUGIN_VERSION` | Optional version override from `truce.toml` |

Part of [truce](https://github.com/truce-audio/truce).
