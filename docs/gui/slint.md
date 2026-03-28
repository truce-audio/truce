# Slint Integration

[Slint](https://github.com/slint-ui/slint) is a declarative GUI toolkit
with its own `.slint` markup language. You design your UI visually in
`.slint` files (with IDE live preview), and Slint compiles them to
efficient Rust code at build time. `truce-slint` wraps it as a truce
editor using Slint's software renderer + wgpu for display.

## When to use Slint

Pick Slint when you want:

- Declarative UI in `.slint` markup (no imperative widget code)
- IDE live preview via the Slint VS Code extension
- Reactive property bindings (change a value, UI updates automatically)
- Self-contained rendering (no GTK/Qt/Cocoa dependencies)

## Setup

Add these to your plugin's `Cargo.toml`:

```toml
[dependencies]
truce-slint = { workspace = true }
slint = { version = "=1.15.1", default-features = false, features = ["compat-1-2", "renderer-software", "std"] }

[build-dependencies]
slint-build = "=1.15.1"
```

Create a `build.rs` in your plugin's root:

```rust
fn main() {
    slint_build::compile("ui/main.slint").expect("failed to compile .slint UI");
}
```

## Quick start

### 1. Define your UI in `.slint`

Create `ui/main.slint`:

```slint
import { Knob, Meter, XYPad } from "@truce";
import { HorizontalBox } from "std-widgets.slint";

export component MyPluginUi inherits Window {
    in-out property <float> gain: 0.5;
    in-out property <float> pan: 0.5;
    in-out property <float> meter-left: 0.0;
    in-out property <float> meter-right: 0.0;

    callback gain-changed(float);
    callback pan-changed(float);

    preferred-width: 200px;
    preferred-height: 150px;
    background: #1f1f24;

    VerticalLayout {
        padding: 10px;
        spacing: 10px;

        HorizontalBox {
            spacing: 10px;

            Knob {
                label: "Gain";
                value <=> root.gain;
                changed(v) => { root.gain-changed(v); }
            }

            Knob {
                label: "Pan";
                value <=> root.pan;
                changed(v) => { root.pan-changed(v); }
            }

            Meter {
                level-left: root.meter-left;
                level-right: root.meter-right;
            }
        }
    }
}
```

### 2. Wire it up in Rust

```rust
use truce::prelude::*;
use truce_slint::{SlintEditor, ParamState};
use truce_core::meter_display;

slint::include_modules!();

use MyParamsParamId as P;

impl PluginLogic for MyPlugin {
    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        Some(Box::new(SlintEditor::new((200, 150), |state: ParamState| {
            let ui = MyPluginUi::new().unwrap();

            // UI -> host (user drags a knob)
            let s = state.clone();
            ui.on_gain_changed(move |v| s.set_immediate(P::Gain, v as f64));
            let s = state.clone();
            ui.on_pan_changed(move |v| s.set_immediate(P::Pan, v as f64));

            // host -> UI (sync every frame)
            Box::new(move |state: &ParamState| {
                ui.set_gain(state.get(P::Gain) as f32);
                ui.set_pan(state.get(P::Pan) as f32);
                ui.set_meter_left(meter_display(state.meter(P::MeterLeft)));
                ui.set_meter_right(meter_display(state.meter(P::MeterRight)));
            })
        })))
    }
}
```

The `(200, 150)` is the window size in logical points. The closure
receives a `ParamState` and returns a sync function that runs every
frame to push host values into the Slint UI.

## The `bind!` macro

For simple float and bool params, `bind!` eliminates the callback
boilerplate:

```rust
let ui = MyPluginUi::new().unwrap();
truce_slint::bind! { state, ui,
    P::Gain   => gain,            // float (default)
    P::Pan    => pan,             // float
    P::Bypass => bypass: bool,    // boolean
    P::Mode   => mode: choice(4), // enum with 4 options (ComboBox index)
}
```

This generates:
1. `ui.on_gain_changed(...)`, `ui.on_pan_changed(...)`, etc. — callbacks from Slint to the host
2. A returned sync closure that calls `ui.set_gain(...)`, `ui.set_pan(...)`, etc. each frame

The macro relies on Slint's naming convention: a property named `foo`
generates `set_foo()` and `on_foo_changed()`.

### Mixing manual wiring with `bind!`

For params needing custom conversion, wire them manually before `bind!`:

```rust
let s = state.clone();
ui.on_freq_changed(move |hz| {
    let norm = (hz.log2() - 20f32.log2()) / (20000f32.log2() - 20f32.log2());
    s.set_immediate(P::Freq, norm as f64);
});

// bind! must come last since it returns the sync closure
truce_slint::bind! { state, ui,
    P::Gain => gain,
}
```

## Formatted value text

To show formatted values (like "0.0 dB" instead of "0.50"), add string
properties to your `.slint` file and sync them from Rust:

```slint
in-out property <string> gain-text: "";

Knob {
    value <=> root.gain;
    value-text: root.gain-text;  // shows formatted text instead of raw number
}
```

```rust
// In your sync closure:
ui.set_gain_text(slint::SharedString::from(state.format(P::Gain)));
```

## Truce widget library

`truce-slint` provides pre-built `.slint` components you can import:

```slint
import { Knob, Meter, XYPad, ParamSlider, Toggle, Selector } from "@truce";
```

| Widget | What it does |
|--------|-------------|
| `Knob` | 270-degree rotary control with arc, pointer, value text, label |
| `Meter` | Dual-channel vertical level meter |
| `XYPad` | 2D drag pad for two parameters |
| `ParamSlider` | Horizontal slider |
| `Toggle` | On/off switch |
| `Selector` | Click-to-cycle for enum params |

You can also use any of Slint's built-in widgets (`Slider`, `Switch`,
`ComboBox`, etc.) from `"std-widgets.slint"` and wire them manually.

## ParamState

Same API as the egui backend, but `Clone`-able so Slint callbacks can
capture it:

| Method | Description |
|--------|-------------|
| `state.get(id)` | Normalized value (0.0-1.0) |
| `state.get_plain(id)` | Plain value (native range) |
| `state.format(id)` | Formatted display string |
| `state.meter(id)` | Meter level (0.0-1.0) |
| `state.set_immediate(id, v)` | One-shot value change |
| `state.begin_gesture(id)` | Start drag gesture |
| `state.set_value(id, v)` | Update during drag |
| `state.end_gesture(id)` | End drag gesture |

## Screenshot testing

Slint snapshots use the software renderer — no GPU or window needed,
making them fast and deterministic:

```rust
#[test]
fn gui_snapshot() {
    truce_slint::snapshot::assert_snapshot(
        "screenshots",
        "my_plugin_slint_default",
        WINDOW_W, WINDOW_H,     // same constants as your editor
        2.0,                     // scale (2.0 for Retina)
        0,                       // max pixel diff (0 = exact)
        |state| {
            let ui = MyPluginUi::new().unwrap();
            truce_slint::bind! { state, ui,
                P::Gain => gain,
            }
        },
    );
}
```

See [screenshot testing](screenshot-testing.md) for details.

## Architecture

- **Rendering**: Slint SoftwareRenderer -> RGBA pixels -> wgpu texture -> screen
- **Windowing**: baseview child window (cross-platform)
- **Event loop**: baseview `on_frame()` drives rendering at ~60fps

## Licensing

Slint offers a royalty-free license for proprietary desktop applications,
which covers audio plugins. See [slint.dev](https://slint.dev) for
current terms.

## Complete example

See `examples/gain-slint/` for a working plugin with custom knobs,
XY pad, level meter, formatted value text, and screenshot test.
