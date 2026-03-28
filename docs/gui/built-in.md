# Built-in GUI

The built-in GUI is the easiest way to get a working plugin interface.
You define a layout — which knobs, sliders, and meters you want — and
truce renders everything automatically. No custom editor code needed.

This is what you get when you don't override `custom_editor()`. Just
implement `layout()` on your plugin and you're done.

## Your first layout

```rust
use truce_gui::layout::{GridLayout, knob, slider, meter, widgets};
use MyParamsParamId as P;

fn layout(&self) -> truce_gui::layout::GridLayout {
    GridLayout::build("MY PLUGIN", "V1.0", 3, 80.0, vec![widgets(vec![
        knob(P::Gain, "Gain"),
        knob(P::Pan, "Pan"),
        meter(&[P::MeterLeft, P::MeterRight], "Level"),
    ])])
}
```

That's it. Truce creates a window with a header, three widgets in a row,
and handles all mouse interaction, parameter automation, and host
communication for you.

### What `GridLayout::build` takes

- **title** — text in the header bar (e.g., "MY PLUGIN")
- **version** — version shown in the header (e.g., "V1.0")
- **cols** — number of grid columns (widgets wrap to the next row)
- **cell_size** — pixel size of one grid cell
- **sections** — your widgets, wrapped in `widgets(vec![...])` or `section("LABEL", vec![...])`

The window size is computed automatically from the grid dimensions.

## Available widgets

| Widget | Constructor | What it does |
|--------|------------|--------------|
| Knob | `knob(P::Gain, "Gain")` | Rotary control. Drag vertically to adjust. |
| Slider | `slider(P::Pan, "Pan")` | Horizontal slider. Drag left/right. |
| Toggle | `toggle(P::Bypass, "Bypass")` | On/off switch. Click to flip. |
| Selector | `selector(P::Mode, "Mode")` | Click to cycle through enum values. |
| Dropdown | `dropdown(P::Mode, "Mode")` | Click to open a popup list of all options. |
| Meter | `meter(&[P::MeterLeft, P::MeterRight], "Level")` | Level display (read-only). One bar per ID. |
| XY Pad | `xy_pad(P::Pan, P::Gain, "XY")` | 2D control for two parameters. Drag anywhere. |

All constructors accept `impl Into<u32>`, so you pass your typed param ID
enum directly (e.g., `P::Gain`). No `.into()` needed.

### Auto-detection

If you don't explicitly choose a widget type, the system picks one
based on the parameter type:

- `BoolParam` (0 or 1) becomes a **toggle**
- `EnumParam` becomes a **selector**
- `FloatParam` / `IntParam` becomes a **knob**

## Sections

Group widgets under labeled headers with `section()`:

```rust
use truce_gui::layout::{GridLayout, knob, section, widgets};

GridLayout::build("EQ", "V0.1", 3, 70.0, vec![
    section("LOW", vec![
        knob(P::LowFreq, "Freq"),
        knob(P::LowGain, "Gain"),
        knob(P::LowQ, "Q"),
    ]),
    section("MID", vec![
        knob(P::MidFreq, "Freq"),
        knob(P::MidGain, "Gain"),
        knob(P::MidQ, "Q"),
    ]),
    widgets(vec![knob(P::Output, "Output")]),
])
```

Each `section("LABEL", ...)` starts a new row with a header label above
it. Use `widgets(vec![...])` for widgets that don't belong to a section.

## Spanning and positioning

Widgets default to 1x1 grid cells. Make them bigger or place them
explicitly:

```rust
// Span 2 rows (tall meter)
meter(&[P::MeterLeft, P::MeterRight], "Level").rows(2)

// Span 3 columns (wide selector)
dropdown(P::Wave, "Wave").cols(3)

// Explicit grid position (overrides auto-flow)
meter(&[P::MeterLeft, P::MeterRight], "Level").at(2, 0).rows(3)
```

## Meters

Meters read values you set in `process()`:

```rust
// In your params struct
#[meter]
pub meter_left: MeterSlot,

#[meter]
pub meter_right: MeterSlot,
```

```rust
// In process()
context.set_meter(P::MeterLeft, buffer.output_peak(0));
context.set_meter(P::MeterRight, buffer.output_peak(1));
```

```rust
// In layout()
meter(&[P::MeterLeft, P::MeterRight], "Level")
```

The `#[meter]` attribute auto-generates IDs. No manual numbering needed.

## Theming

Customize colors by passing a `Theme`:

```rust
fn custom_editor(&self) -> Option<Box<dyn Editor>> {
    Some(Box::new(
        BuiltinEditor::new_grid(self.params.clone(), self.layout())
            .with_theme(Theme {
                primary: Color::rgb(0x00, 0xd2, 0xff),
                ..Theme::dark()
            })
    ))
}
```

## Interaction reference

| Widget | Input | Reset |
|--------|-------|-------|
| Knob | Vertical drag | Double-click for default |
| Slider | Horizontal drag | Double-click for default |
| Toggle | Click | Click |
| Selector | Click to cycle | Double-click for default |
| Dropdown | Click to open list | Double-click for default |
| XY Pad | 2D drag | Double-click for default |
| Meter | Display-only | — |

Mouse wheel adjusts the control under the cursor. All interactions
automatically follow the host's automation gesture protocol.

## When to use something else

The built-in GUI handles standard plugin UIs well. Consider switching if
you need:

- Custom layouts (tabs, scrolling, collapsible sections) — try [egui](egui.md)
- Text input fields — try [egui](egui.md)
- Elm-architecture state management — try [Iced](iced.md)
- Declarative markup with IDE preview — try [Slint](slint.md)
- Completely custom rendering — see [Raw window handle](raw-window-handle.md)
