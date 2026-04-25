# truce

Main entry point for the truce audio plugin framework.

## Overview

`truce` is the only dependency most plugin authors need. It re-exports
`truce-core` (traits and types), `truce-params` (parameter system), and the
derive macros from `truce-derive` and `truce-params-derive`, giving you a
single import path for everything.

## Key re-exports

- `Plugin`, `PluginExport`, `AudioBuffer`, `Editor` -- from truce-core
- `FloatParam`, `IntParam`, `BoolParam`, `EnumParam`, `Smoother` -- from truce-params
- `#[derive(Params)]`, `#[derive(ParamEnum)]` -- from truce-params-derive
- `plugin_info!()` -- from truce-derive

## Features

| Feature | Description |
|---------|-------------|
| `clap` (default) | Enable CLAP format export |
| `vst3` | Enable VST3 format export |
| `dev` | Hot-reload support for development |
| `gpu` | GPU-accelerated GUI rendering |

## Usage

```toml
[dependencies]
truce = { git = "https://github.com/truce-audio/truce", features = ["clap"] }
```

```rust
use truce::prelude::*;

pub struct MyPlugin {
    params: Arc<MyParams>,
}

impl PluginLogic for MyPlugin {
    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.params.set_sample_rate(sample_rate);
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        // ...
        ProcessStatus::Normal
    }
}

truce::plugin! { logic: MyPlugin, params: MyParams }
```

Part of [truce](https://github.com/truce-audio/truce).
