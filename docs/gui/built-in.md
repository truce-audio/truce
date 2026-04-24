# Built-in GUI

The built-in GUI renders widgets from a layout you define in code.
No custom editor, no framework dependency — declare what you want
and truce draws it. This is the default; override `layout()` on
`PluginLogic` and you're done.

For a first walkthrough see
[reference/gui.md](../reference/gui.md). This page is the
reference for every option.

## `GridLayout::build`

```rust
use truce_gui::layout::{GridLayout, knob, slider, toggle, widgets};
use MyParamsParamId as P;

fn layout(&self) -> GridLayout {
    GridLayout::build("MY PLUGIN", "V1.0", 3, 50.0, vec![widgets(vec![
        knob(P::Gain, "Gain"),
        slider(P::Pan, "Pan"),
        toggle(P::Bypass, "Bypass"),
    ])])
}
```

Signature:

```rust
GridLayout::build(
    title: &str,          // header bar text
    version: &str,        // header bar version label
    cols: u32,            // number of grid columns
    cell_size: f32,       // cell size in pixels (square cells)
    sections: Vec<Section>, // widgets() or section() entries
) -> GridLayout
```

Widget constructors accept `impl Into<u32>`, so both typed enum IDs
(recommended — `P::Gain`) and raw `u32` values work.

## Widgets

| Constructor | Widget | Default span | Typical param type |
|-------------|--------|--------------|--------------------|
| `knob(P::X, "Label")` | rotary knob | 1×1 | `FloatParam`, `IntParam` |
| `slider(P::X, "Label")` | horizontal slider | 1×1 | `FloatParam` |
| `toggle(P::X, "Label")` | pill on/off | 1×1 | `BoolParam` |
| `selector(P::X, "Label")` | click-to-cycle | 1×1 | `EnumParam<T>`, `IntParam` |
| `dropdown(P::X, "Label")` | click-to-open list | 1×1 | `EnumParam<T>`, `IntParam` |
| `meter(&[P::L, P::R], "Label")` | level meter (one bar per ID) | 1×1 | `MeterSlot` |
| `xy_pad(P::X, P::Y, "Label")` | 2D control pad | 2×2 | two `FloatParam`s |

If you don't specify a widget type, the default is inferred from
the parameter type: `BoolParam` → toggle, `EnumParam` →
selector, everything else → knob.

## Sections

Group widgets under labelled headers with `section()`. Use
`widgets()` for the ungrouped rows.

```rust
use truce_gui::layout::{GridLayout, knob, section, widgets};

GridLayout::build("EQ", "V0.1", 3, 50.0, vec![
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

Each `section` starts a new row with a header strip. Widgets inside
a section flow left-to-right within that section's row.

## Spanning and positioning

Widgets default to 1×1 cells. Override with `.cols()`, `.rows()`,
and `.at()`:

```rust
dropdown(P::Wave, "Wave").cols(2),                     // 2 cells wide
meter(&[P::L, P::R], "Level").rows(3),                 // 3 cells tall
xy_pad(P::X, P::Y, "Pad").cols(2).rows(2),             // already 2×2 by default
meter(&[P::L, P::R], "Level").at(2, 0).rows(3),        // pinned to column 2, row 0
```

`.at(col, row)` is useful when you want a widget anchored (e.g. a
tall meter in the corner) while the rest flow freely.

## Meters

Declare meter slots in your params struct with `#[meter]`:

```rust
#[derive(Params)]
pub struct MyParams {
    #[param(name = "Gain", range = "linear(-60, 6)", unit = "dB")]
    pub gain: FloatParam,

    #[meter] pub meter_left:  MeterSlot,
    #[meter] pub meter_right: MeterSlot,
}
```

Meter IDs auto-assign starting at 256 and appear in the generated
`MyParamsParamId` enum alongside parameters.

Push from `process()` (realtime-safe atomic write):

```rust
context.set_meter(P::MeterLeft,  buffer.output_peak(0));
context.set_meter(P::MeterRight, buffer.output_peak(1));
```

Draw in the layout:

```rust
meter(&[P::MeterLeft, P::MeterRight], "Level").rows(3)
```

## Theming

Colours come from a named theme. Dark is the default. Switch
themes or override individual colours:

```rust
use truce_gui::theme::{Theme, Color};

GridLayout::build("MY PLUGIN", "V0.1", 3, 50.0, sections)
    .theme(Theme::light())
```

```rust
GridLayout::build("MY PLUGIN", "V0.1", 3, 50.0, sections)
    .theme(Theme {
        primary: Color::rgb(0x00, 0xd2, 0xff),
        ..Theme::dark()
    })
```

Fonts: fontdue rasterisation with JetBrains Mono Regular embedded
at compile time. No font file on disk, no runtime load.

Rendering: `truce-gpu` through wgpu (Metal on macOS, DX12 on
Windows, Vulkan on Linux). Tiny-skia CPU rasterisation is the
fallback.

## Interaction

The framework handles all of the following automatically — you
don't wire any of it by hand:

- **Knob / slider**: drag to adjust. Scroll-wheel to fine-tune.
  Double-click to reset to default.
- **Toggle**: click to flip.
- **Selector**: click to cycle forward.
- **Dropdown**: click to open the popup list, click an option to
  select.
- **XY pad**: drag anywhere on the pad to set both parameters.
- **Right-click**: opens the host's context menu (automation,
  reset, enter value).

## A full example

```rust
use GainParamsParamId as P;
use truce_gui::layout::{GridLayout, knob, meter, widgets, xy_pad};

fn layout(&self) -> GridLayout {
    GridLayout::build("GAIN", "V0.1", 3, 50.0, vec![widgets(vec![
        knob(P::Gain, "Gain"),
        knob(P::Pan,  "Pan"),
        xy_pad(P::Pan, P::Gain, "XY"),
        meter(&[P::MeterLeft, P::MeterRight], "Level").rows(2),
    ])])
}
```

## Moving beyond the built-in GUI

The built-in widget set covers the common audio-plugin shape —
knobs / sliders / meters / dropdowns. When you need text input
fields, lists, tables, analyzer curves, or a specific visual style
the theme system can't reach, switch to a framework backend:

- [egui](egui.md) — immediate-mode, fast to prototype.
- [iced](iced.md) — retained-mode, Elm architecture, good for
  complex custom UIs.
- [slint](slint.md) — declarative `.slint` markup with data
  binding.
- [raw-window-handle](raw-window-handle.md) — full control: Metal,
  OpenGL, Skia, anything.
