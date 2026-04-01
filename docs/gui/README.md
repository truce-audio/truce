# GUI Backends

truce supports several GUI backends. They all produce the same plugin
formats (CLAP, VST3, VST2, AU, AAX) and share the same parameter
system, so switching between them is straightforward.

## Starting out

If you haven't built a plugin GUI before, start with the
**[built-in GUI](built-in.md)**. You define a layout in code and truce
renders the widgets for you — no custom editor code required.

```rust
fn layout(&self) -> GridLayout {
    GridLayout::build("MY PLUGIN", "V1.0", 2, 50.0, vec![widgets(vec![
        knob(P::Gain, "Gain"),
        knob(P::Pan, "Pan"),
    ])])
}
```

That gives you a working GUI with knobs, mouse interaction, automation,
and host integration. Most plugins start here.

## Moving to a framework

When you need something the built-in GUI doesn't support — text input,
tabs, custom drawing — pick a framework:

- **[egui](egui.md)** — immediate-mode. Write UI code that runs every
  frame. Large widget library, lots of third-party crates.
- **[Iced](iced.md)** — Elm architecture. Define a model, emit messages,
  update state, render a view. Good for complex state management.
- **[Slint](slint.md)** — declarative markup. Design your UI in `.slint`
  files, wire properties in Rust. Has an IDE live preview.

All three integrate the same way: override `custom_editor()` and return
your editor. The rest of your plugin (params, DSP, format export) stays
the same.

## If you need full control

For an existing rendering pipeline, web views, or anything else,
implement the `Editor` trait directly. See
**[raw window handle](raw-window-handle.md)**.

## Parameter communication

Every backend wraps the host's parameter callbacks in a `ParamState`
with the same API:

```rust
state.get(P::Gain)            // normalized 0.0-1.0
state.format(P::Gain)         // "0.0 dB"
state.meter(P::MeterLeft)     // level 0.0-1.0
state.set_immediate(P::Gain, 0.75)  // write (single action)
state.begin_gesture(P::Gain)        // write (start drag)
state.set_value(P::Gain, 0.75)      // write (during drag)
state.end_gesture(P::Gain)          // write (end drag)
```

The gesture protocol tells the DAW that the user is dragging a control,
so it records smooth automation. For single-click actions (toggles),
`set_immediate()` handles everything.

## Screenshot testing

All backends support headless rendering for visual regression tests.
See **[screenshot testing](screenshot-testing.md)**.
