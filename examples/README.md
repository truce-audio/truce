# Examples

Example plugins covering effects, instruments, MIDI processors, and
GUI framework integrations.

## Plugins

| Plugin | Type | GUI | Screenshot |
|--------|------|-----|-----------|
| [gain](truce-example-gain/) | Effect | Built-in | <img src="screenshots/gain_default.png" width="300"> |
| [eq](truce-example-eq/) | Effect | Built-in | <img src="screenshots/eq_default.png" width="300"> |
| [synth](truce-example-synth/) | Instrument | Built-in | <img src="screenshots/synth_default.png" width="300"> |
| [transpose](truce-example-transpose/) | MIDI | Built-in | <img src="screenshots/transpose_default.png" width="300"> |
| [arpeggio](truce-example-arpeggio/) | MIDI | Built-in | <img src="screenshots/arpeggio_default.png" width="300"> |
| [tremolo](truce-example-tremolo/) | Effect | egui | <img src="screenshots/tremolo_default.png" width="300"> |
| [gain-egui](truce-example-gain-egui/) | Effect | egui | <img src="screenshots/gain_egui_default.png" width="300"> |
| [gain-iced](truce-example-gain-iced/) | Effect | Iced | <img src="screenshots/gain_iced_default.png" width="300"> |
| [gain-slint](truce-example-gain-slint/) | Effect | Slint | <img src="screenshots/gain_slint_default.png" width="300"> |

The four gain variants (gain, gain-egui, gain-iced, gain-slint) implement
the same plugin with different GUI frameworks. Compare them to see how
each framework handles the same layout.

## Out-of-tree

Larger examples live in their own repos — useful when you want to
see what truce looks like at the scale of a real plugin rather than
a 100-line teaching example.

| Plugin | What it shows |
|--------|---------------|
| [truce-analyzer](https://github.com/truce-audio/truce-analyzer) | Real-time spectrum analyzer with diff overlay; non-trivial GUI built on truce. |

## Building

```bash
cargo build --workspace                       # build all
cargo test --workspace                        # run all tests
cargo truce build                             # build every format into target/bundles/
cargo truce install -p truce-example-gain     # install one plugin
cargo truce run -p truce-example-synth        # run a plugin standalone
cargo truce validate -p truce-example-gain    # auval + pluginval + clap-validator
```

## Project structure

Each example follows the same layout:

```
examples/<name>/
├── Cargo.toml
└── src/
    └── lib.rs
```

GUI framework examples may have additional files:

```
examples/gain-slint/
├── build.rs              # slint-build compilation
└── ui/
    └── main.slint        # declarative UI markup
```
