# Examples

Five example plugins covering effects, instruments, and MIDI processors.

| Plugin                      | Type        | Description                                           |
|-----------------------------|-------------|-------------------------------------------------------|
| [gain](gain/)               | Effect      | Gain + pan with metering, XY pad, bypass              |
| [eq](eq/)                   | Effect      | 3-band parametric EQ with biquad filters              |
| [synth](synth/)             | Instrument  | 16-voice poly synth with oscillator, filter, ADSR     |
| [transpose](transpose/)     | MIDI Effect | Note transposition with stuck-note prevention         |
| [arpeggio](arpeggio/)       | MIDI Effect | Tempo-synced arpeggiator with 4 patterns              |

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
├── truce.toml       # plugin metadata (name, IDs, category)
└── src/
    └── lib.rs       # plugin implementation
```

Additional source files: `synth/src/voice.rs` (voice/envelope/oscillator), `eq/src/biquad.rs` (biquad filter).
