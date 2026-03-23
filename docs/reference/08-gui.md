## Adding a GUI

> The built-in GUI system (`truce-gui`) is under active
> development. The API described here reflects the current state.

### Rendering

The default rendering backend is CPU-based (tiny-skia). A wgpu-based
GPU backend is available behind the `gpu` feature flag — enable it
with `cargo xtask install --gpu`. When the GPU backend is enabled,
it is used automatically; if GPU initialization fails at runtime, the
CPU backend is used as a fallback.

Text is rendered using fontdue (TrueType rasterization) with JetBrains
Mono Regular embedded at compile time. Glyphs are cached and
anti-aliased. Any font size is supported.

### Option A: Built-in declarative GUI

The built-in GUI uses the `layout!` macro to declare rows of widgets.
Define a public function in your `lib.rs`:

```rust
use GainParamsParamId as P;

pub fn gui_layout() -> truce_gui::layout::PluginLayout {
    truce_gui::layout!("MY GAIN", "V0.1", 80.0, {
        row {
            knob(P::Gain, "Gain")
            slider(P::Pan, "Pan")
            toggle(P::Bypass, "Bypass")
        }
    })
}
```

Available widgets: `knob`, `slider`, `toggle`, `selector`, `meter`,
`xy_pad`. Use `.span(N)` for multi-column widgets. Widget type is
auto-detected from param range (bool → toggle, enum → selector) but
can be overridden explicitly.

For complex layouts needing row spanning (tall meters, 2D XY pads),
use the `grid!` macro instead:

```rust
use GainParamsParamId as P;

// Define meter IDs as a #[repr(u32)] enum
#[repr(u32)]
#[derive(Clone, Copy)]
pub enum Meter { Left = 100, Right = 101 }
impl From<Meter> for u32 { fn from(m: Meter) -> u32 { m as u32 } }

pub fn gui_layout() -> truce_gui::layout::GridLayout {
    truce_gui::grid!("MY GAIN", "V0.1", cols: 3, cell: 80.0, {
        knob(P::Gain, "Gain")
        slider(P::Pan, "Pan")
        toggle(P::Bypass, "Bypass")

        xy_pad(P::Pan, P::Gain, "XY")
        meter(&[Meter::Left.into(), Meter::Right.into()], "Level")
    })
}
```

Widgets auto-flow left-to-right. Use `.cols(N)` and `.rows(N)` for
spanning. `section("LABEL")` inserts a labeled row break.

### Option B: iced backend

`truce-iced` provides an [iced](https://iced.rs) 0.13 GUI backend with
two modes:

- **Auto mode** — generates an iced UI from your `PluginLayout`,
  similar to the built-in GUI but rendered with iced.
- **Custom mode** — implement the `IcedPlugin` trait for full control
  over the iced `Application`.

See the `gain-iced` example for a working plugin.

### Option C: egui backend

See [egui Integration](12-egui.md) for full egui support via
`truce-egui`.

### Option D: Bring your own GUI

Use any Rust GUI framework. The `Editor` trait gives you a raw
window handle:

```rust
use truce::prelude::*;

pub struct MyCustomEditor {
    // your state here
}

impl Editor for MyCustomEditor {
    fn size(&self) -> (u32, u32) {
        (600, 400)
    }

    fn open(&mut self, parent: RawWindowHandle, ctx: EditorContext) {
        // `parent` is the host-provided window (NSView on macOS,
        // HWND on Windows, X11 window on Linux).
        //
        // Create your GUI framework's window as a child of `parent`.
    }

    fn close(&mut self) {
        // Tear down your GUI
    }

    fn idle(&mut self) {
        // Called ~60fps by the host on the UI thread.
    }
}
```

---


---

[← Previous](07-synth.md) | [Next →](09-hot-reload.md) | [Index](README.md)
