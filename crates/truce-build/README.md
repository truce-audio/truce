# truce-build

Build-time helper for truce plugins.

Reads `truce.toml` and emits `cargo:rustc-env` directives so the
`plugin_info!()` macro can derive all plugin metadata at compile time.

## Usage

Add to your plugin crate's `build.rs`:

```rust
fn main() {
    truce_build::emit_plugin_env();
}
```

## Emitted variables

- `TRUCE_PLUGIN_NAME` — display name
- `TRUCE_PLUGIN_ID` — combined vendor + plugin ID (CLAP / VST3)
- `TRUCE_VENDOR_NAME` / `TRUCE_VENDOR_URL` — vendor metadata
- `TRUCE_AU_TYPE` / `TRUCE_AU_SUBTYPE` / `TRUCE_AU_MANUFACTURER` — AU identifiers
- `TRUCE_CATEGORY` — `"Effect"` or `"Instrument"`
- `TRUCE_PLUGIN_VERSION` — optional override from `truce.toml`
