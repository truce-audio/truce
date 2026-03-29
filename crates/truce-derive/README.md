# truce-derive

Proc macros for truce plugins.

Provides the `plugin_info!()` macro, which reads `truce.toml` at compile time
and generates a `PluginInfo` struct literal with all plugin and vendor metadata.
This eliminates the need for a `build.rs` in every plugin crate.

## Usage

```rust
fn info() -> PluginInfo {
    truce::plugin_info!()
}
```

Part of [truce](https://github.com/truce-audio/truce).
