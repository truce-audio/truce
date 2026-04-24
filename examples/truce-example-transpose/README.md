# Transpose

MIDI note transposer with semitone and octave controls.

## What it demonstrates

- MIDI effect plugin (processes note events, no audio processing)
- Discrete integer parameters (`range = "discrete(-12, 12)"`)
- Active note map to prevent stuck notes when transposition changes
- Output event generation via `ProcessContext::output_events`

## Parameters

| Name | Range | Unit | Description |
|------|-------|------|-------------|
| Semitones | -12 to 12 | st | Transpose by semitones |
| Octave | -3 to 3 | -- | Transpose by octaves |

## Build and test

```bash
cargo build -p truce-example-transpose
cargo test -p truce-example-transpose
```
