# truce

Audio plugin framework — write once, build VST3, CLAP, AU, AAX.

`truce` is the main entry point for plugin authors. It re-exports the core
types, parameter system, and derive macros so you only need a single dependency
in your plugin crate.

## Usage

```toml
[dependencies]
truce = { version = "0.1", features = ["clap"] }
```

```rust
use truce::prelude::*;
```

## Features

| Feature | Description |
|---------|-------------|
| `clap` (default) | Enable CLAP format export |
| `vst3` | Enable VST3 format export |
| `dev` | Hot-reload support for development |
| `gpu` | GPU-accelerated GUI rendering |

Part of [truce](https://github.com/truce-audio/truce).
