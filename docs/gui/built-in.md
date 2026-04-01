# Built-in GUI

The built-in GUI renders widgets from a layout you define in code. No
custom editor, no framework dependency — just describe what you want
and truce draws it.

## A simple layout

```rust
use truce_gui::layout::{GridLayout, knob, widgets};
use MyParamsParamId as P;

fn layout(&self) -> GridLayout {
    GridLayout::build("MY PLUGIN", "V1.0", 2, 50.0, vec![widgets(vec![
        knob(P::Gain, "Gain"),
        knob(P::Pan, "Pan"),
    ])])
}
```

This creates a window with a header bar and two rotary knobs. The `2`
means two columns, `50.0` is the cell size in pixels. Widgets flow
left-to-right and wrap to the next row automatically.

## Adding more widgets

```rust
use truce_gui::layout::{GridLayout, knob, slider, toggle, meter, dropdown, xy_pad, widgets};

GridLayout::build("MY PLUGIN", "V1.0", 3, 50.0, vec![widgets(vec![
    knob(P::Gain, "Gain"),
    slider(P::Pan, "Pan"),
    toggle(P::Bypass, "Bypass"),
    dropdown(P::Mode, "Mode"),
    meter(&[P::MeterLeft, P::MeterRight], "Level"),
    xy_pad(P::Pan, P::Gain, "XY"),
])])
```

Each constructor takes a parameter ID and a label. `meter()` takes a
slice of meter IDs (one bar per channel). `xy_pad()` takes two parameter
IDs (X and Y axes).

You don't have to specify widget types for every parameter. If you leave
it to auto-detection:
- `BoolParam` becomes a toggle
- `EnumParam` becomes a selector
- Everything else becomes a knob

## Grouping with sections

For larger plugins, organize widgets under labeled headers:

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

`section("LABEL", vec![...])` starts a new row with a header. Use
`widgets(vec![...])` for the ungrouped leftovers.

## Making widgets bigger

Widgets default to 1x1 grid cells. Override with `.cols()`, `.rows()`,
or `.at()`:

```rust
// tall meter spanning 3 rows, pinned to column 2
meter(&[P::MeterLeft, P::MeterRight], "Level").at(2, 0).rows(3)

// wide dropdown spanning 2 columns
dropdown(P::Wave, "Wave").cols(2)
```

## Meters

Declare meter slots in your params struct with `#[meter]`:

```rust
#[derive(Params)]
pub struct MyParams {
    #[param(name = "Gain", range = "linear(-60, 6)", unit = "dB")]
    pub gain: FloatParam,

    #[meter]
    pub meter_left: MeterSlot,

    #[meter]
    pub meter_right: MeterSlot,
}
```

Report values in `process()`:

```rust
context.set_meter(P::MeterLeft, buffer.output_peak(0));
context.set_meter(P::MeterRight, buffer.output_peak(1));
```

Display them in the layout:

```rust
meter(&[P::MeterLeft, P::MeterRight], "Level")
```

## Theming

Pass a custom `Theme` to change colors:

```rust
use truce_gui::theme::{Theme, Color};

BuiltinEditor::new_grid(params, layout)
    .with_theme(Theme {
        primary: Color::rgb(0x00, 0xd2, 0xff),
        ..Theme::dark()
    })
```

## Interaction

- **Knobs** and **sliders**: drag to adjust. Double-click to reset.
- **Toggles**: click to flip.
- **Selectors**: click to cycle.
- **Dropdowns**: click to open a list, click an option to select.
- **XY pads**: drag anywhere to set both values.
- **Mouse wheel**: adjusts the control under the cursor.

## Moving beyond the built-in GUI

The built-in GUI handles standard plugin UIs. When you need custom
layouts, text input, or more complex interaction, look at
[egui](egui.md), [Iced](iced.md), or [Slint](slint.md).
