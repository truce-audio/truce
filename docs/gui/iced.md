# Iced Integration

[Iced](https://github.com/iced-rs/iced) is a retained-mode GUI framework
following the Elm architecture (model → message → update → view).
`truce-iced` wraps it as a truce `Editor` with two modes: auto-generated
UI from your `GridLayout`, or fully custom UI via the `IcedPlugin` trait.

## When to Use Iced

Use iced when you want:

- Elm-architecture state management (message-driven updates)
- Auto-generated UI from your existing `layout()` definition
- Iced's widget library and layout primitives
- Custom canvas drawing with wgpu

## Setup

Add `truce-iced` and `iced` to your plugin's `Cargo.toml`:

```toml
[dependencies]
truce-iced = { workspace = true }
iced = { version = "0.13", default-features = false, features = ["canvas", "wgpu"] }
```

## Auto Mode

Generate a UI directly from your `GridLayout` with zero custom code:

```rust
use truce::prelude::*;
use truce_iced::IcedEditor;

impl PluginLogic for MyPlugin {
    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        Some(Box::new(IcedEditor::from_layout(
            Arc::new(self.params.clone()),
            self.layout(),
        )))
    }
}
```

This renders the same 6 widget types as the built-in GUI (knob, slider,
toggle, selector, meter, XY pad) but using iced's rendering pipeline.

## Custom Mode (IcedPlugin Trait)

For full control, implement the `IcedPlugin` trait:

```rust
use truce_iced::{IcedEditor, IcedPlugin, EditorHandle, ParamState};
use truce_iced::widgets::knob;
use iced::widget::{column, row, text};
use iced::Element;

use MyParamsParamId as P;

#[derive(Debug, Clone)]
enum Msg {
    // Your custom messages here
}

struct MyEditor;

impl IcedPlugin<MyParams> for MyEditor {
    type Message = Msg;

    fn new(params: Arc<MyParams>) -> Self {
        Self
    }

    fn update(
        &mut self,
        msg: Message<Msg>,
        params: &ParamState<MyParams>,
        ctx: &EditorHandle,
    ) -> Task<Message<Msg>> {
        // Handle your custom messages
        Task::none()
    }

    fn view<'a>(
        &'a self,
        params: &'a ParamState<MyParams>,
    ) -> Element<'a, Message<Msg>> {
        column![
            text("My Plugin").size(24),
            row![
                knob(P::Gain, params).label("Gain"),
                knob(P::Pan, params).label("Pan"),
            ].spacing(20),
        ]
        .padding(20)
        .into()
    }
}
```

Then in `custom_editor()`:

```rust
fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
    Some(Box::new(IcedEditor::<MyParams, MyEditor>::new(
        Arc::new(self.params.clone()),
        (640, 480),
    )))
}
```

## ParamState

`ParamState<P>` provides type-safe parameter access:

| Method | Description |
|--------|-------------|
| `params.get(id)` | Normalized value (0.0–1.0) |
| `params.get_plain(id)` | Plain value |
| `params.label(id)` | Formatted display string |
| `params.meter(id)` | Meter level (0.0–1.0) |
| `params.set_immediate(id, v)` | Set value in one shot |
| `params.begin_gesture(id)` | Start a drag gesture |
| `params.set_value(id, v)` | Update during a drag |
| `params.end_gesture(id)` | End a drag gesture |

## Widgets

`truce-iced` provides parameter-aware canvas widgets:

```rust
use truce_iced::widgets::{knob, param_slider, param_toggle, xy_pad, meter};

// Knob with builder pattern
knob(P::Gain, params).label("Gain").size(80)

// Other widgets
param_slider(P::Pan, params)
param_toggle(P::Bypass, params)
xy_pad(P::Pan, P::Gain, params)
meter(&[P::MeterLeft, P::MeterRight], params)
```

These are canvas-based widgets that handle the gesture protocol
automatically.

## Message Routing

Iced uses a message-driven architecture. `truce-iced` wraps your custom
messages in a `Message<M>` enum that also handles parameter events
internally:

```rust
pub enum Message<M> {
    Param(ParamMessage),   // handled by truce-iced internally
    Custom(M),             // your messages
}
```

In your `update()` method, you only need to handle `Message::Custom(_)`
variants — parameter messages are routed automatically.

## EditorHandle

`EditorHandle` provides non-blocking communication with the host:

```rust
fn update(&mut self, msg: Message<Msg>, params: &ParamState<MyParams>, ctx: &EditorHandle) -> Task<Message<Msg>> {
    match msg {
        Message::Custom(Msg::ResetGain) => {
            ctx.begin_edit(P::Gain.into());
            ctx.set_param(P::Gain.into(), 0.5); // normalized
            ctx.end_edit(P::Gain.into());
        }
        _ => {}
    }
    Task::none()
}
```

## Theming

Override the `theme()` method on `IcedPlugin` to customize the iced theme:

```rust
fn theme(&self) -> iced::Theme {
    iced::Theme::Dark
}
```

## Snapshot Testing

Iced UIs can be snapshot-tested using the same infrastructure as
the built-in GUI. `truce-test` provides `assert_gui_snapshot_raw()`
which accepts raw RGBA pixels from any backend.

## Example

See `examples/gain-iced/` for a complete working
example with auto mode and custom widgets.
