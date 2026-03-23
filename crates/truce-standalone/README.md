# truce-standalone

Standalone host wrapper for truce plugins.

Runs a truce plugin outside of a DAW with audio output via cpal and QWERTY
keyboard-to-MIDI mapping. Useful for quick testing during development.

## Features

| Feature | Description |
|---------|-------------|
| `gui` | Open a window with rendered parameter knobs via minifb |

## Usage

```rust
truce_standalone::run::<MyPlugin>();
```
