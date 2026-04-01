# Slint Integration

[Slint](https://github.com/slint-ui/slint) is a declarative GUI toolkit.
You write your UI in `.slint` markup files, Slint compiles them to Rust
at build time, and `truce-slint` handles the window embedding and
parameter communication.

## Getting started

Add the dependencies:

```toml
[dependencies]
truce-slint = { workspace = true }
slint = { version = "=1.15.1", default-features = false, features = ["compat-1-2", "renderer-software", "std"] }

[build-dependencies]
slint-build = "=1.15.1"
```

Create `build.rs`:

```rust
fn main() {
    slint_build::compile("ui/main.slint").expect("failed to compile .slint UI");
}
```

## A simple plugin UI

Create `ui/main.slint`:

```slint
import { Knob, Meter } from "@truce";
import { HorizontalBox } from "std-widgets.slint";

export component MyPluginUi inherits Window {
    in-out property <float> gain: 0.5;
    in-out property <float> meter-left: 0.0;
    in-out property <float> meter-right: 0.0;
    callback gain-changed(float);

    preferred-width: 200px;
    preferred-height: 120px;
    background: #1f1f24;

    HorizontalBox {
        padding: 10px;
        spacing: 10px;

        Knob {
            label: "Gain";
            value <=> root.gain;
            changed(v) => { root.gain-changed(v); }
        }

        Meter {
            level-left: root.meter-left;
            level-right: root.meter-right;
        }
    }
}
```

Wire it up in Rust:

```rust
use truce::prelude::*;
use truce_slint::{SlintEditor, ParamState};
use truce_core::meter_display;
slint::include_modules!();
use MyParamsParamId as P;

impl PluginLogic for MyPlugin {
    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        Some(Box::new(SlintEditor::new((200, 120), |state: ParamState| {
            let ui = MyPluginUi::new().unwrap();

            // UI -> host: when the user drags the knob
            let s = state.clone();
            ui.on_gain_changed(move |v| s.set_immediate(P::Gain, v as f64));

            // host -> UI: sync every frame
            Box::new(move |state: &ParamState| {
                ui.set_gain(state.get(P::Gain) as f32);
                ui.set_meter_left(meter_display(state.meter(P::MeterLeft)));
                ui.set_meter_right(meter_display(state.meter(P::MeterRight)));
            })
        })))
    }
}
```

The closure receives a `ParamState` and returns a sync function. Slint
calls the sync function every frame (~60fps) to push host values into
the UI.

## Adding more parameters

For each parameter, you need three things:

1. A property in the `.slint` file (`in-out property <float> pan: 0.5;`)
2. A callback in the `.slint` file (`callback pan-changed(float);`)
3. Wiring in Rust (callback + sync)

The `bind!` macro handles steps 2-3 for simple float and bool params:

```rust
let ui = MyPluginUi::new().unwrap();
truce_slint::bind! { state, ui,
    P::Gain   => gain,           // float
    P::Pan    => pan,            // float
    P::Bypass => bypass: bool,   // boolean
    P::Mode   => mode: choice(4), // enum with 4 options
}
```

This generates all the `on_*_changed` callbacks and the sync closure.
You still need the properties and callbacks in your `.slint` file.

## Showing formatted values

By default, the `Knob` widget shows the raw normalized value. To show
formatted text like "0.0 dB", add a string property:

```slint
in-out property <string> gain-text: "";

Knob {
    value <=> root.gain;
    value-text: root.gain-text;
}
```

```rust
// In the sync closure:
ui.set_gain_text(slint::SharedString::from(state.format(P::Gain)));
```

## Available truce widgets

Import from `"@truce"`:

```slint
import { Knob, Meter, XYPad, ParamSlider, Toggle, Selector } from "@truce";
```

- **Knob** — 270-degree rotary control with arc, pointer, label
- **Meter** — dual-channel vertical level meter
- **XYPad** — 2D drag pad for two parameters
- **ParamSlider** — horizontal slider
- **Toggle** — on/off switch
- **Selector** — click-to-cycle for enum params

You can also use Slint's built-in widgets (`Slider`, `Switch`,
`ComboBox`) from `"std-widgets.slint"` and wire them manually.

## Mixing manual wiring with `bind!`

For parameters that need custom conversion (log scales, enums), wire
them before the `bind!` macro:

```rust
let s = state.clone();
ui.on_freq_changed(move |hz| {
    let norm = (hz.log2() - 20f32.log2()) / (20000f32.log2() - 20f32.log2());
    s.set_immediate(P::Freq, norm as f64);
});

// bind! must come last — it returns the sync closure
truce_slint::bind! { state, ui,
    P::Gain => gain,
}
```

## ParamState

Same as the other backends:

```rust
state.get(P::Gain)           // normalized 0.0-1.0
state.get_plain(P::Gain)     // plain value
state.format(P::Gain)        // formatted string
state.meter(P::MeterLeft)    // meter level
state.set_immediate(P::Gain, v)  // write (one shot)
state.begin_gesture(P::Gain)     // write (start drag)
state.set_value(P::Gain, v)      // write (during drag)
state.end_gesture(P::Gain)       // write (end drag)
```

`ParamState` is `Clone`-able, so Slint callbacks can capture copies.

## Custom state

If your plugin has persistent state beyond parameters, check for
changes in the sync callback:

```rust
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(State, Default)]
pub struct MyState {
    pub instance_name: String,
}

let state_dirty = Arc::new(AtomicBool::new(false));

// In the sync closure:
if state_dirty.swap(false, Ordering::Relaxed) {
    // Re-read custom state from plugin
    let data = (state.context().get_state)();
    if let Some(s) = MyState::deserialize(&data) {
        ui.set_instance_name(s.instance_name.into());
    }
}
```

Set the flag from `Editor::state_changed()` using a thin wrapper (see
the [state persistence](../reference/10-state.md) guide for details).

If your plugin only uses `#[param]` fields, you don't need any of this —
parameter values sync automatically through `ParamState`.

## Screenshot testing

Slint snapshots use the software renderer — no GPU needed:

```rust
#[test]
fn gui_snapshot() {
    truce_slint::snapshot::assert_snapshot(
        "screenshots", "my_plugin_slint_default",
        WINDOW_W, WINDOW_H, 2.0, 0,
        |state| {
            let ui = MyPluginUi::new().unwrap();
            truce_slint::bind! { state, ui, P::Gain => gain }
        },
    );
}
```

See [screenshot testing](screenshot-testing.md) for more.

## Licensing

Slint has a royalty-free license for proprietary desktop applications,
which covers audio plugins. See [slint.dev](https://slint.dev) for
terms.

## Example

`examples/gain-slint/` has a complete plugin with knobs, XY pad, meter,
formatted values, and screenshot test.
