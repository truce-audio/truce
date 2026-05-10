# truce-au

Audio Unit v2 + v3 format wrapper for the truce audio plugin framework.

## Overview

Bridges a truce `PluginExport` implementation to Apple's Audio Unit API.
The same Rust dylib serves both AU v2 (`.component`, in-process) and AU
v3 (`.appex` inside a container `.app`, sandboxed) — only the bundle
shape and the surrounding shim differ. The framework dylib is the
identical artifact in either case; cargo-truce's install / package
commands wrap it in the right bundle layout.

This crate only builds on macOS. User plugins opt into AU by adding
`truce-au = { workspace = true, optional = true }` and gating it behind
an `au` Cargo feature.

## What it handles

- `AudioComponent` (v2) and `AUAudioUnit` (v3) registration
- Audio render block bridging + sample-rate / block-size lifecycle
- Parameter tree construction from truce parameter metadata
- Plugin state serialization via `truce_core::state`
- GUI view hosting via `NSViewController` (v2) / `AUViewController` (v3)
- Effects (`aufx`), instruments (`aumu`), and MIDI processors (`aumi`)

## Architecture

- **v2** uses a hand-written C shim (`shim/au_v2_shim.c`) that exposes an
  `AudioComponentFactory` to the host and forwards every callback into
  Rust via a C ABI function-pointer table.
- **v3** uses a Swift `AUAudioUnit` subclass generated at install time by
  `cargo truce` (so it can stamp in plugin-specific identifiers), with the
  same Rust-side callback table.

AU type codes (`aufx` / `aumu` / `aumi`) are derived from the plugin's
`category` in `truce.toml` by `truce::plugin_info!()` at compile time.

Part of [truce](https://github.com/truce-audio/truce).
