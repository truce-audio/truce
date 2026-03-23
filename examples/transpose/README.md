# Transpose

MIDI note transposer with semitone and octave controls.

## What it demonstrates

- MIDI effect plugin (processes note events, ignores audio)
- Discrete (integer-stepped) parameters (`range = "discrete(-12, 12)"`)
- Active note map to prevent stuck notes when transposition changes mid-hold
- Output event generation via `ProcessContext::output_events`

## Parameters

| Name      | Range     | Unit | Description                          |
|-----------|-----------|------|--------------------------------------|
| Semitones | -12 -- 12 | st   | Transpose by semitones               |
| Octave    | -3 -- 3   | --   | Transpose by octaves (+/- 36 semis)  |

## Code structure

- `active_notes: [Option<u8>; 128]` -- maps input note number to the output note that was actually sent. On NoteOff, the stored pitch is used instead of recomputing, so changing transposition while holding notes does not cause stuck notes.
- `process` -- iterates events, applies `semitones + octave * 12` shift, clamps to 0-127, pushes to output
- No audio processing -- the audio buffer is untouched

## Build and test

```bash
cargo build -p transpose
cargo test -p transpose
```
