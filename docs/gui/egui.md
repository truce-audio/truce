# egui Integration

[egui](https://github.com/emilk/egui) is an immediate-mode GUI library.
`truce-egui` wraps it as a truce `Editor`, giving you access to egui's
full widget library, layout system, text input, and ecosystem while
retaining truce's parameter binding and host integration.

## When to Use egui

Use egui when you need capabilities beyond the built-in GUI:

- Custom layouts (tabs, scrolling, collapsible sections)
- Text input fields
- Dropdown menus, graphs, tables, color pickers
- Any third-party egui crate

## Setup

Add `truce-egui` and `egui` to your plugin's `Cargo.toml`:

```toml
[dependencies]
truce-egui = { workspace = true }
egui = "0.31"
```

## Quick Start

Override `custom_editor()` in your `PluginLogic` to return an `EguiEditor`:

```rust
use truce::prelude::*;
use truce_egui::{EguiEditor, ParamState};
use truce_egui::widgets::param_knob;

use MyParamsParamId as P;

impl PluginLogic for MyPlugin {
    // ... reset(), process() as usual ...

    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        Some(Box::new(
            EguiEditor::new((640, 480), |ctx: &egui::Context, state: &ParamState| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.heading("My Plugin");
                    param_knob(ui, state, P::Gain, "Gain");
                });
            })
        ))
    }
}
```

The rest of the plugin (`truce::plugin!`, params, DSP) stays exactly the same.
The only change is adding `custom_editor()`.

## Window Size

`EguiEditor::new((width, height), ui_fn)` takes the window size in **pixels**
(physical). On a 2x Retina display, a 640x480 pixel window is 320x240 logical
points. egui handles the scaling automatically via `pixels_per_point`.

## ParamState

`ParamState` bridges egui widgets to truce's host automation:

| Method | Description |
|--------|-------------|
| `state.get(id)` | Normalized value (0.0–1.0) |
| `state.get_plain(id)` | Plain value (in the param's native range) |
| `state.format(id)` | Host-formatted display string |
| `state.meter(id)` | Meter level (0.0–1.0) |
| `state.set_immediate(id, v)` | Set value in one shot (begin+set+end) |
| `state.begin_gesture(id)` | Start a drag gesture |
| `state.set_value(id, v)` | Update during a drag |
| `state.end_gesture(id)` | End a drag gesture |

For click/toggle interactions, use `set_immediate()`. For continuous drags
(knobs, sliders, XY pads), use the `begin_gesture` / `set_value` /
`end_gesture` sequence so the host records proper automation.

## Helper Widgets

`truce-egui` ships helper widgets that wrap egui primitives with the
correct gesture protocol:

```rust
use truce_egui::widgets::{
    param_knob,      // Rotary knob (vertical drag)
    param_slider,    // Horizontal slider (egui::Slider)
    param_toggle,    // On/off switch
    param_selector,  // Cycling button for enum/discrete params
    param_xy_pad,    // 2D pad controlling two params
    level_meter,     // Vertical meter bars (display-only)
};
```

Usage:

```rust
use GainParamsParamId as P;

fn my_ui(ctx: &egui::Context, state: &ParamState) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.horizontal(|ui| {
            param_knob(ui, state, P::Gain, "Gain");
            param_knob(ui, state, P::Pan, "Pan");
            param_toggle(ui, state, P::Bypass, "Bypass");
            level_meter(ui, state, &[P::MeterLeft.into(), P::MeterRight.into()], "Output");
        });
        param_xy_pad(ui, state, P::Pan, P::Gain, "Pan / Gain");
    });
}
```

These are optional conveniences. You can always use raw egui widgets and
call `ParamState` methods directly:

```rust
let mut value = state.get(id) as f32;
let response = ui.add(egui::Slider::new(&mut value, 0.0..=1.0));
if response.drag_started() { state.begin_gesture(id); }
if response.changed()      { state.set_value(id, value as f64); }
if response.drag_stopped()  { state.end_gesture(id); }
```

## Theming

`truce_egui::theme::dark()` returns egui `Visuals` matching the built-in
dark theme. Apply it with `.with_visuals()`:

```rust
EguiEditor::new((640, 480), my_ui)
    .with_visuals(truce_egui::theme::dark())
```

If omitted, `dark()` is applied automatically. Pass your own `egui::Visuals`
to customize colors, spacing, and fonts.

## Stateful UIs (EditorUi Trait)

For complex UIs with internal state (tab selection, animation, caches),
implement the `EditorUi` trait instead of using a closure:

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
                0 => { /* draw controls */ }
                1 => { /* draw settings */ }
                _ => {}
            }
        });
    }
}

// In custom_editor():
EguiEditor::with_ui((640, 480), MyEditorUi { selected_tab: 0 })
```

## Snapshot Testing

`truce-egui` includes headless rendering for snapshot tests. It creates a
wgpu device without a window, runs your UI function, and compares the result
against a reference PNG:

```rust
#[test]
fn gui_snapshot() {
    truce_egui::snapshot::assert_snapshot(
        "screenshots",           // directory (relative to workspace root)
        "my_plugin_default",   // snapshot name
        640, 480,              // width, height in pixels
        2.0,                   // pixels_per_point
        0,                     // max allowed pixel differences
        |ctx, state| my_ui(ctx, state),
    );
}
```

On the first run, the reference PNG is created. On subsequent runs, the
rendered output is compared pixel-by-pixel. Delete the PNG to regenerate.

The snapshot uses `ParamState::mock()` internally, which returns `0.5` for
all param values, `0.0` for meters, and `"pN"` for formatted strings.

## Example

See `examples/gain-egui/` for a complete working example
with knobs, sliders, toggle, XY pad, meters, and snapshot test.
