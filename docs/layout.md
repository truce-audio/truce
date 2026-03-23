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
| `GridWidget::meter(&[Meter::Left.into()], "Level")` | Level meter (one bar per ID) | 1×1 |
| `GridWidget::xy_pad(P::Pan, P::Gain, "XY")` | 2D control pad | 2×2 |

## Spanning

```rust
GridWidget::selector(P::Wave, "Wave").cols(2)                       // 2 columns wide
GridWidget::meter(&[Meter::Left.into(), Meter::Right.into()], "Level").rows(2)  // 2 rows tall
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

## Meter IDs

Meter IDs are separate from parameter IDs. Define a `#[repr(u32)]`
enum with `From<T> for u32` to prevent mixing param/meter IDs at
compile time:

```rust
#[repr(u32)]
#[derive(Clone, Copy)]
pub enum Meter { Left = 100, Right = 101 }
impl From<Meter> for u32 { fn from(m: Meter) -> u32 { m as u32 } }
```

Report values via `context.set_meter(Meter::Left, value)` in
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
        GridWidget::meter(&[Meter::Left.into(), Meter::Right.into()], "Level").rows(2),
    ], vec![])
}
```
