# Examples

Example plugins covering effects, instruments, MIDI processors, and
GUI framework integrations.

## Plugins

| Plugin                      | Type        | GUI     | Description                                       |
|-----------------------------|-------------|---------|---------------------------------------------------|
| [gain](gain/)               | Effect      | Built-in | Gain + pan with metering, XY pad, bypass         |
| [eq](eq/)                   | Effect      | Built-in | 3-band parametric EQ with biquad filters         |
| [synth](synth/)             | Instrument  | Built-in | 16-voice poly synth with oscillator, filter, ADSR|
| [transpose](transpose/)     | MIDI Effect | Built-in | Note transposition with stuck-note prevention    |
| [arpeggio](arpeggio/)       | MIDI Effect | Built-in | Tempo-synced arpeggiator with 4 patterns         |
| [gain-egui](gain-egui/)     | Effect      | egui     | Gain plugin with egui knobs, sliders, XY pad     |
| [gain-iced](gain-iced/)     | Effect      | Iced     | Gain plugin with iced auto-generated + custom UI |
| [gain-slint](gain-slint/)   | Effect      | Slint    | Gain plugin with declarative `.slint` markup     |

## Building

```bash
cargo build --workspace                       # build all (CLAP + VST3)
cargo build -p gain --features vst2,au,aax    # build with extra formats
cargo test --workspace                        # run all tests
cargo xtask install -p gain                   # install to system plugin folders
cargo run -p synth --features standalone      # run synth standalone
```

## Project structure

Each example follows the same layout:

```
examples/<name>/
├── Cargo.toml       # dependencies + feature flags
└── src/
    └── lib.rs       # plugin implementation
```

GUI examples may have additional files:

```
examples/gain-slint/
├── build.rs         # slint-build compilation
└── ui/
    └── main.slint   # declarative UI markup
```

Additional source files: `synth/src/voice.rs` (voice/envelope/oscillator), `eq/src/biquad.rs` (biquad filter).
