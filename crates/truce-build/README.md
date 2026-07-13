# truce-build

Build-time schema + target-dir helpers for truce.

## Overview

Plugin crates do not need a `build.rs` - `truce::plugin_info!()` reads
`truce.toml` directly at compile time and tracks it via
`include_bytes!`. This crate exists for two roles:

- **`Config` / `PluginDef` / `VendorConfig`** - the shared deserializer
  for `truce.toml`, used by both `truce-derive` (proc macros) and
  `cargo-truce` (install / build pipeline).
- **`target_dir(root)`** - resolves cargo's effective target directory
  for a workspace root, honoring `CARGO_TARGET_DIR` and
  `[build].target-dir` in `.cargo/config.toml`. Used by runtime callers
  (cargo-truce, truce-test) that need to anchor artifact paths.

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
