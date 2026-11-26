# truce-build

Build-time schema + target-dir helpers for truce.

## Overview

Plugin crates do not need a `build.rs` - `truce::plugin_info!()` reads
`truce.toml` directly at compile time and tracks it via
`include_bytes!`. This crate exists for three roles:

- **`Config` / `PluginDef` / `VendorConfig`** - the shared deserializer
  for `truce.toml`, used by both `truce-derive` (proc macros) and
  `cargo-truce` (install / build pipeline).
- **`target_dir(root)`** - resolves cargo's effective target directory
  for a workspace root, honoring `CARGO_TARGET_DIR` and
  `[build].target-dir` in `.cargo/config.toml`. Used by runtime callers
  (cargo-truce, truce-test) that need to anchor artifact paths.
- **`meta`** - the format-neutral metadata tree at
  `<target>/truce-meta/<crate>/`, which `derive(Params)` fills while a
  plugin compiles and `cargo truce` resolves at package time.
  `meta::resolve_root` flattens it into one id-resolved parameter list,
  reproducing the runtime's id arithmetic exactly; every format renders
  from that. `schema = 1` makes the tree a versioned contract, so a
  format living outside this repo can read it. `lv2_meta` is the LV2
  view of it - where the lossy mappings and the refusals LV2 can't
  encode live - and `lv2` turns that into Turtle.

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
