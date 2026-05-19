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
| `gui` | Open a baseview window hosting the plugin's editor |
| `playback` | Add `--input-file` / `--output-file` / `--no-playback` for WAV-driven test and CI workflows (pulls in `hound`) |

## Key functions

- **`truce_standalone::run::<P>()`** - launches the standalone host for plugin type `P`
- **`truce_standalone::run_with::<P>(Defaults { … })`** - same, but with plugin-author launch defaults (CLI / env still take precedence)

## Usage

```rust
fn main() {
    truce_standalone::run::<MyPlugin>();
}
```

This opens an audio stream on the default output device, optionally displays a
GUI window (with the `gui` feature), and processes audio until the window is
closed or the process is terminated.

With `playback` enabled, the same binary doubles as a headless WAV-renderer:

```sh
# Real-time capture from the input file.
my-plugin-standalone --input-file in.wav --output-file out.wav

# Offline render - bypass cpal entirely, run as fast as the CPU allows.
my-plugin-standalone --no-playback --input-file in.wav --output-file out.wav
```

Part of [truce](https://github.com/truce-audio/truce).
