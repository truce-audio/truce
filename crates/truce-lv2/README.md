# truce-lv2

LV2 format wrapper for the truce audio plugin framework.

## Overview

Bridges a truce `PluginExport` implementation to the
[LV2](https://lv2plug.in/) plugin API. LV2's C ABI is small and stable,
so the bindings are hand-rolled rather than pulled from a large
`lv2-sys` crate. Supports audio, MIDI, state, and UI on every truce
platform - X11UI on Linux, CocoaUI on macOS, WindowsUI on Windows.

User plugins typically take a direct optional dep on this crate
(`truce-lv2 = { workspace = true, optional = true }`) gated behind an
`lv2` Cargo feature; the `truce::plugin!` macro emits a
`::truce_lv2::export_lv2!(...)` call when that feature is on. `cargo
truce build --lv2` / `install --lv2` selects it at the CLI.

## What it handles

- LV2 `lv2_descriptor` entry point and bundle layout
- Audio + control + atom port layout
  - `0..num_in` - audio input (one port per channel)
  - `num_in..num_in+num_out` - audio output (one port per channel)
  - next N - control input (one port per parameter)
  - one `AtomPort` for MIDI input (if the plugin accepts MIDI)
- State save/restore via the LV2 State extension
- UI hosting per platform (X11UI / CocoaUI / WindowsUI)
- Turtle (`manifest.ttl`, `plugin.ttl`) sidecars emitted by the
  `export_lv2!` proc-macro at compile time

## Key macro

- **`export_lv2!`** -- generates the LV2 entry point for a `PluginExport` type

Part of [truce](https://github.com/truce-audio/truce).
