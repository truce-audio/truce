# Reference

Everything a first-time truce user needs, in order. Each chapter
is self-contained; most people will read 1–3 straight through and
then dip into the rest by topic.

## The tutorial track

| # | Chapter | What you get |
|---|---------|--------------|
| 1 | [install.md](install.md) | Rust + system deps + `cargo install cargo-truce` + `cargo truce doctor` |
| 2 | [first-plugin.md](first-plugin.md) | `cargo truce new`, a tour of the generated files, `install`, load in a DAW |
| 3 | [plugin-anatomy.md](plugin-anatomy.md) | `PluginLogic` trait, bus layouts, state persistence |
| 4 | [parameters.md](parameters.md) | `#[derive(Params)]`, `#[param(...)]` attributes, smoothing, meters |
| 5 | [processing.md](processing.md) | `process()` patterns for effects, MIDI, sample-accurate events, synths |
| 6 | [gui.md](gui.md) | Built-in GUI widgets + the alternative backends |
| 7 | [hot-reload.md](hot-reload.md) | ~2 second edit → hear loop with `--features dev` |
| 8 | [shipping.md](shipping.md) | `cargo truce install / build / validate / package`, signing, installers |

## See also

- [Formats](../formats/) — per-format reference (CLAP, VST3, VST2,
  LV2, AU, AAX) with env vars, install paths, signing, and gotchas.
- [GUI backends](../gui/) — deep-dive guides for egui, iced, Slint,
  and raw-window-handle.
- [Built-in GUI reference](../gui/built-in.md) — the `GridLayout` builder, every widget constructor, theming.
- [Status](../status.md) — what's shipped, what's next.
