# egui Integration

[egui](https://github.com/emilk/egui) is an immediate-mode GUI library.
`truce-egui` wraps it so you can use egui's widgets and layout system
inside a plugin window.

## Getting started

Add the dependencies:

```toml
[dependencies]
truce-egui = { workspace = true }
egui = "0.31"
```

Override `custom_editor()` to return an `EguiEditor`:

```rust
use truce::prelude::*;
use truce_egui::{EguiEditor, ParamState};
use truce_egui::widgets::param_knob;
use MyParamsParamId as P;

impl PluginLogic for MyPlugin {
    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        Some(Box::new(
            EguiEditor::new((400, 300), |ctx: &egui::Context, state: &ParamState| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.heading("My Plugin");
                    param_knob(ui, state, P::Gain, "Gain");
                });
            })
        ))
    }
}
```

The closure runs every frame. `(400, 300)` is the window size in logical
points. Everything else about your plugin stays the same.

## Reading parameter values

`ParamState` gives you access to parameter values from the host:

```rust
let gain = state.get(P::Gain);         // normalized 0.0-1.0
let gain_db = state.get_plain(P::Gain); // plain value, e.g. -60.0
let text = state.format(P::Gain);       // formatted, e.g. "0.0 dB"
let level = state.meter(P::MeterLeft);  // meter level 0.0-1.0
```

## Writing parameter values

For click actions (toggles, selectors):

```rust
state.set_immediate(P::Bypass, 1.0);
```

For continuous drags (knobs, sliders), wrap in a gesture so the DAW
records smooth automation:

```rust
state.begin_gesture(P::Gain);
state.set_value(P::Gain, new_value);  // call each frame during drag
state.end_gesture(P::Gain);
```

## Helper widgets

`truce-egui` provides widgets that handle the gesture protocol for you:

```rust
use truce_egui::widgets::{
    param_knob,     // rotary knob
    param_slider,   // horizontal slider
    param_toggle,   // on/off switch
    param_selector, // click-to-cycle for enums
    param_xy_pad,   // 2D pad for two params
    level_meter,    // vertical bar meter
};
```

A typical layout:

```rust
fn my_ui(ctx: &egui::Context, state: &ParamState) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.horizontal(|ui| {
            param_knob(ui, state, P::Gain, "Gain");
            param_knob(ui, state, P::Pan, "Pan");
            level_meter(ui, state, &[P::MeterLeft, P::MeterRight], 200.0);
        });
        param_xy_pad(ui, state, P::Pan, P::Gain, "Pan / Gain", 130.0, 130.0);
    });
}
```

You can also use raw egui widgets and call `ParamState` directly:

```rust
let mut value = state.get(P::Gain) as f32;
let response = ui.add(egui::Slider::new(&mut value, 0.0..=1.0));
if response.drag_started() { state.begin_gesture(P::Gain); }
if response.changed()      { state.set_value(P::Gain, value as f64); }
if response.drag_stopped() { state.end_gesture(P::Gain); }
```

## Theme and colors

The default dark theme is applied automatically. Customize with
`.with_visuals()`:

```rust
EguiEditor::new((400, 300), my_ui)
    .with_visuals(truce_egui::theme::dark())
    .with_font(truce_gui::font::JETBRAINS_MONO)
```

Standard colors are exported as constants:

```rust
use truce_egui::theme::{BACKGROUND, SURFACE, PRIMARY, TEXT, TEXT_DIM,
                         HEADER_BG, HEADER_TEXT, KNOB_FILL, METER_CLIP};
```

## Stateful UIs

For UIs with internal state (tab selection, caches), implement
`EditorUi` instead of passing a closure:

```rust
use truce_egui::{EditorUi, ParamState};

struct MyUi { tab: usize }

impl EditorUi for MyUi {
    fn ui(&mut self, ctx: &egui::Context, state: &ParamState) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, 0, "Controls");
                ui.selectable_value(&mut self.tab, 1, "Settings");
            });
            // draw based on self.tab
        });
    }
}

// In custom_editor():
EguiEditor::with_ui((640, 480), MyUi { tab: 0 })
```

## Custom state

If your plugin has persistent state beyond parameters (instance names,
view modes, selections), use `StateBinding<T>` with the `EditorUi`
lifecycle methods:

```rust
#[derive(State, Default)]
pub struct MyState {
    pub instance_name: String,
    pub view_mode: u8,
}

struct MyUi {
    state: StateBinding<MyState>,
}

impl EditorUi for MyUi {
    fn opened(&mut self, ps: &ParamState) {
        // Create the binding when the editor window opens
        self.state = StateBinding::new(ps.context());
    }

    fn ui(&mut self, ctx: &egui::Context, ps: &ParamState) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label(&self.state.get().instance_name);
        });
    }

    fn state_changed(&mut self, _ps: &ParamState) {
        // Re-read state after preset recall, undo, or session load
        self.state.sync();
    }
}
```

`EditorUi` has three lifecycle methods:

- **`opened()`** — called once when the editor window opens. Create
  `StateBinding` here.
- **`ui()`** — called every frame. Read state with `self.state.get()`.
- **`state_changed()`** — called when the DAW restores state. Call
  `sync()` to re-read.

To write state from the GUI (e.g., user renames an instance):

```rust
self.state.update(|s| s.instance_name = new_name);
```

For the closure API, use `.on_state_changed()`:

```rust
EguiEditor::new((400, 300), |ctx, state| { /* ui */ })
    .on_state_changed(|state| { /* re-read cached state */ })
```

If your plugin only uses `#[param]` fields, you don't need any of this —
parameter values sync automatically every frame.

## Screenshot testing

```rust
#[test]
fn gui_snapshot() {
    truce_egui::snapshot::assert_snapshot(
        "screenshots", "my_plugin_default",
        WINDOW_W, WINDOW_H, 2.0, 0,
        Some(truce_gui::font::JETBRAINS_MONO),
        |ctx, state| my_ui(ctx, state),
    );
}
```

Use the same `WINDOW_W` / `WINDOW_H` constants as your editor so they
stay in sync. See [screenshot testing](screenshot-testing.md) for more.

## Example

`examples/gain-egui/` has a complete plugin with knobs, XY pad, meter,
header, and screenshot test.
