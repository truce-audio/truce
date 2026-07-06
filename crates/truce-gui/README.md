# truce-gui

Built-in GUI runtime for truce plugins.

## Overview

Provides a complete widget toolkit that draws knobs, sliders, meters,
dropdowns, and XY pads straight from your parameter layout - no custom
editor code, no pixel math. The `BuiltinEditor` auto-generates a full UI
from a `GridLayout`. By default it rasterises on the CPU with tiny-skia;
opt into GPU rendering (wgpu) with the `gpu` feature.

Windowing is handled through baseview. In CPU mode the tiny-skia pixmap
is uploaded to a wgpu surface each frame for compositing; in GPU mode
`GpuEditor` renders the widgets directly through wgpu. All supported
formats (CLAP, VST3, VST2, AU, LV2, AAX, standalone) use the same path.

`truce-gui` orchestrates the two `RenderBackend` implementations into
editor types. The renderers themselves live in sibling crates, so a
plugin only compiles the one it uses:

- the CPU rasteriser (`CpuBackend`, tiny-skia + fontdue) lives in
  [`truce-cpu`](../truce-cpu), pulled in by the default `cpu` feature;
- the GPU backend (`WgpuBackend`, wgpu) lives in
  [`truce-gpu`](../truce-gpu), pulled in by the `gpu` feature.

The lightweight trait + data surface (`GridLayout`, `RenderBackend`,
`WidgetType`, `Theme`, ...) lives in
[`truce-gui-types`](../truce-gui-types) so alt-GUI backends
(`truce-egui` / `truce-iced` / `truce-slint`) and plugin authors using a
custom editor don't transitively pull in the rasterisation + windowing
stack.

## Key types

- **`BuiltinEditor`** -- the main `Editor` implementation; auto-generates UI from params
- **`GpuEditor`** (with the `gpu` feature) -- wraps `BuiltinEditor` to render through `truce_gpu::WgpuBackend`
- **`default_editor()` / `IntoLayoutEditor`** -- turn a `GridLayout` into a `Box<dyn Editor>`, picking the renderer the active feature selects
- **`CpuBackend`** (re-exported from `truce-cpu` with the `cpu` feature) -- the tiny-skia software rasterizer (the `RenderBackend` trait it implements lives in `truce-gui-types`)
- **`BaseviewTranslator`** -- maps `baseview::Event`s into the
  `InputEvent` stream consumed by `truce_gui_types::interaction::dispatch`
- **Re-exports** of every `truce-gui-types` and `truce-plugin`
  public symbol so existing `truce_gui::layout::*` /
  `truce_gui::PluginLogic` paths keep resolving

## Widgets

Knobs, sliders, toggles, dropdowns, XY pads, level meters, labels, and
parameter groups. All widgets bind directly to truce parameters. The
widget data types + draw helpers live in `truce-gui-types::widgets`;
this crate orchestrates the renderer the draw helpers paint into.

## Usage

```rust
fn editor(params: Arc<MyParams>) -> Box<dyn Editor> {
    GridLayout::build(vec![widgets(vec![
        knob(P::Gain, "Gain"),
    ])])
    .into_editor(&params)
}
```

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
