# truce-core

Core traits and types for the truce audio plugin framework.

## Overview

This crate defines the fundamental abstractions that all truce crates build on.
Plugin authors should depend on the `truce` crate instead, which re-exports
everything from here. This crate is intended for internal use and for authors of
format wrappers or GUI backends.

## Key types and traits

- **`PluginRuntime`** -- the wrapper-facing runtime trait every
  format wrapper consumes. Plugin authors don't implement this
  directly - the `truce::plugin!` macro emits the impl.
  `PluginRuntime::Sample` is the audio buffer's element type
  (`f32` / `f64`).
- **`PluginExport`** -- extension trait over `PluginRuntime` adding
  the format-specific export glue
- **`AudioBuffer<'a, S>`** -- deinterleaved sample buffer, generic over
  the plugin's sample type (defaults to `f32`)
- **`Editor`** / **`PluginContext<P>`** -- editor lifecycle and the
  bridge handle the GUI uses to talk to the host
- **`ProcessContext` / `ProcessStatus`** -- audio callback context and return status
- **`Event` / `TransportInfo`** -- MIDI events and DAW transport state
- **`BusConfig` / `BusLayout`** -- I/O channel configuration
- **`PluginInfo` / `PluginCategory`** -- plugin metadata (name, ID, vendor, category)
- **`PluginContextReadF32` / `PluginContextReadF64`** -- extension
  traits that route `state.get_param(...)` to the prelude's
  chosen precision (mirror of `FloatParamReadF{32,64}` for the
  audio thread)

## Utilities

- `db_to_linear` / `linear_to_db` -- gain conversion helpers (re-exported from `truce-params::sample::Float`)
- `midi_note_to_freq` -- MIDI note number to frequency in Hz

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
