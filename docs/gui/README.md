# GUI Backends

truce supports multiple GUI backends. Every backend implements the same
`Editor` trait from `truce-core`, so your choice of GUI has zero impact
on DSP code, parameter handling, or format compatibility — all backends
work with CLAP, VST3, VST2, AU, and AAX.

## Choosing a Backend

| Backend | Crate | Rendering | Best for |
|---------|-------|-----------|----------|
| [Built-in](built-in.md) | `truce-gui` / `truce-gpu` | tiny-skia (CPU) or wgpu (GPU) | Standard plugin UIs — knobs, sliders, meters. Zero custom code. |
| [egui](egui.md) | `truce-egui` | wgpu via egui-wgpu | Custom layouts, text input, graphs, tables, third-party egui widgets |
| [Vizia](vizia.md) | `truce-vizia` | Skia/GL via vizia | Reactive/declarative UIs, CSS-like styling, complex widget trees |
| [Iced](iced.md) | `truce-iced` | wgpu/Metal | Elm-architecture fans, auto-generated or fully custom retained-mode UIs |
| [Raw window handle](raw-window-handle.md) | `truce-core` | Bring your own | Full control — Metal, OpenGL, Skia, HTML canvas, anything |

## How It Works

Every GUI backend implements the `Editor` trait defined in `truce-core`:

```rust
pub trait Editor: Send {
    fn size(&self) -> (u32, u32);
    fn open(&mut self, parent: RawWindowHandle, context: EditorContext);
    fn close(&mut self);
    fn idle(&mut self) {}
    fn set_size(&mut self, width: u32, height: u32) -> bool;
    fn can_resize(&self) -> bool;
    fn scale_factor(&self) -> f64 { 1.0 }
    fn set_scale_factor(&mut self, factor: f64);
}
```

The host calls `open()` with a parent window handle and an `EditorContext`
for parameter communication. The editor creates its UI as a child of the
host window.

## Integration Pattern

All backends follow the same pattern — override `custom_editor()` in your
`PluginLogic` implementation:

```rust
impl PluginLogic for MyPlugin {
    // DSP code stays the same regardless of GUI backend...

    fn custom_editor(&self) -> Option<Box<dyn Editor>> {
        Some(Box::new(/* your editor here */))
    }
}
```

If you don't override `custom_editor()`, the built-in GUI is used
automatically based on your `layout()` return value.

## EditorContext

All backends receive an `EditorContext` that bridges parameter changes
to the host. The fields are `Arc<dyn Fn>` closures, so call syntax
uses parentheses around the field: `(ctx.begin_edit)(id)`.

| Field | Call syntax | Description |
|-------|-------------|-------------|
| `begin_edit` | `(ctx.begin_edit)(id)` | User starts dragging a control |
| `set_param` | `(ctx.set_param)(id, normalized)` | Value changes during drag |
| `end_edit` | `(ctx.end_edit)(id)` | User releases the control |
| `request_resize` | `(ctx.request_resize)(w, h) -> bool` | Request a window resize |
| `get_param` | `(ctx.get_param)(id) -> f64` | Read normalized value (0.0–1.0) |
| `get_param_plain` | `(ctx.get_param_plain)(id) -> f64` | Read plain value (native range) |
| `format_param` | `(ctx.format_param)(id) -> String` | Host-formatted display string |
| `get_meter` | `(ctx.get_meter)(id) -> f32` | Read meter level |

For single-shot changes (toggles, selectors), call all three in
sequence: `begin_edit` → `set_param` → `end_edit`.

Most GUI backends wrap `EditorContext` in a higher-level `ParamState`
that provides a cleaner API (e.g., `state.get(id)`, `state.begin_gesture(id)`).
You only need the raw closure syntax when implementing the `Editor` trait
directly (see [raw window handle](raw-window-handle.md)).

## Guides

- [Built-in GUI](built-in.md) — zero-code layout-driven UI
- [egui](egui.md) — immediate-mode UI with full widget library
- [Vizia](vizia.md) — reactive declarative UI with data binding
- [Iced](iced.md) — Elm-architecture retained-mode UI
- [Raw window handle](raw-window-handle.md) — bring your own renderer
