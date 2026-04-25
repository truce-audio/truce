# Examples

Example plugins covering effects, instruments, MIDI processors, and
GUI framework integrations.

## Plugins

| Plugin | Type | GUI | What it shows |
|--------|------|-----|---------------|
| [gain](gain/) | Effect | Built-in | Gain, pan, metering, XY pad |
| [gain-egui](gain-egui/) | Effect | egui | Same gain plugin with egui widgets |
| [gain-iced](gain-iced/) | Effect | Iced | Same gain plugin with iced widgets |
| [gain-slint](gain-slint/) | Effect | Slint | Same gain plugin with `.slint` markup |
| [eq](eq/) | Effect | Built-in | 3-band parametric EQ with biquad filters |
| [synth](synth/) | Instrument | Built-in | 16-voice poly synth with filter and ADSR |
| [transpose](transpose/) | MIDI Effect | Built-in | Note transposition with stuck-note prevention |
| [arpeggio](arpeggio/) | MIDI Effect | Built-in | Tempo-synced arpeggiator with 4 patterns |

The four gain variants (gain, gain-egui, gain-iced, gain-slint) implement
the same plugin with different GUI frameworks. Compare them to see how
each framework handles the same layout.

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
