# Vizia Integration

[Vizia](https://github.com/vizia/vizia) is a reactive, declarative GUI
framework. `truce-vizia` wraps it as a truce `Editor`, giving you a
retained-mode widget tree with CSS-like styling, data binding via lenses,
and vizia's full layout engine.

## When to Use Vizia

Use vizia when you want:

- Reactive data binding (lenses that auto-update on param changes)
- CSS-like styling and theming
- Complex widget trees with vizia's layout engine
- Vizia's built-in widget library (knobs, sliders, labels, etc.)

## Setup

Add `truce-vizia` to your plugin's `Cargo.toml`:

```toml
[dependencies]
truce-vizia = { workspace = true }
```

## Quick Start

Override `custom_editor()` to return a `ViziaEditor`:

```rust
use truce::prelude::*;
use truce_vizia::ViziaEditor;
use truce_vizia::widgets::*;

impl PluginLogic for MyPlugin {
    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        Some(Box::new(ViziaEditor::new((400, 300), |cx| {
            VStack::new(cx, |cx| {
                ParamKnob::new(cx, 0, "Gain");
                ParamSlider::new(cx, 1, "Pan");
                ParamToggle::new(cx, 2, "Bypass");
            });
        })))
    }
}
```

The closure receives a vizia `Context` (`cx`). Build your widget tree
using vizia's declarative API. `ViziaEditor` handles windowing, event
dispatch, and rendering (Skia/GL) internally via baseview.

## ParamModel and Events

`truce-vizia` uses vizia's model/event system for parameter communication.
A `ParamModel` is automatically registered in the vizia context tree and
provides parameter access.

### Reading parameters

Use lenses for reactive data binding:

```rust
use truce_vizia::{ParamNormLens, ParamFormatLens, ParamBoolLens, MeterLens};

// These lenses auto-update when the host changes values
Label::new(cx, ParamFormatLens(0));        // "0.0 dB"
Knob::new(cx, ParamNormLens(0), false);    // normalized 0.0–1.0
Checkbox::new(cx, ParamBoolLens(2));       // bool (>0.5)
```

Or read values directly from the model:

```rust
// Inside a vizia event handler or build closure
let value = ParamModel::get(cx, 0);          // normalized
let plain = ParamModel::get_plain(cx, 0);    // plain
let text = ParamModel::format(cx, 0);        // display string
let level = ParamModel::meter(cx, 100);      // meter level
```

### Writing parameters

Emit `ParamEvent` variants to update host parameters:

```rust
use truce_vizia::ParamEvent;

// Single-shot (toggles, selectors)
cx.emit(ParamEvent::SetImmediate(param_id, normalized_value));

// Drag gesture (knobs, sliders)
cx.emit(ParamEvent::BeginEdit(param_id));
cx.emit(ParamEvent::SetNormalized(param_id, value));
cx.emit(ParamEvent::EndEdit(param_id));
```

### Sync

The host notifies the editor of external parameter changes via:

```rust
cx.emit(ParamEvent::Sync);
```

This is handled automatically by `ViziaEditor` — you don't need to emit
it yourself.

## Widgets

`truce-vizia` provides parameter-aware widget wrappers:

| Widget | Constructor | Behavior |
|--------|------------|----------|
| `ParamKnob` | `ParamKnob::new(cx, id, label)` | Rotary knob with drag gesture |
| `ParamSlider` | `ParamSlider::new(cx, id, label)` | Horizontal slider |
| `ParamToggle` | `ParamToggle::new(cx, id, label)` | On/off switch |
| `LevelMeter` | `LevelMeter::new(cx, ids, label)` | Multi-channel meter |

These handle the gesture protocol automatically. For custom widgets,
use the lenses and events directly.

## Styling

Vizia supports CSS-like styling. Apply styles in your build closure:

```rust
ViziaEditor::new((400, 300), |cx| {
    // Load a stylesheet
    cx.add_stylesheet("style.css");

    VStack::new(cx, |cx| {
        Label::new(cx, "My Plugin")
            .class("title");
        ParamKnob::new(cx, 0, "Gain");
    })
    .class("main-panel");
})
```

## Resizing

`ViziaEditor` reports `can_resize() = true` by default. The host can
resize the window and vizia's layout engine reflows the widget tree
automatically.

## Example

See `crates/truce-vizia/examples/gain-vizia/` for a complete working
example with knobs, sliders, and reactive parameter display.
