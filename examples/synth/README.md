# Synth

16-voice polyphonic synthesizer with oscillator, filter, and ADSR envelope.

## What it demonstrates

- Instrument plugin with MIDI input (stereo output, no audio input)
- Sample-accurate MIDI event handling with `EventBody::NoteOn`/`NoteOff`
- Per-voice ADSR envelope and one-pole low-pass filter
- Enum parameter (`EnumParam<Waveform>`) with `ParamEnum` trait
- Voice stealing (oldest-first) and tail mode (`ProcessStatus::Tail`)
- Custom `BusLayout` declaration for instrument (output-only)

## Parameters

| Name      | Range           | Unit | Description                    |
|-----------|-----------------|------|--------------------------------|
| Waveform  | Sine/Saw/Sq/Tri | --   | Oscillator shape               |
| Cutoff    | 20 -- 20000     | Hz   | Low-pass filter cutoff         |
| Resonance | 0 -- 1          | --   | Filter resonance (unused)      |
| Attack    | 0.001 -- 5      | s    | Envelope attack time           |
| Decay     | 0.001 -- 5      | s    | Envelope decay time            |
| Sustain   | 0 -- 1          | --   | Envelope sustain level         |
| Release   | 0.01 -- 10      | s    | Envelope release time          |
| Volume    | -60 -- 0        | dB   | Master output level            |

## Code structure

- `lib.rs` -- plugin logic, MIDI dispatch, waveform enum, GUI layout
- `voice.rs` -- `Voice` (oscillator + phase accumulator), `Envelope` (linear ADSR), `OnePoleFilter`
- `process` iterates samples, dispatches MIDI at correct offsets, sums all voices, clamps output
- Dead voices are removed with `retain` after each block

## Build and test

```bash
cargo build -p synth
cargo test -p synth
cargo run -p synth --features standalone   # run standalone
```
