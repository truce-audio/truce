# truce

Main entry point for the truce audio plugin framework.

## Overview

`truce` is the only dependency most plugin authors need. It re-exports
`truce-core` (traits and types), `truce-params` (parameter system), and the
proc macros from `truce-derive` (all four - `Params`, `ParamEnum`, `State`,
and `plugin_info!()`), giving you a single import path for everything.

## Key re-exports

- `Plugin`, `PluginExport`, `AudioBuffer`, `Editor` from truce-core
- `FloatParam`, `IntParam`, `BoolParam`, `EnumParam`, `Smoother` from truce-params
- `FloatParamReadF32` / `FloatParamReadF64` extension traits, bringing `param.read()` into scope at the prelude's precision
- `#[derive(Params)]`, `#[derive(ParamEnum)]`, `#[derive(State)]` from truce-derive (at the crate root); `plugin_info!()` is available via the preludes
- `PluginLogic` from truce-plugin (the user-facing leaf trait. `PluginLogic` for `f32`, `PluginLogic64` for `f64`; the prelude aliases the right one as `PluginLogic`)
- The `truce::plugin!` macro generates all the format-export glue from one declaration

## Preludes

Four flavours, each pinning a different precision combination:

| Prelude | Audio buffer | `param.read()` returns | When to pick |
|---|---|---|---|
| `prelude` / `prelude32` | `f32` | `f32` | Default - host wire is `f32` everywhere |
| `prelude64m` | `f32` | `f64` | Stable `f64` intermediate math, narrow on buffer write |
| `prelude64` | `f64` | `f64` | Wrapper widens host `f32` -> plugin `f64` once per block |

Each prelude also defines `pub type AudioBuffer<'a, S = Sample> = ...`,
so `&mut AudioBuffer` resolves to the prelude's chosen precision
(and `&mut AudioBuffer<f32>` still works as an explicit override).

## Features

| Feature | Description |
|---------|-------------|
| `clap` (default) | Enable CLAP format export |
| `vst3` | Enable VST3 format export |
| `vst2` | Enable VST2 format export (legacy - see `Cargo.toml` note) |
| `lv2` | Enable LV2 format export |
| `shell` | Build a dynamic shell that dlopens a hot-reloadable logic dylib (turns on `truce-loader/shell`) |
| `hot-debug` | Verbose hot-reload diagnostics |

AU and AAX live in their own optional `truce-au` / `truce-aax` deps
(macOS-only AU; macOS/Windows AAX with the SDK + PACE wraptool). User
plugins gate them behind their own `au` / `aax` features rather than
through the facade. See `examples/truce-example-gain/Cargo.toml` for
the conventional pattern.

## Usage

```toml
[dependencies]
truce = { version = "0.49", features = ["clap"] }
```

(Cargo's caret resolver expands `"0.49"` to `>=0.49.0, <0.50.0`, so
you'll pick up every `0.49.x` patch release without re-editing. To
track an unreleased checkout, swap the line for
`truce = { git = "https://github.com/truce-audio/truce", branch = "main", features = ["clap"] }`.
Or just run `cargo truce new` and let the scaffolder write the
right pin for you.)

```rust
use truce::prelude::*;
// Built-in widget toolkit. `GridLayout` + the `knob` / `widgets` /
// `meter` / `xy_pad` constructors live in `truce-gui-types::layout`;
// `IntoLayoutEditor` is the trait that turns a `GridLayout` into a
// `Box<dyn Editor>` via the default renderer (`truce-cpu`).
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, widgets};

// Alias the derive-generated param-id enum so widget constructors
// can use `P::Gain` instead of `MyParamsParamId::Gain`.
use MyParamsParamId as P;

#[derive(Params)]
pub struct MyParams {
    #[param(name = "Gain", range = "linear(-60, 6)", unit = "dB")]
    pub gain: FloatParam,
}

pub struct MyPlugin;

impl PluginLogic for MyPlugin {
    type Params = MyParams;
    type DspState = ();

    fn init(_params: &MyParams) -> Self::DspState {}

    fn process(
        _state: &mut Self::DspState,
        params: &MyParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        // ...
        ProcessStatus::Normal
    }

    fn editor(params: Arc<MyParams>) -> Box<dyn Editor> {
        // Built-in widgets from a GridLayout. See the GUI guide for
        // framework backends (egui / iced / slint) and custom editors.
        GridLayout::build(vec![widgets(vec![knob(P::Gain, "Gain")])])
            .into_editor(&params)
    }
}

truce::plugin! { logic: MyPlugin, params: MyParams }
```

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
