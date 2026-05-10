# Raw Window Handle (Bring Your Own Renderer)

If none of the built-in backends fit, you can implement the `Editor`
trait directly. The host gives you a window handle — what you draw
inside it is entirely up to you.

## When to reach for this

This is the lowest level of GUI integration. You probably want it if:

- You have an existing rendering pipeline (Metal, OpenGL, Skia)
- You want a web view (CEF, wry)
- You need a framework truce doesn't wrap yet
- You want absolute pixel-level control

For most plugins, start with the [built-in GUI](built-in.md) or one of
the framework integrations ([egui](egui.md), [iced](iced.md),
[slint](slint.md)).

## Implementing the Editor trait

Here's the minimum implementation. The host calls `open()` with a parent
window handle, and you create your UI as a child of that window:

```rust
use truce_core::editor::{Editor, PluginContext, RawWindowHandle};

pub struct MyEditor {
    size: (u32, u32),
    context: Option<PluginContext>,
    // your renderer state goes here
}

impl Editor for MyEditor {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        self.context = Some(context);

        // parent tells you what kind of window to create:
        match parent {
            RawWindowHandle::AppKit(ns_view) => {
                // ns_view is an NSView* — create a child NSView or CAMetalLayer
            }
            RawWindowHandle::Win32(hwnd) => {
                // hwnd is an HWND — create a child window
            }
            RawWindowHandle::X11(window_id) => {
                // window_id is an X11 Window — create a child window
            }
        }
    }

    fn close(&mut self) {
        // tear down your renderer and child window
        self.context = None;
    }

    fn idle(&mut self) {
        // called ~60fps by the host — repaint here
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        self.size = (width, height);
        true
    }

    fn can_resize(&self) -> bool { false }
    fn set_scale_factor(&mut self, _factor: f64) {
        // Hosts deliver content scale here on Windows VST3
        // (`IPlugViewContentScaleSupport`) and CLAP
        // (`clap_plugin_gui::set_scale`). macOS hosts rarely call
        // it — AppKit handles Retina backing automatically. Resize
        // your off-screen buffers to physical pixels here if you
        // need them.
    }
}

unsafe impl Send for MyEditor {}
```

## Reading and writing parameters

`PluginContext` exposes its host bridge as methods. IDs use
`#[derive(Params)]`'s generated `*ParamId` enum and convert to `u32`
through `impl Into<u32>`, so you can pass either the enum variant or a
literal `u32`:

```rust
fn idle(&mut self) {
    let ctx = self.context.as_ref().unwrap();

    // read current values
    let gain = ctx.get_param(P::Gain);            // normalized 0.0-1.0
    let gain_plain = ctx.get_param_plain(P::Gain); // e.g., -60.0 to 6.0
    let gain_text = ctx.format_param(P::Gain);     // "0.0 dB"
    let meter_l = ctx.get_meter(P::MeterLeft);     // 0.0-1.0

    // render your UI with these values...

    // when the user drags a control:
    ctx.begin_edit(P::Gain);
    ctx.set_param(P::Gain, 0.75);   // normalized value
    ctx.end_edit(P::Gain);

    // ...or, for click-to-toggle / single-shot edits, the
    // `begin_edit + set_param + end_edit` triple collapses to:
    ctx.automate(P::Bypass, 1.0);
}
```

Always wrap drag gestures in `begin_edit` / `end_edit` so the host
records automation correctly — `automate(id, val)` is the convenience
wrapper for one-shot edits where the gesture and value arrive together.

## Connecting to your plugin

Same as every other backend:

```rust
impl PluginLogic for MyPlugin {
    fn custom_editor(&self) -> Option<Box<dyn Editor>> {
        Some(Box::new(MyEditor {
            size: (800, 600),
            context: None,
        }))
    }
}
```

Works with all formats (CLAP, VST3, VST2, LV2, AU, AAX) without
any format-specific code.

## Useful helpers

### Scale factor

Query the display scale on macOS (2.0 on Retina, 1.0 otherwise):

```rust
let scale = truce_gui::backing_scale();
```

### baseview for windowing

If you want cross-platform window management without building it
yourself, use baseview directly. It handles macOS/Windows/Linux child
window creation, event dispatch, and DPI scaling:

```rust
use baseview::{Window, WindowOpenOptions, Size, WindowScalePolicy};

fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
    let options = WindowOpenOptions {
        title: String::from("My Plugin"),
        size: Size::new(800.0, 600.0),
        scale: WindowScalePolicy::SystemScaleFactor,
    };
    // open a child window with your custom WindowHandler
}
```

See the truce-egui and truce-slint source code for complete baseview
integration examples.

## Reference implementations

The existing backends are good examples of real `Editor` implementations:

| Backend | Source | Approach |
|---------|--------|----------|
| Built-in | `crates/truce-gui/src/editor.rs` | baseview + wgpu + CPU pixel blit |
| GPU | `crates/truce-gpu/src/editor.rs` | baseview + wgpu + GPU rendering |
| egui | `crates/truce-egui/src/editor.rs` | baseview + egui-wgpu |
| Iced | `crates/truce-iced/src/editor.rs` | baseview + iced-wgpu |
| Slint | `crates/truce-slint/src/editor.rs` | baseview + software renderer + wgpu blit |
