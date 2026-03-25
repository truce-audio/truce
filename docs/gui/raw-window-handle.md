# Raw Window Handle (Bring Your Own Renderer)

If none of the built-in backends fit your needs, you can implement the
`Editor` trait directly and render with whatever you want — Metal, OpenGL,
Vulkan, Skia, a web view, or anything that can draw into a child window.

## When to Use This

This is the nuclear option. Use it when:

- You have an existing rendering pipeline you want to reuse
- You need a web view (CEF, wry, etc.)
- You want to use a framework truce doesn't wrap (nannou, macroquad, etc.)
- You need absolute control over every pixel

For most plugins, the [built-in](built-in.md), [egui](egui.md),
[vizia](vizia.md), or [iced](iced.md) backends are better choices.

## The Editor Trait

Implement `Editor` from `truce-core`:

```rust
use truce_core::editor::{Editor, EditorContext, RawWindowHandle};

pub struct MyCustomEditor {
    size: (u32, u32),
    context: Option<EditorContext>,
    // Your renderer state...
}

impl Editor for MyCustomEditor {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: RawWindowHandle, context: EditorContext) {
        // parent is the host's window handle.
        // Create your child window/view inside it.
        //
        // RawWindowHandle variants:
        //   AppKit(*mut c_void)  — NSView* on macOS
        //   Win32(*mut c_void)   — HWND on Windows
        //   X11(u64)             — X11 Window ID on Linux

        self.context = Some(context);

        match parent {
            RawWindowHandle::AppKit(ns_view) => {
                // Create a child NSView or CAMetalLayer
                // Attach your renderer
            }
            RawWindowHandle::Win32(hwnd) => {
                // Create a child HWND
                // Attach your renderer
            }
            RawWindowHandle::X11(window_id) => {
                // Create a child X11 window
                // Attach your renderer
            }
        }
    }

    fn close(&mut self) {
        // Tear down your renderer and child window
        self.context = None;
    }

    fn idle(&mut self) {
        // Called ~60fps by the host. Repaint here.
        // Read parameter values via self.context and render.
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        // Host requests a resize. Return true if you accept.
        self.size = (width, height);
        true
    }

    fn can_resize(&self) -> bool {
        false // or true if your renderer supports it
    }

    fn set_scale_factor(&mut self, factor: f64) {
        // DPI changed (e.g., window moved between displays)
    }
}

// Must be Send
unsafe impl Send for MyCustomEditor {}
```

## Parameter Communication

Use the `EditorContext` to communicate with the host:

```rust
fn idle(&mut self) {
    let ctx = self.context.as_ref().unwrap();

    // Read current values — fields are Arc<dyn Fn> closures
    let gain = (ctx.get_param)(0);           // normalized 0.0–1.0
    let gain_plain = (ctx.get_param_plain)(0); // e.g., -60.0 to 6.0
    let gain_text = (ctx.format_param)(0);     // "0.0 dB"
    let meter_l = (ctx.get_meter)(100);        // 0.0–1.0

    // Render your UI with these values...

    // When the user interacts with a control:
    (ctx.begin_edit)(0);
    (ctx.set_param)(0, 0.75);  // normalized value
    (ctx.end_edit)(0);
}
```

Always use the begin/set/end sequence for drag gestures so the host
records automation correctly.

## Platform Patterns

### macOS (Metal)

The most common pattern on macOS is to create a `CAMetalLayer` as a
sublayer of the parent `NSView`:

```rust
// Pseudocode — actual implementation uses objc crate
let parent_layer: id = msg_send![ns_view, layer];
let metal_layer: id = msg_send![class!(CAMetalLayer), layer];
msg_send![parent_layer, addSublayer: metal_layer];

// Create wgpu surface from the metal layer
let surface = unsafe {
    instance.create_surface_unsafe(
        wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(metal_layer as *mut c_void)
    )
};
```

### macOS (baseview)

If you want windowing handled for you but still want custom rendering,
use baseview directly:

```rust
use baseview::{Window, WindowOpenOptions, Size};

fn open(&mut self, parent: RawWindowHandle, context: EditorContext) {
    let options = WindowOpenOptions {
        title: "My Plugin".into(),
        size: Size::new(self.size.0 as f64, self.size.1 as f64),
        scale: baseview::WindowScalePolicy::SystemScaleFactor,
        gl_config: None,
    };

    // Convert truce RawWindowHandle to baseview's expected format
    // Then open a child window with your custom handler
}
```

### Scale Factor

Query the display scale factor on macOS:

```rust
// Via truce-gui helper
let scale = truce_gui::backing_scale(); // 2.0 on Retina, 1.0 otherwise

// Or via objc directly from the NSView
let window: id = msg_send![ns_view, window];
let factor: f64 = msg_send![window, backingScaleFactor];
```

## Integrating Your Editor

Wire it up the same way as any other backend:

```rust
impl PluginLogic for MyPlugin {
    fn custom_editor(&self) -> Option<Box<dyn Editor>> {
        Some(Box::new(MyCustomEditor {
            size: (800, 600),
            context: None,
        }))
    }
}
```

The `custom_editor()` return value works with all formats (CLAP, VST3,
VST2, AU, AAX) without any format-specific code. The format wrappers
handle embedding the child window in the host.

## Reference

Look at how the existing backends implement `Editor` for real-world
examples:

| Backend | File | Approach |
|---------|------|----------|
| Built-in | `crates/truce-gui/src/editor.rs` | NSView + CALayer + CPU blit |
| GPU | `crates/truce-gpu/src/editor.rs` | baseview + wgpu |
| egui | `crates/truce-egui/src/editor.rs` | baseview + egui-wgpu |
| Vizia | `crates/truce-vizia/src/editor.rs` | baseview + vizia (Skia/GL) |
| Iced | `crates/truce-iced/src/editor.rs` | CAMetalLayer + iced-wgpu |
