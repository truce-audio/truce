# truce-loader

Hot-reloadable plugin logic for truce.

## Overview

Splits a plugin into two parts: a static shell (the binary loaded by the DAW)
and a hot-reloadable logic dylib that can be swapped at runtime without
restarting the host. When you recompile your plugin logic, the shell detects
the new dylib and loads it on the fly, preserving audio continuity.

Developers implement the `PluginLogic` trait -- a safe Rust trait -- and export
it via `#[no_mangle]` functions. The shell loads the dylib with `libloading`,
verifies ABI compatibility, and delegates audio processing and GUI rendering to
the trait object.

This is the infrastructure behind the `hot-reload` feature flag in the `truce` crate.

## Key types

- **`PluginLogic`** -- trait for the reloadable half of a plugin
- **`LogicHost`** -- shell-side dylib loader and hot-swap manager

## Features

| Feature | Description |
|---------|-------------|
| `shell` | Enable dylib loading via `libloading` |
| `gpu` | GPU rendering support in the shell |

## Usage

Enable hot-reload during development:

```toml
[dependencies]
truce = { git = "https://github.com/truce-audio/truce", features = ["hot-reload"] }
```

Part of [truce](https://github.com/truce-audio/truce).
