# truce-gui

Built-in GPU-free GUI for truce plugins.

Uses a `RenderBackend` trait to abstract over rendering implementations. The
default `CpuBackend` uses tiny-skia for software rasterization, providing a
zero-dependency GUI that works everywhere without a GPU.

## Key types

- **`BuiltinEditor`** — the main `Editor` implementation
- **`RenderBackend`** — trait for plugging in alternative renderers
- **`Theme`** — visual styling configuration
