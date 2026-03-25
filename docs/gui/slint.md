# Slint Integration

[Slint](https://github.com/slint-ui/slint) is a declarative GUI toolkit
with its own `.slint` markup language compiled at build time. `truce-slint`
wraps it as a truce `Editor` using Slint's software renderer, baseview
for windowing, and wgpu for screen presentation.

## When to Use Slint

Use slint when you want:

- Declarative UI in `.slint` markup (compiled to Rust at build time)
- IDE live preview (Slint VS Code extension)
- Self-contained rendering (no GTK/Qt/Cocoa dependency)
- Reactive property bindings

## Setup

Add `truce-slint` and `slint` to your plugin's `Cargo.toml`:

```toml
[dependencies]
truce-slint = { workspace = true }
slint = { version = "=1.15.1", default-features = false, features = ["compat-1-2", "renderer-software", "std"] }

[build-dependencies]
slint-build = "=1.15.1"
```

Create a `build.rs`:

```rust
fn main() {
    slint_build::compile("ui/main.slint").expect("failed to compile .slint UI");
}
```

## Quick Start

Define your UI in a `.slint` file:

```slint
// ui/main.slint
import { Slider, Switch, VerticalLayout, HorizontalBox } from "std-widgets.slint";

export component MyPluginUi inherits Window {
    in-out property <float> gain: 0.5;
    in-out property <bool> bypass: false;
    callback gain-changed(float);
    callback bypass-changed(bool);

    VerticalLayout {
        Slider {
            value <=> root.gain;
            changed(v) => { root.gain-changed(v); }
        }
        Switch {
            checked <=> root.bypass;
            toggled => { root.bypass-changed(root.bypass); }
        }
    }
}
```

Wire it up in Rust:

```rust
use truce::prelude::*;
use truce_slint::{SlintEditor, ParamState};

slint::include_modules!();

use MyParamsParamId as P;

impl PluginLogic for MyPlugin {
    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        Some(Box::new(SlintEditor::new((400, 300), |state: ParamState| {
            let ui = MyPluginUi::new().unwrap();
            truce_slint::bind! { state, ui,
                P::Gain   => gain,
                P::Bypass => bypass: bool,
            }
        })))
    }
}
```

## The `bind!` Macro

`bind!` eliminates boilerplate by generating both callback wiring
(UI → host) and the per-frame sync closure (host → UI):

```rust
truce_slint::bind! { state, ui,
    P::Gain   => gain,            // float param (default)
    P::Pan    => pan,             // float param
    P::Bypass => bypass: bool,    // boolean param
}
```

This expands to:
1. `ui.on_gain_changed(...)` / `ui.on_pan_changed(...)` / `ui.on_bypass_changed(...)` — wiring Slint callbacks to `state.set_immediate()`
2. A returned `Box<dyn Fn(&ParamState)>` that calls `ui.set_gain(...)` / `ui.set_pan(...)` / `ui.set_bypass(...)` each frame

The macro relies on Slint's naming convention: property `foo` generates
`set_foo()` and `on_foo_changed()`. Annotate boolean params with `: bool`.

For params needing custom conversion (log scales, enums), wire them
manually alongside the macro:

```rust
let s = state.clone();
ui.on_freq_changed(move |hz| {
    let norm = (hz.log2() - 20f32.log2()) / (20000f32.log2() - 20f32.log2());
    s.set_immediate(P::Freq, norm as f64);
});

// Bind the rest with the macro
truce_slint::bind! { state, ui,
    P::Gain   => gain,
    P::Bypass => bypass: bool,
}
```

Note: when mixing manual wiring with `bind!`, the macro must come last
since it returns the sync closure (consuming `ui`).

## ParamState

Same API as `truce-egui`'s `ParamState`, but `Clone`-able so Slint
callbacks can capture it:

| Method | Description |
|--------|-------------|
| `state.get(id)` | Normalized value (0.0–1.0) |
| `state.get_plain(id)` | Plain value (native range) |
| `state.format(id)` | Formatted display string |
| `state.meter(id)` | Meter level (0.0–1.0) |
| `state.set_immediate(id, v)` | One-shot value change |
| `state.begin_gesture(id)` | Start drag gesture |
| `state.set_value(id, v)` | Update during drag |
| `state.end_gesture(id)` | End drag gesture |

## Custom Widgets in `.slint`

Slint doesn't have built-in knob or meter widgets. Define them in
`.slint` markup using `Path` for arcs and `TouchArea` for interaction.
See the gain-slint example for a complete `Knob` component with:
- 270° arc track and value indicator
- Pointer dot
- Vertical drag interaction
- Label and value text

Key tip: set `viewbox-width` and `viewbox-height` on Path elements to
prevent auto-fit scaling from distorting arc geometry.

## Snapshot Testing

`truce-slint` includes headless snapshot rendering using the software
renderer (no GPU or window needed):

```rust
#[test]
fn gui_snapshot() {
    truce_slint::snapshot::assert_snapshot(
        "screenshots",          // directory
        "my_plugin_default",    // name
        400, 300,               // logical size
        2.0,                    // scale (2.0 for Retina)
        0,                      // max pixel diff
        |state| {
            let ui = MyPluginUi::new().unwrap();
            truce_slint::bind! { state, ui,
                P::Gain => gain,
            }
        },
    );
}
```

## Architecture

- **Rendering**: Slint SoftwareRenderer → RGBA pixel buffer → wgpu texture → screen
- **Windowing**: baseview child window (cross-platform)
- **Platform**: Custom `slint::platform::Platform` impl set once per process via `set_platform()`
- **Event loop**: baseview's `on_frame()` drives rendering at ~60fps

## Toolchain

Slint 1.15 requires Rust >= 1.88. If using Homebrew Rust (which may be
newer), install rustup and pin to a compatible version.

## Licensing

Slint offers a royalty-free license for proprietary desktop applications
(which covers audio plugins). See [slint.dev](https://slint.dev) for
current terms.

## Example

See `crates/truce-slint/examples/gain-slint/` for a complete working
example with custom knob widgets, bypass switch, and snapshot test.
