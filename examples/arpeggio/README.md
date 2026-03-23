# Arpeggio

Tempo-synced arpeggiator that sequences held notes in configurable patterns.

## What it demonstrates

- Tempo-synced MIDI processing using `context.transport.tempo`
- Enum parameter (`EnumParam<ArpPattern>`) for pattern selection
- Stateful MIDI effect: tracks held notes, manages arp step counter and gate timing
- Note sequence construction with octave stacking and directional patterns
- Simple xorshift RNG for the random pattern

## Parameters

| Name    | Range              | Unit | Description                                   |
|---------|--------------------|------|-----------------------------------------------|
| Rate    | 1 -- 8 (discrete)  | --   | Note divisions: 1=whole, 4=quarter, 8=eighth  |
| Octaves | 1 -- 4 (discrete)  | --   | How many octaves to stack above held notes     |
| Pattern | Up/Down/UpDown/Rnd | --   | Arp direction                                 |
| Gate    | 0.1 -- 1.0         | %    | Note length as fraction of step duration       |

## Code structure

- `held_notes: Vec<u8>` -- tracks currently pressed input notes
- `build_sequence` -- sorts held notes, stacks octaves, applies pattern direction (reverse for Down, mirror for UpDown)
- `process` -- per-sample loop counts samples since last trigger; fires NoteOn at step boundaries, NoteOff at gate cutoff
- Falls back to 120 BPM when host transport tempo is unavailable

## Build and test

```bash
cargo build -p arpeggio
cargo test -p arpeggio
```
