# truce-build

Build-time helpers for truce plugins.

## Overview

Plugin crates no longer need a `build.rs` — the `truce::plugin_info!()`
proc macro reads `truce.toml` directly at compile time and tracks it
via `include_bytes!`. This crate exists for two remaining roles:

- **`Config` / `PluginDef` / `VendorConfig`** — the shared deserializer
  for `truce.toml`, used by both `truce-derive` (proc macros) and
  `cargo-truce` (install / build pipeline).
- **`target_dir(root)`** — resolves cargo's effective target directory
  for a workspace root, honouring `CARGO_TARGET_DIR` and
  `[build].target-dir` in `.cargo/config.toml`. Used by runtime callers
  (cargo-truce, truce-test) that need to anchor artifact paths.

## Compatibility shim

`emit_plugin_env()` is preserved for plugin crates still on a pre-0.33
scaffold that ship a `build.rs`. It now only emits
`cargo:rerun-if-changed=truce.toml` (belt-and-braces alongside the
proc-macro's `include_bytes!` tracking) — the historical `TRUCE_*`
env-var bake is gone, since nothing in the workspace consumed it any
more. Will be marked `#[deprecated]` once any out-of-tree pre-0.33
plugins have had time to drop their `build.rs`.

Part of [truce](https://github.com/truce-audio/truce).
