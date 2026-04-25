# 6. GUI

truce ships a built-in GUI designed for audio plugins. You
declare a layout — rows of widgets — and the framework draws it,
routes input events, and keeps everything in sync with the
parameter `Arc`. Zero pixel math.

If that's not enough, truce has adapters for egui, iced, and Slint,
plus a raw-window-handle escape hatch for anything else. Start with
the built-in and reach for a framework when you hit its limits.

## The built-in GUI

### Declaring a layout

```rust
use truce_gui::layout::{GridLayout, knob, slider, toggle,
                        dropdown, meter, xy_pad, widgets, section};
use MyParamsParamId as P;

impl PluginLogic for MyPlugin {
    // ...

    fn layout(&self) -> GridLayout {
        GridLayout::build("MY PLUGIN", "V0.1", 3, 80.0, vec![
            widgets(vec![
                knob(P::Gain, "Gain"),
                knob(P::Pan,  "Pan"),
                toggle(P::Bypass, "Bypass"),
            ]),
            section("FILTER", vec![
                knob(P::Cutoff,    "Cutoff"),
                knob(P::Resonance, "Reso"),
            ]),
        ])
    }
}
```

`GridLayout::build(title, version, cols, cell_size_dp, sections)`.
Each section is either a `widgets(vec![...])` row or a labelled
`section("NAME", vec![...])` group. Widgets flow left-to-right;
use `.cols(n)` / `.rows(n)` to span cells.

### The seven widgets

| Constructor | Widget | Typical use |
|-------------|--------|-------------|
| `knob(P::X, "Label")` | rotary | gain, cutoff, resonance, any `FloatParam` |
| `slider(P::X, "Label")` | linear slider | pan, mix, sometimes easier to read than a knob |
| `toggle(P::X, "Label")` | pill on/off | `BoolParam`, bypass |
| `selector(P::X, "Label")` | click-to-cycle | `EnumParam<T>` when the list is short |
| `dropdown(P::X, "Label")` | click-to-open list | `EnumParam<T>` / `IntParam` when the list is longer |
| `meter(&[P::L, P::R], "Label")` | vertical level meters | peak / RMS output |
| `xy_pad(P::X, P::Y, "Label")` | 2-axis pad | two continuous params on one surface |

Most plugins only use `knob`, `toggle`, and `meter`.

### Spanning cells

```rust
knob(P::Gain, "Gain"),                          // 1×1 cell (default)
dropdown(P::Wave, "Wave").cols(2),              // 2 cells wide
meter(&[P::L, P::R], "Level").rows(3),          // 3 cells tall
xy_pad(P::X, P::Y, "Pad").cols(2).rows(2),      // 2×2 cell block
```

Explicit positions work too: `knob(P::Gain, "Gain").at(col, row)`.
Useful when you want a dial tucked into the corner of a meter.

### Meters

Declare meters as `#[meter] pub x: MeterSlot` fields alongside your
params (see [parameters.md § Meters](parameters.md#meters)), push
from `process()` with `context.set_meter(P::MeterL, peak)`, and
render them in `layout()`:

```rust
meter(&[P::MeterL, P::MeterR], "Level").rows(3)
```

The DSP side is atomic and realtime-safe. The GUI reads the latest
value every frame.

### Interaction for free

- Drag on a knob / slider → change the param.
- Scroll-wheel on a knob → fine-tune.
- Double-click a knob → reset to default.
- Click a toggle / selector / dropdown → set / cycle / open.
- Right-click a widget → host context menu (automation, reset,
  enter value).

You don't write any of this. The framework knows the widget kind
from the layout and the parameter behaviour from the `ParamId`.

### Rendering and theming

The built-in GUI renders through `truce-gpu` (wgpu → Metal on
macOS, DX12 on Windows, Vulkan on Linux). CPU rasterisation via
tiny-skia is available as a fallback.

Colours come from a named theme — dark by default. Swap to light or
a custom palette:

```rust
GridLayout::build("MY PLUGIN", "V0.1", 3, 80.0, sections)
    .theme(truce_gui::theme::Theme::light())
```

Text renders via fontdue with JetBrains Mono Regular embedded at
compile time — no font file on disk, no runtime load.

See the [built-in GUI reference](../gui/built-in.md) for every
widget constructor, cell-spanning option, and theming detail.

## Alternatives

The built-in GUI covers knobs / sliders / meters / dropdowns —
the common audio-plugin shape. Reach for an alternative backend
when you need:

- **Text input fields** beyond the built-in value pop-in.
- **Lists or tables** (preset browsers, modulation matrices, sample
  browsers).
- **Custom graphics** — analyzer curves, waveforms, drawable
  envelopes.
- **Specific aesthetics** the built-in theme system can't reach.

All alternatives integrate the same way: override `custom_editor()`
on `PluginLogic` and return a boxed `Editor`. The `PluginLogic::
layout()` method becomes irrelevant when a custom editor is used.

| Backend | Crate | When |
|---------|-------|------|
| **egui** | `truce-egui` | Immediate-mode. Good for prototyping, CPU-graph-heavy debugging UIs, and dev tools. Full guide: [gui/egui.md](../gui/egui.md). |
| **iced** | `truce-iced` | Retained-mode with Elm architecture. Good for complex custom UIs where you want a proper widget tree and state machine. Auto-generated from `GridLayout` is also available. [gui/iced.md](../gui/iced.md). |
| **Slint** | `truce-slint` | Declarative markup (`.slint` files) with data binding. Good for visually rich UIs designed outside Rust. [gui/slint.md](../gui/slint.md). |
| **BYO** | `truce-core` + `RawWindowHandle` | Full control — Metal, OpenGL, Skia, anything. You handle painting, input, DPI, and lifecycle yourself. [gui/raw-window-handle.md](../gui/raw-window-handle.md). |

See [gui/README.md](../gui/README.md) for a side-by-side comparison
of the backends.

## Screenshot tests

Catch visual regressions by rendering your GUI headlessly and
diffing the result against a committed reference PNG. Same shape
across all four backends — render to RGBA pixels, then compare:

```rust
#[test]
fn gui_screenshot() {
    let params = Arc::new(MyParams::new());
    let plugin = MyPlugin::new(Arc::clone(&params));
    let layout = plugin.layout();
    let (pixels, w, h) = truce_gpu::screenshot::render_to_pixels(params, layout);
    truce_test::assert_screenshot(
        "my_plugin_default", &pixels, w, h, 0, "snapshots",
    );
}
```

Swap `truce_gpu::screenshot::render_to_pixels` for the matching
helper in `truce_egui`, `truce_iced`, or `truce_slint` and the
shape stays identical. The current render always lands in
`target/screenshots/` (gitignored); the comparator loads the
committed reference from the directory you pass in (`"snapshots"`
above) and prints a `cp`-based promote hint when the reference
doesn't exist yet.

See [gui/screenshot-testing.md](../gui/screenshot-testing.md) for
the full flow — promoting new references, cross-OS behavior, the
`TRUCE_SCREENSHOT_REFERENCE_OS` override, and per-backend examples.

## What's next

- **[Chapter 7 → hot-reload.md](hot-reload.md)** — edit the
  layout, save, see the change in the running plugin without
  closing the DAW.
- **[Built-in GUI reference](../gui/built-in.md)** — every widget
  constructor, all the cell options, theming.
- **[Screenshot testing](../gui/screenshot-testing.md)** — diff
  rendered pixels against committed PNGs.
- **[GUI backends](../gui/)** — deep-dives per framework when the
  built-in GUI isn't enough.
