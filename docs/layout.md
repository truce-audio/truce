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
    use truce_gui::layout::{GridLayout, GridWidget};
    GridLayout::build("MY PLUGIN", "V0.1", 3, 80.0, vec![
        GridWidget::knob(P::Gain, "Gain"),
        GridWidget::slider(P::Pan, "Pan"),
        GridWidget::toggle(P::Bypass, "Bypass"),
    ], vec![])
}
```

Arguments:
- `title` — header text
- `version` — version text
- `cols` — number of grid columns
- `cell_size` — cell size in pixels (width and height)
- `widgets` — widget list (auto-flows left-to-right)
- `breaks` — section breaks: `vec![(index, "LABEL")]`

## Widgets

| Constructor | What | Default span |
|-------------|------|-------------|
| `GridWidget::knob(P::Gain, "Gain")` | Rotary knob | 1×1 |
| `GridWidget::slider(P::Pan, "Pan")` | Horizontal slider | 1×1 |
| `GridWidget::toggle(P::Bypass, "Bypass")` | On/off toggle | 1×1 |
| `GridWidget::selector(P::Mode, "Mode")` | Click-to-cycle (enums) | 1×1 |
| `GridWidget::meter(&[P::MeterLeft.into()], "Level")` | Level meter (one bar per ID) | 1×1 |
| `GridWidget::xy_pad(P::Pan, P::Gain, "XY")` | 2D control pad | 2×2 |

## Spanning

```rust
GridWidget::selector(P::Wave, "Wave").cols(2)                       // 2 columns wide
GridWidget::meter(&[P::MeterLeft.into(), P::MeterRight.into()], "Level").rows(2)  // 2 rows tall
GridWidget::xy_pad(P::Pan, P::Gain, "XY")                          // defaults to 2×2
```

## Section Breaks

Add labeled section headers between widget groups:

```rust
GridLayout::build("EQ", "V0.1", 3, 80.0, vec![
    GridWidget::knob(P::LowFreq, "Low Freq"),
    GridWidget::knob(P::LowGain, "Low Gain"),
    GridWidget::knob(P::LowQ, "Low Q"),
    GridWidget::knob(P::MidFreq, "Mid Freq"),  // index 3: starts "MID" section
    GridWidget::knob(P::MidGain, "Mid Gain"),
    GridWidget::knob(P::MidQ, "Mid Q"),
], vec![
    (3, "MID"),  // section break before widget at index 3
])
```

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
    use truce_gui::layout::{GridLayout, GridWidget};
    GridLayout::build("GAIN", "V0.1", 3, 80.0, vec![
        GridWidget::knob(P::Gain, "Gain"),
        GridWidget::slider(P::Pan, "Pan"),
        GridWidget::toggle(P::Bypass, "Bypass"),
        GridWidget::xy_pad(P::Pan, P::Gain, "XY"),
        GridWidget::meter(&[P::MeterLeft.into(), P::MeterRight.into()], "Level").rows(2),
    ], vec![])
}
```
