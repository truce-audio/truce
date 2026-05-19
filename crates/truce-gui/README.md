# truce-gui

Built-in GPU-free GUI runtime for truce plugins.

## Overview

Provides a complete widget toolkit that renders entirely on the CPU using
tiny-skia for software rasterization. This gives plugins a zero-dependency GUI
that works on every platform without requiring a GPU. The `BuiltinEditor`
can auto-generate a full UI from your parameter layout, or you can compose
widgets manually for a custom look.

Windowing is handled through baseview; the tiny-skia pixmap is uploaded to a
wgpu surface each frame for compositing. All supported formats (CLAP, VST3,
VST2, AU, LV2, AAX, standalone) use the same path.

`truce-gui` is the **heavy** half of the GUI split - the
`BuiltinEditor` and its runtime dependencies (tiny-skia,
baseview, fontdue, truce-font) live here. The lightweight
trait + data surface (`GridLayout`, `RenderBackend`,
`WidgetType`, `Theme`, …) lives in
[`truce-gui-types`](../truce-gui-types) so alt-GUI backends
(`truce-egui` / `truce-iced` / `truce-slint`) and plugin authors
using a custom editor don't transitively pull in the
rasterisation + windowing stack.

## Key types

- **`BuiltinEditor`** -- the main `Editor` implementation; auto-generates UI from params
- **`CpuBackend`** -- default tiny-skia software rasterizer (the
  `RenderBackend` trait it implements lives in `truce-gui-types`)
- **`BaseviewTranslator`** -- maps `baseview::Event`s into the
  `InputEvent` stream consumed by `truce_gui_types::interaction::dispatch`
- **`ColorExt`** -- extension trait that adds `to_skia()` /
  `to_premultiplied()` to the light `truce_gui_types::theme::Color`
  type (the rasterizer-specific conversions live here so the
  light crate stays tiny-skia-free)
- **Re-exports** of every `truce-gui-types` and `truce-plugin`
  public symbol so existing `truce_gui::layout::*` /
  `truce_gui::PluginLogic` paths keep resolving

## Widgets

Knobs, sliders, toggles, dropdowns, XY pads, level meters, labels, and
parameter groups. All widgets bind directly to truce parameters. The
widget data types + draw helpers live in `truce-gui-types::widgets`;
this crate provides the CPU rasterizer that the draw helpers paint
into.

## Usage

```rust
fn editor() -> Option<Box<dyn Editor>> {
    Some(Box::new(BuiltinEditor::new()))
}
```

Part of [truce](https://github.com/truce-audio/truce).
