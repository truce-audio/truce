# truce-cpu

CPU rendering backend for the truce built-in GUI.

## Overview

Provides `CpuBackend`, a software implementation of
`truce_gui_types::RenderBackend` built on tiny-skia, plus the skrifa
glyph cache (`font`) and the `ColorExt` conversions the rasterizer
needs. This is the default renderer: it works on every platform without
a usable GPU, and its output is deterministic, which makes it the
reliable path for screenshot tests.

`truce-cpu` is an implementation detail of
[`truce-gui`](../truce-gui): `BuiltinEditor` rasterizes the widget tree
into a tiny-skia pixmap through this backend and blits it to a wgpu
surface for compositing. Plugins don't depend on `truce-cpu` directly -
it's pulled in by `truce-gui`'s default `cpu` feature. Its peer
[`truce-gpu`](../truce-gpu) provides the wgpu backend behind the `gpu`
feature.

## Key types

- **`CpuBackend`** -- tiny-skia `truce_gui_types::RenderBackend` implementation
- **`ColorExt`** -- extension trait adding `to_skia()` / `to_premultiplied()` to the light `truce_gui_types::theme::Color` type, so that type stays tiny-skia-free
- **`font`** -- glyph cache + text measurement (skrifa outlines rasterized with tiny-skia), fed by the bundled JetBrains Mono from [`truce-font`](../truce-font)

## Usage

`truce-cpu` is selected automatically - the default `cpu` feature on
`truce-gui` enables it, and `editor()` renders through it:

```rust
fn editor(params: Arc<MyParams>) -> Box<dyn Editor> {
    GridLayout::build(vec![widgets(vec![
        knob(P::Gain, "Gain"),
    ])])
    .into_editor(&params)
}
```

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
