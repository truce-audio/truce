# egui Integration

[egui](https://github.com/emilk/egui) is an immediate-mode GUI library
with a huge widget ecosystem. `truce-egui` wraps it as a truce editor,
so you get access to all of egui's widgets, layout system, and
third-party crates while truce handles the window embedding and parameter
automation.

## When to use egui

Pick egui when you need more than the built-in GUI offers:

- Custom layouts (tabs, scrolling, collapsible panels)
- Text input fields
- Graphs, tables, color pickers
- Any widget from the egui ecosystem

## Setup

Add these to your plugin's `Cargo.toml`:

```toml
[dependencies]
truce-egui = { workspace = true }
egui = "0.31"
```

## Quick start

Override `custom_editor()` to return an `EguiEditor`:

```rust
use truce::prelude::*;
use truce_egui::{EguiEditor, ParamState};
use truce_egui::widgets::param_knob;

use MyParamsParamId as P;

impl PluginLogic for MyPlugin {
    // ... reset(), process() stay the same ...

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

The `(400, 300)` is the window size in logical points. The closure runs
every frame (~60fps) — just like a normal egui app.

Everything else about your plugin (params, DSP, `truce::plugin!`) stays
exactly the same. The only change is adding `custom_editor()`.

## Reading and writing parameters

`ParamState` is your bridge between egui widgets and the DAW:

```rust
// Read
let gain = state.get(P::Gain);           // normalized 0.0-1.0
let gain_db = state.get_plain(P::Gain);  // plain value (e.g., -60.0 to 6.0)
let text = state.format(P::Gain);        // formatted string ("0.0 dB")
let level = state.meter(P::MeterLeft);   // meter level 0.0-1.0

// Write — click/toggle (one shot)
state.set_immediate(P::Gain, 0.75);

// Write — drag (gesture protocol for smooth automation)
state.begin_gesture(P::Gain);
state.set_value(P::Gain, new_value);   // call repeatedly during drag
state.end_gesture(P::Gain);
```

Use `set_immediate()` for toggles and selectors (single-click actions).
Use the `begin_gesture` / `set_value` / `end_gesture` sequence for
continuous drags (knobs, sliders, XY pads) so the DAW records smooth
automation.

## Helper widgets

`truce-egui` ships widgets that handle the gesture protocol for you:

```rust
use truce_egui::widgets::{
    param_knob,      // Rotary knob (vertical drag)
    param_slider,    // Horizontal slider
    param_toggle,    // On/off switch
    param_selector,  // Cycling button for enums
    param_xy_pad,    // 2D pad controlling two params
    level_meter,     // Vertical meter bars (display-only)
};
```

Example layout:

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

These are convenience wrappers. You can always use raw egui widgets and
call `ParamState` directly:

```rust
let mut value = state.get(P::Gain) as f32;
let response = ui.add(egui::Slider::new(&mut value, 0.0..=1.0));
if response.drag_started() { state.begin_gesture(P::Gain); }
if response.changed()      { state.set_value(P::Gain, value as f64); }
if response.drag_stopped() { state.end_gesture(P::Gain); }
```

## Theme colors

`truce-egui` exports color constants matching the built-in dark theme:

```rust
use truce_egui::theme::{BACKGROUND, SURFACE, PRIMARY, TEXT, TEXT_DIM,
                         HEADER_BG, HEADER_TEXT, KNOB_FILL, METER_CLIP};
```

Apply the dark theme with `.with_visuals()`:

```rust
EguiEditor::new((400, 300), my_ui)
    .with_visuals(truce_egui::theme::dark())
```

If omitted, `dark()` is applied automatically. Pass your own
`egui::Visuals` to customize.

## Custom fonts

Embed a font at compile time:

```rust
EguiEditor::new((400, 300), my_ui)
    .with_font(truce_gui::font::JETBRAINS_MONO)
```

## Stateful UIs

For complex UIs with internal state (tab selection, animation), implement
`EditorUi` instead of using a closure:

```rust
use truce_egui::{EditorUi, ParamState};

struct MyEditorUi {
    selected_tab: usize,
}

impl EditorUi for MyEditorUi {
    fn ui(&mut self, ctx: &egui::Context, state: &ParamState) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.selected_tab, 0, "Controls");
                ui.selectable_value(&mut self.selected_tab, 1, "Settings");
            });
            match self.selected_tab {
                0 => { /* controls */ }
                1 => { /* settings */ }
                _ => {}
            }
        });
    }
}

// In custom_editor():
EguiEditor::with_ui((640, 480), MyEditorUi { selected_tab: 0 })
```

## Screenshot testing

Render your UI headlessly and compare against a reference PNG:

```rust
#[test]
fn gui_snapshot() {
    truce_egui::snapshot::assert_snapshot(
        "screenshots",           // directory
        "my_plugin_default",     // snapshot name
        WINDOW_W, WINDOW_H,     // same constants as your editor
        2.0,                     // scale (2.0 for Retina)
        0,                       // max pixel differences (0 = exact)
        Some(truce_gui::font::JETBRAINS_MONO),  // font (match your editor)
        |ctx, state| my_ui(ctx, state),
    );
}
```

First run creates the reference. Subsequent runs compare pixel-by-pixel.
Delete the PNG to regenerate. See [screenshot testing](screenshot-testing.md)
for details.

## Complete example

See `examples/gain-egui/` for a working plugin with knobs, XY pad,
level meter, custom header, themed colors, and screenshot test.
