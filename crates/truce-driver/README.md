# truce-driver

Headless driver for truce plugins.

## Overview

Instantiate a plugin, feed it scripted audio + events for a fixed
duration, capture the output - no DAW, no format wrapper, no GUI.
Three consumers in-tree:

- **[`truce-test`](../truce-test)** - assertion helpers
  (`assert_state_round_trip`, `assert_nonzero`, the `driver!`
  macro) layered on top of the captured `DriverResult`.
- **`truce-standalone`'s offline-render path** - `cargo truce run
  --no-playback` parses CLI flags into an `InputSource::Buffer` +
  `Script`, runs `PluginDriver`, writes the captured audio out
  as WAV.
- **Plugin authors writing custom `main.rs` binaries** - batch
  CI renders, demo audio generation, preset-rendering pipelines.

## Key types

- **`PluginDriver<P>`** - the builder. Carries sample rate, block
  size, channel count, the input source, the script, the
  transport state, and the capture spec.
- **`InputSource`** - `Silence` / `Constant` / `Buffer(Vec<Vec<f32>>)` /
  `Generator(Box<dyn FnMut(usize, f64) -> f32>)`.
- **`Script`** - sample-accurate sequence of `EventBody`s with a
  cursor (`note_on`, `note_off`, `set_param`, `wait_ms`, …).
- **`CaptureSpec`** - what to capture per block (audio, output
  events, meters, param snapshots).
- **`DriverResult`** - the post-run capture, indexable by channel.

## Precision

`PluginDriver` routes through `RawBufferScratch<P::Sample>` - the
same widening/narrowing path the format wrappers use - so
`prelude64` plugins can be driven in headless tests with the
wrapper-boundary conversion done internally. The driver's public
input / output buffers are always `f32` (host-wire); the plugin's
`process()` sees `AudioBuffer<P::Sample>`.

## Example

```rust
use std::time::Duration;
use truce_driver::{InputSource, PluginDriver};

let result = PluginDriver::<MyPlugin>::new()
    .sample_rate(48_000.0)
    .duration(Duration::from_secs(2))
    .input(InputSource::Constant(0.5))
    .set_param(MyParamId::Gain, 0.7)
    .script(|s| {
        s.note_on(60, 0.8);
        s.wait_ms(500);
        s.note_off(60);
    })
    .run();
```

Part of [truce](https://github.com/truce-audio/truce).
