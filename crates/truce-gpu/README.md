# truce-gpu

wgpu rendering backend for the truce built-in GUI.

## Overview

Provides `WgpuBackend`, a hardware-accelerated implementation of
`truce_gui_types::RenderBackend` via wgpu (Metal on macOS, DX12 on
Windows, Vulkan on Linux), plus the wgpu surface / swapchain glue. Uses
lyon for path tessellation and skrifa for glyph atlas generation.
Widgets render identically to the CPU path but with better performance
on complex UIs.

`truce-gpu` is an implementation detail of
[`truce-gui`](../truce-gui): the `GpuEditor` there wraps the built-in
widget runtime and renders through this backend. Plugins don't depend on
`truce-gpu` directly - they enable GPU rendering through `truce-gui`'s
`gpu` feature.

## Key types

- **`WgpuBackend`** -- implements `truce_gui_types::RenderBackend` using wgpu

## Usage

Opt into GPU rendering by enabling the `gpu` feature on `truce-gui`:

```toml
[dependencies]
truce     = { version = "6.1", features = ["clap"] }
truce-gui = { version = "6.1", features = ["gpu"] }
```

`truce-gui` pulls in `truce-gpu` transitively and routes the built-in
editor through `WgpuBackend`. No direct dependency and no per-editor
code: the same `editor()` impl works for both renderers.

```rust
fn editor(params: Arc<MyParams>) -> Box<dyn Editor> {
    GridLayout::build(vec![widgets(vec![
        knob(P::Gain, "Gain"),
    ])])
    .into_editor(&params)
}
```

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
