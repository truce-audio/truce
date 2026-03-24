## GUI

truce supports multiple GUI backends. All backends implement the same
`Editor` trait, so your choice of GUI has zero impact on DSP, parameters,
or format compatibility.

### Built-in GUI

The default. Define a `layout()` method and truce renders it
automatically — no custom editor code needed.

The rendering backend is GPU-based (wgpu/Metal) by default.

Text is rendered using fontdue (TrueType rasterization) with JetBrains
Mono Regular embedded at compile time.

### Other backends

For more complex UIs, truce provides additional backends. Each has a
dedicated deep-dive guide:

| Backend | Crate | Best for | Guide |
|---------|-------|----------|-------|
| Built-in | `truce-gui` / `truce-gpu` | Standard plugin UIs (knobs, sliders, meters) | [Built-in](../gui/built-in.md) |
| egui | `truce-egui` | Custom layouts, text input, graphs, third-party widgets | [egui](../gui/egui.md) |
| Vizia | `truce-vizia` | Reactive data binding, CSS-like styling | [Vizia](../gui/vizia.md) |
| Iced | `truce-iced` | Elm architecture, auto-generated or custom retained-mode UI | [Iced](../gui/iced.md) |
| Raw | `truce-core` | Full control — Metal, OpenGL, Skia, anything | [Raw window handle](../gui/raw-window-handle.md) |

All backends integrate the same way — override `custom_editor()` in
your `PluginLogic` implementation. See the [GUI guide](../gui/) for
the full comparison and integration details.

---

[← Previous](07-synth.md) | [Next →](09-hot-reload.md) | [Index](README.md)
