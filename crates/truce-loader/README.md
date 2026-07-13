# truce-loader

Plugin runtime + hot-reload infrastructure for truce.

## Overview

Hot-reload mechanics for truce: dylib loading, ABI canary
(including a `sample_precision` field that pins `f32` vs `f64`),
and the shells (`HotShell<P, S>`, `StaticShell<P,
L, S>`) that bridge the user-facing leaf traits onto
`truce_core::PluginRuntime` for format wrappers. Used by every truce
plugin, in two modes:

- **Static (default).** `export_static!` embeds the user's struct
  directly into the format wrapper at compile time. Zero runtime
  overhead, single dylib.
- **Hot-reload (opt-in).** `export_plugin!` exports the plugin
  across a `#[no_mangle]` C ABI in a separate dylib; the shell
  loads it via `libloading`, verifies ABI compatibility, and swaps
  the logic dylib on rebuild without restarting the DAW.
  Preserves audio continuity.

Plugin authors don't reach into this crate directly. They write
a single `impl PluginLogic` (from `truce-plugin`) on their plugin
struct -- one trait covering both DSP (`reset`, `process`, ...) and
GUI (`editor`) -- and `truce::plugin!` emits
the right `export_*!` call based on the `shell` Cargo feature.

## Key types and macros

- **`HotShell<P, S = f32>`** -- shell-side dylib loader and
  hot-swap manager, generic over the plugin's sample type.
- **`StaticShell<P, L, S = f32>`** -- shell-side wrapper that
  embeds the plugin at compile time, generic over the sample
  type.
- **`NativeLoader<S>`** -- the `libloading`-backed dylib loader
  that holds the resolved flat-ABI symbol table (`LogicSymbols<S>`)
  and the ABI-canary machinery.
- **`AbiCanary`** -- ABI fingerprint compared between shell and
  dylib before loading. Includes `sample_precision: u8` so a
  prelude64 logic dylib loaded by an f32 shell (or vice versa)
  fails the canary check rather than binding to a wrong-layout
  dylib.
- **`export_static!`** -- emits the `__HotShellWrapper` for static
  mode.
- **`export_plugin!`** -- emits the `#[no_mangle]` C ABI symbols
  for shell mode (`truce_init_state`, `truce_process`,
  `truce_build_editor`, `truce_abi_canary_v2`, ...). Each one
  carries the plugin's chosen precision (via the prelude's
  `Sample` alias).

## Features

| Feature | Description |
|---------|-------------|
| `shell` | Enable dylib loading via `libloading` (turns on `HotShell`) |
| `hot-debug` | Verbose hot-reload diagnostics (load timings, ABI checks) |

## Usage

Enable the dynamic shell (hot-reload) during development:

```toml
[dependencies]
truce = { version = "6.1", features = ["shell"] }
```

(Cargo's caret resolver expands `"6.1"` to `>=6.1.0, <7.0.0`,
so you'll pick up every `6.1.x` patch release without re-editing.
To track an unreleased checkout, swap for
`git = "https://github.com/truce-audio/truce", branch = "main"`.)

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
