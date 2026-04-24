# Arpeggio

Tempo-synced arpeggiator that sequences held notes in configurable patterns.

## What it demonstrates

- Tempo-synced MIDI processing using `context.transport.tempo`
- Enum parameter (`EnumParam<ArpPattern>`) with dropdown widget
- Stateful MIDI effect: tracks held notes, manages step counter and gate timing
- Octave stacking and directional patterns (Up, Down, Up/Down, Random)

## Parameters

| Name | Range | Unit | Description |
|------|-------|------|-------------|
| Rate | 1 to 8 | -- | Note divisions (1=whole, 4=quarter, 8=eighth) |
| Gate | 0.1 to 1.0 | % | Note length as fraction of step |
| Octaves | 1 to 4 | -- | Octaves to stack above held notes |
| Pattern | Up/Down/Up-Down/Random | -- | Arp direction |

## Build and test

```bash
cargo build -p truce-example-arpeggio
cargo test -p truce-example-arpeggio
```
