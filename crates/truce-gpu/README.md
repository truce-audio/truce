# truce-gpu

GPU rendering backend for truce plugins.

Uses wgpu (Metal / DX12 / Vulkan) with lyon tessellation and a fontdue glyph
atlas. Implements `truce_gui::RenderBackend` so widgets render identically to
the CPU path, but with hardware acceleration. Platform windowing is provided by
baseview.

## Key types

- **`WgpuBackend`** — the `RenderBackend` implementation
- **`GpuEditor`** — GPU-accelerated `Editor`
