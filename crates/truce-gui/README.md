# truce-gui

Built-in GPU-free GUI for truce plugins.

## Overview

Provides a complete widget toolkit that renders entirely on the CPU using
tiny-skia for software rasterization. This gives plugins a zero-dependency GUI
that works on every platform without requiring a GPU. The `BuiltinEditor`
can auto-generate a full UI from your parameter layout, or you can compose
widgets manually for a custom look.

A `CgBlit` rendering path is included for AAX on macOS, where standard
compositor-based rendering is not safe.

## Key types

- **`BuiltinEditor`** -- the main `Editor` implementation; auto-generates UI from params
- **`RenderBackend`** -- trait for plugging in alternative renderers
- **`CpuBackend`** -- default tiny-skia software rasterizer
- **`Theme`** -- visual styling configuration (colors, sizes, fonts)

## Widgets

Knobs, sliders, toggles, dropdowns, XY pads, level meters, labels, and
parameter groups. All widgets bind directly to truce parameters.

## Usage

```rust
fn editor() -> Option<Box<dyn Editor>> {
    Some(Box::new(BuiltinEditor::new()))
}
```

Part of [truce](https://github.com/truce-audio/truce).
