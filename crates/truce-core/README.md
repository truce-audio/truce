# truce-core

Core traits and types for the truce audio plugin framework.

## Overview

This crate defines the fundamental abstractions that all truce crates build on.
Plugin authors should depend on the `truce` crate instead, which re-exports
everything from here. This crate is intended for internal use and for authors of
format wrappers or GUI backends.

## Key types and traits

- **`Plugin`** -- the main trait a plugin implements for audio processing
- **`PluginExport`** -- wraps a `Plugin` for format-specific export
- **`AudioBuffer`** -- interleaved/deinterleaved sample buffer abstraction
- **`Editor`** -- trait for plugin GUI editors
- **`ProcessContext` / `ProcessStatus`** -- audio callback context and return status
- **`Event` / `TransportInfo`** -- MIDI events and DAW transport state
- **`BusConfig` / `BusLayout`** -- I/O channel configuration
- **`PluginInfo` / `PluginCategory`** -- plugin metadata (name, ID, vendor, category)

## Utilities

- `db_to_linear` / `linear_to_db` -- gain conversion helpers
- `midi_note_to_freq` -- MIDI note number to frequency in Hz

Part of [truce](https://github.com/truce-audio/truce).
