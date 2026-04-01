# Layout Reference

Declare your plugin's GUI layout in `PluginLogic::layout()`. The
framework renders standard widgets automatically.

---

## GridLayout

All layouts use `GridLayout::build()`. Widgets auto-flow
left-to-right, wrapping to the next row. Widget constructors accept
`impl Into<u32>`, so both typed enum IDs (recommended) and raw `u32`
values work.

```rust
use MyParamsParamId as P;

fn layout(&self) -> truce_gui::layout::GridLayout {
    use truce_gui::layout::{GridLayout, knob, slider, toggle, widgets};
    GridLayout::build("MY PLUGIN", "V0.1", 3, 50.0, vec![widgets(vec![
        knob(P::Gain, "Gain"),
        slider(P::Pan, "Pan"),
        toggle(P::Bypass, "Bypass"),
    ])])
}
```

Arguments:
- `title` — header text
- `version` — version text
- `cols` — number of grid columns
- `cell_size` — cell size in pixels (width and height)
- `sections` — list of `section()` or `widgets()` groups

## Widgets

| Constructor | What | Default span |
|-------------|------|-------------|
| `knob(P::Gain, "Gain")` | Rotary knob | 1×1 |
| `slider(P::Pan, "Pan")` | Horizontal slider | 1×1 |
| `toggle(P::Bypass, "Bypass")` | On/off toggle | 1×1 |
| `selector(P::Mode, "Mode")` | Click-to-cycle (enums) | 1×1 |
| `dropdown(P::Mode, "Mode")` | Dropdown list (enums) | 1×1 |
| `meter(&[P::MeterLeft, P::MeterRight], "Level")` | Level meter (one bar per ID) | 1×1 |
| `xy_pad(P::Pan, P::Gain, "XY")` | 2D control pad | 2×2 |

## Spanning

```rust
dropdown(P::Wave, "Wave").cols(2)                            // 2 columns wide
meter(&[P::MeterLeft, P::MeterRight], "Level").rows(2)      // 2 rows tall
xy_pad(P::Pan, P::Gain, "XY")                               // defaults to 2×2
```

## Sections

Group widgets under labeled section headers with `section()`:

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

Use `widgets(vec![...])` for ungrouped widgets (no section header).

## Meters

Meters are declared as `MeterSlot` fields with the `#[meter]`
attribute in your params struct. IDs are auto-assigned starting at
256, and meter variants are included in the generated `ParamId` enum:

```rust
#[derive(Params)]
pub struct GainParams {
    // ... params ...

    #[meter]
    pub meter_left: MeterSlot,

    #[meter]
    pub meter_right: MeterSlot,
}
```

Report values via `context.set_meter(P::MeterLeft, value)` in
`process()`. `set_meter()` accepts `impl Into<u32>`.

## Complete Example

```rust
use GainParamsParamId as P;

fn layout(&self) -> truce_gui::layout::GridLayout {
    use truce_gui::layout::{GridLayout, knob, meter, xy_pad, widgets};
    GridLayout::build("GAIN", "V0.1", 3, 50.0, vec![widgets(vec![
        knob(P::Gain, "Gain"),
        knob(P::Pan, "Pan"),
        xy_pad(P::Pan, P::Gain, "XY"),
        meter(&[P::MeterLeft, P::MeterRight], "Level").rows(2),
    ])])
}
```
