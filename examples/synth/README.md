# Synth

16-voice polyphonic synthesizer with oscillator, filter, and ADSR envelope.

## What it demonstrates

- Instrument plugin with MIDI input (stereo output, no audio input)
- Sample-accurate MIDI event handling (`NoteOn`/`NoteOff`)
- Per-voice ADSR envelope and one-pole low-pass filter
- Enum parameter (`EnumParam<Waveform>`) with dropdown widget
- Voice stealing (oldest-first) and tail mode
- Section-based grid layout (`FILTER`, `ENVELOPE`)

## Parameters

| Name | Range | Unit | Description |
|------|-------|------|-------------|
| Waveform | Sine/Saw/Square/Triangle | -- | Oscillator shape |
| Volume | -60 to 0 | dB | Master output level |
| Cutoff | 20 to 20000 | Hz | Low-pass filter cutoff |
| Resonance | 0 to 1 | -- | Filter resonance |
| Attack | 0.001 to 5 | s | Envelope attack time |
| Decay | 0.001 to 5 | s | Envelope decay time |
| Sustain | 0 to 1 | -- | Envelope sustain level |
| Release | 0.01 to 10 | s | Envelope release time |

## Files

- `src/lib.rs` — plugin logic, MIDI dispatch, GUI layout
- `src/voice.rs` — `Voice`, `Envelope`, `OnePoleFilter`

## Build and test

```bash
cargo build -p truce-example-synth
cargo test -p truce-example-synth
cargo run -p truce-example-synth --features standalone
```
