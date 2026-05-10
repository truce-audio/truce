# Reference

> **Build profile.** Every `cargo truce` command (`install`, `build`,
> `package`, `run`, `screenshot`) defaults to the cargo **release**
> profile — plugins are typically loaded into a DAW where debug-build
> DSP can spike CPU under load. Pass `--debug` to opt into the cargo
> dev profile for fast-compile iteration.

| # | Chapter | What you get |
|---|---------|--------------|
| 1 | [install.md](install.md) | Rust + system deps + `cargo install cargo-truce` + `cargo truce doctor` |
| 2 | [first-plugin.md](first-plugin.md) | `cargo truce new`, a tour of the generated files, `install`, load in a DAW |
| 3 | [plugin-anatomy.md](plugin-anatomy.md) | `PluginLogic` trait, bus layouts, state persistence |
| 4 | [parameters.md](parameters.md) | `#[derive(Params)]`, `#[param(...)]` attributes, smoothing, meters |
| 5 | [processing.md](processing.md) | `process()` patterns for effects, MIDI, sample-accurate events, synths |
| 6 | [midi.md](midi.md) | Reading and emitting MIDI events; per-format support; testing MIDI plugins |
| 7 | [gui.md](gui.md) | Built-in GUI widgets + the alternative backends |
| 8 | [hot-reload.md](hot-reload.md) | ~2 second edit → hear loop with `--features shell` |
| 9 | [shipping.md](shipping.md) | `cargo truce install / build / validate / package`, signing, installers |

## See also

- [Formats](../formats/) — per-format reference (CLAP, VST3, VST2,
  LV2, AU, AAX) with env vars, install paths, signing, and gotchas.
- [GUI backends](gui/) — deep-dive guides for egui, iced, Slint,
  and raw-window-handle.
- [Built-in GUI reference](gui/built-in.md) — the `GridLayout` builder, every widget constructor, theming.
- [Audio testing](audio-testing.md) — `truce_test::PluginDriver` for
  in-process audio + MIDI tests.
- [Screenshot testing](gui/screenshot-testing.md) — visual regression
  tests for the GUI.
- [Status](../README.md) — what's shipped, what's next.
