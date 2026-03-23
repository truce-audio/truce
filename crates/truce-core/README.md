# truce-core

Core types and traits for the truce audio plugin framework.

Defines the fundamental abstractions that all truce crates build on:

- **`Plugin`** — the main trait a plugin implements for audio processing
- **`PluginExport`** — wraps a `Plugin` for format-specific export
- **`AudioBuffer`** — sample buffer abstraction
- **`Editor`** — trait for plugin GUI editors
- **`PluginInfo` / `PluginCategory`** — plugin metadata
- **`ProcessContext` / `ProcessStatus`** — audio processing context
- **`Event` / `TransportInfo`** — MIDI events and DAW transport state
- **`BusConfig` / `BusLayout`** — I/O channel configuration
- **Utilities** — `db_to_linear`, `linear_to_db`, `midi_note_to_freq`

Most plugin authors should depend on `truce` directly rather than this crate.
