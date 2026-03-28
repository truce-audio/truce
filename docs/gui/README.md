# GUI Backends

truce gives you multiple ways to build your plugin's user interface.
Every option produces the same CLAP, VST3, VST2, AU, and AAX output —
your choice of GUI framework has zero impact on DSP, parameters, or
format compatibility.

## Which one should I use?

If you're getting started, **use the built-in GUI**. You don't have to
write any UI code — just define a layout and truce draws the knobs,
sliders, and meters for you.

If you outgrow it, pick the framework that matches your style:

| Backend | Crate | Best for |
|---------|-------|----------|
| **[Built-in](built-in.md)** | `truce-gui` | Standard plugin UIs. Zero custom code. Just define a layout. |
| **[egui](egui.md)** | `truce-egui` | Custom layouts, text input, graphs, any third-party egui widget. |
| **[Iced](iced.md)** | `truce-iced` | Elm-architecture fans. Message-driven state management. |
| **[Slint](slint.md)** | `truce-slint` | Declarative `.slint` markup with IDE live preview. |
| **[Raw window handle](raw-window-handle.md)** | `truce-core` | Full control — Metal, OpenGL, web views, anything. |

You can switch between backends at any time without touching your DSP
code. The only thing that changes is the `custom_editor()` method.

## How it works

Every GUI backend implements the `Editor` trait defined in `truce-core`.
The host calls `open()` with a parent window handle, and the editor
creates its UI as a child window. Parameters flow through an
`EditorContext` that bridges your knobs and sliders to the DAW's
automation system.

You don't need to worry about this plumbing — the backends handle it
for you. But if you're curious (or building a custom backend), here's
the trait:

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

## Connecting a GUI to your plugin

All backends follow the same pattern. Override `custom_editor()` in your
`PluginLogic`:

```rust
impl PluginLogic for MyPlugin {
    // DSP code stays exactly the same...

    fn custom_editor(&self) -> Option<Box<dyn Editor>> {
        Some(Box::new(/* your editor here */))
    }
}
```

If you don't override `custom_editor()`, the built-in GUI is used
automatically based on your `layout()` return value. That's the
zero-code path — most plugins start here and never leave.

## Parameter communication

All backends share the same `EditorContext` for talking to the host.
You rarely need to use it directly — each backend wraps it in a
friendlier `ParamState` API:

```rust
// Read values
let gain = state.get(P::Gain);           // normalized 0.0-1.0
let gain_text = state.format(P::Gain);   // "0.0 dB"
let meter_l = state.meter(P::MeterLeft); // level 0.0-1.0

// Write values (click/toggle — one shot)
state.set_immediate(P::Gain, 0.75);

// Write values (drag — gesture protocol)
state.begin_gesture(P::Gain);
state.set_value(P::Gain, 0.75);  // call repeatedly during drag
state.end_gesture(P::Gain);
```

The gesture protocol (`begin_gesture` / `set_value` / `end_gesture`)
tells the DAW that the user is dragging a control, so it records smooth
automation rather than a series of discrete jumps. For click interactions
(toggles, selectors), `set_immediate()` handles the full sequence.

## Screenshot testing

All backends support headless screenshot tests — render the GUI without
a window and compare pixel-by-pixel against a reference PNG. See
[screenshot testing](screenshot-testing.md) for details.

## Guides

- **[Built-in GUI](built-in.md)** — start here. Zero-code layout-driven UI.
- **[egui](egui.md)** — immediate-mode UI with a huge widget library.
- **[Iced](iced.md)** — Elm-architecture retained-mode UI.
- **[Slint](slint.md)** — declarative `.slint` markup compiled at build time.
- **[Raw window handle](raw-window-handle.md)** — bring your own renderer.
- **[Screenshot testing](screenshot-testing.md)** — pixel-perfect visual regression tests.
