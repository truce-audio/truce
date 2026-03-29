# truce-standalone

Standalone host wrapper for truce plugins.

## Overview

Runs a truce plugin outside of a DAW using cpal for real-time audio I/O.
Useful for quick testing and prototyping during development without launching a
full DAW. For instrument plugins, QWERTY keyboard keys are mapped to MIDI
notes so you can play sounds immediately.

## Features

| Feature | Description |
|---------|-------------|
| `gui` | Open a window with rendered parameter knobs via minifb |

## Key function

- **`truce_standalone::run::<P>()`** -- launches the standalone host for plugin type `P`

## Usage

```rust
fn main() {
    truce_standalone::run::<MyPlugin>();
}
```

This opens an audio stream on the default output device, optionally displays a
GUI window (with the `gui` feature), and processes audio until the window is
closed or the process is terminated.

Part of [truce](https://github.com/truce-audio/truce).
