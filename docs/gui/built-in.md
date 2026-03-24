# Built-in GUI

The built-in GUI (`truce-gui`) is a zero-code, layout-driven UI system.
Define your layout in the `layout()` method and truce renders it
automatically. No custom editor code needed.

## Rendering

The built-in GUI renders using wgpu (Metal/DX12/Vulkan) with lyon
tessellation. GPU rendering is always on — no feature flags needed.

The CPU backend (`truce-gui`, tiny-skia) is used internally for snapshot
testing and as the rendering abstraction layer. The GPU backend implements
the same `RenderBackend` trait and renders identically.

Text is rendered with fontdue (TrueType rasterization) using JetBrains
Mono Regular, embedded at compile time.

## GridLayout

The current layout system uses `GridLayout` with auto-flow placement.
Define a grid with a column count, cell size, and a list of widgets:

```rust
use truce_gui::layout::{GridLayout, GridWidget};

fn layout(&self) -> truce_gui::layout::GridLayout {
    GridLayout::build("MY PLUGIN", "V1.0", 3, 80.0, vec![
        GridWidget::knob(P::Gain, "Gain"),
        GridWidget::slider(P::Pan, "Pan"),
        GridWidget::toggle(P::Bypass, "Bypass"),
        GridWidget::meter(&[P::MeterLeft.into(), P::MeterRight.into()], "Level").rows(2),
        GridWidget::xy_pad(P::Pan, P::Gain, "XY"),
    ], vec![
        (3, "METERS"),  // section label before widget index 3
    ])
}
```

Arguments to `GridLayout::build`:
- `title` — header text
- `version` — version string shown in header
- `cols` — number of grid columns
- `cell_size` — pixel size of one grid cell
- `widgets` — list of widgets (auto-placed left-to-right, top-to-bottom)
- `breaks` — section labels: `(widget_index, label)` pairs

Window size is computed automatically from the grid dimensions.

## Widget Types

6 widget types, all constructed via `GridWidget`:

| Widget | Constructor | Default span | Input |
|--------|------------|-------------|-------|
| Knob | `GridWidget::knob(id, label)` | 1x1 | Vertical drag |
| Slider | `GridWidget::slider(id, label)` | 1x1 | Horizontal drag |
| Toggle | `GridWidget::toggle(id, label)` | 1x1 | Click |
| Selector | `GridWidget::selector(id, label)` | 1x1 | Click to cycle |
| Meter | `GridWidget::meter(ids, label)` | 1x1 | Display-only |
| XY Pad | `GridWidget::xy_pad(x_id, y_id, label)` | 2x2 | 2D drag |

All constructors accept `impl Into<u32>`, so you can pass typed param ID
enums directly (e.g., `P::Gain`).

### Spanning and positioning

```rust
// Span 2 rows
GridWidget::meter(&[P::MeterLeft.into(), P::MeterRight.into()], "Level").rows(2)

// Span 3 columns
GridWidget::knob(P::Gain, "Gain").cols(3)

// Explicit grid position (overrides auto-flow)
GridWidget::knob(P::Gain, "Gain").at(0, 2)
```

### Auto-detection

If you don't specify a widget type, the system auto-detects from the
parameter range:
- `BoolParam` → toggle
- `EnumParam` → selector
- Continuous (`FloatParam`, `IntParam`) → knob

## Theme

Customize colors with the `Theme` struct:

```rust
fn custom_editor(&self) -> Option<Box<dyn Editor>> {
    let layout = self.layout();
    let params = Arc::new(self.params.clone());
    Some(Box::new(
        BuiltinEditor::new_grid(params, layout)
            .with_theme(Theme {
                bg: Color::rgb(0x1a, 0x1a, 0x2e),
                surface: Color::rgb(0x25, 0x25, 0x3a),
                primary: Color::rgb(0x00, 0xd2, 0xff),
                ..Theme::dark()
            })
    ))
}
```

## Interaction

- **Knobs**: vertical drag to adjust. Double-click to reset to default.
- **Sliders**: horizontal drag. Double-click to reset.
- **Toggles**: click to flip.
- **Selectors**: click to cycle through values.
- **XY pads**: 2D drag controlling two parameters simultaneously.
- **Scroll**: mouse wheel adjusts the control under the cursor.

All interactions follow the host automation gesture protocol
(begin → set → end) automatically.

## When to Use Something Else

The built-in GUI covers standard plugin UIs well. Consider a different
backend if you need:

- Custom layouts (tabs, scrolling, collapsible sections) → [egui](egui.md)
- Text input fields → [egui](egui.md)
- Reactive data binding, CSS-like styling → [Vizia](vizia.md)
- Elm-architecture, complex state management → [Iced](iced.md)
- Completely custom rendering → [Raw window handle](raw-window-handle.md)
