# Iced Integration

[Iced](https://github.com/iced-rs/iced) is a retained-mode GUI framework
following the Elm architecture: model, message, update, view. `truce-iced`
wraps it as a truce editor with two modes — auto-generated UI from your
layout, or fully custom UI via the `IcedPlugin` trait.

## When to use Iced

Pick Iced when you want:

- Elm-architecture state management (messages drive all state changes)
- Auto-generated UI from your existing `layout()` — zero custom code
- Canvas-based custom drawing with wgpu
- A retained-mode alternative to egui's immediate mode

## Setup

Add these to your plugin's `Cargo.toml`:

```toml
[dependencies]
truce-iced = { workspace = true }
iced = { version = "0.13", default-features = false, features = ["canvas", "wgpu"] }
```

## Auto mode (zero custom code)

Generate a UI directly from your `GridLayout`:

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

This renders the same widgets as the built-in GUI (knob, slider, toggle,
selector, dropdown, meter, XY pad) but using iced's rendering pipeline.
No `IcedPlugin` trait needed.

## Custom mode

For full control, implement `IcedPlugin`:

```rust
use std::sync::Arc;
use truce_iced::{
    knob, meter, xy_pad,
    IcedEditor, IcedPlugin, IntoElement,
    Message, ParamState, EditorHandle,
};
use iced::widget::{container, text, Column, Row};
use iced::{Element, Task};

use MyParamsParamId as P;

// Your custom message type (use () if you don't need any)
#[derive(Debug, Clone)]
pub enum Msg {}

pub struct MyEditor;

impl IcedPlugin<MyParams> for MyEditor {
    type Message = Msg;

    fn new(_params: Arc<MyParams>) -> Self { Self }

    // update() has a default no-op — only override if you handle custom messages

    fn view<'a>(
        &'a self,
        params: &'a ParamState<MyParams>,
    ) -> Element<'a, Message<Msg>> {
        Column::new()
            .push(text("MY PLUGIN").size(14))
            .push(
                Row::new()
                    .push(knob(P::Gain, params).label("Gain").size(60.0).el())
                    .push(knob(P::Pan, params).label("Pan").size(60.0).el())
                    .spacing(10)
            )
            .spacing(10)
            .padding(10)
            .into()
    }
}
```

Then in `custom_editor()`:

```rust
fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
    Some(Box::new(IcedEditor::<MyParams, MyEditor>::new(
        Arc::new(self.params.clone()),
        (400, 300),
    )))
}
```

### The `.el()` shorthand

Iced normally requires verbose type conversions when pushing widgets
into `Row` and `Column`. truce-iced provides `.el()` via the
`IntoElement` trait to simplify this:

```rust
use truce_iced::{knob, meter, IntoElement};

// Instead of: .push(Into::<Element<'a, Message<Msg>>>::into(knob(...)))
// Just write:
Row::new()
    .push(knob(P::Gain, params).label("Gain").size(60.0).el())
    .push(meter(&[P::MeterLeft, P::MeterRight], params).size(16.0, 200.0).el())
```

Import `IntoElement` and call `.el()` on any truce-iced widget.

## Reading and writing parameters

`ParamState<P>` provides type-safe access to your parameters:

```rust
// Read
let gain = params.get(P::Gain);           // normalized 0.0-1.0
let gain_db = params.get_plain(P::Gain);  // plain value
let text = params.label(P::Gain);         // formatted string
let level = params.meter(P::MeterLeft);   // meter level

// Write — one shot (toggles, selectors)
params.set_immediate(P::Gain, 0.75);

// Write — gesture (knobs, sliders)
params.begin_gesture(P::Gain);
params.set_value(P::Gain, new_value);
params.end_gesture(P::Gain);
```

## Widgets

`truce-iced` provides canvas-based widgets that handle the gesture
protocol automatically:

```rust
use truce_iced::{knob, param_slider, param_toggle, param_selector, xy_pad, meter};

knob(P::Gain, params).label("Gain").size(60.0)
param_slider(P::Pan, params).label("Pan")
param_toggle(P::Bypass, params).label("Bypass")
param_selector(P::Mode, params).label("Mode")
xy_pad(P::Pan, P::Gain, params).label("XY").size(130.0)
meter(&[P::MeterLeft, P::MeterRight], params).size(16.0, 200.0)
```

All widgets use the builder pattern. Call `.el()` to convert to an
iced `Element` for layout.

## Message routing

`truce-iced` wraps your custom messages in a `Message<M>` enum:

```rust
pub enum Message<M> {
    Param(ParamMessage),   // handled internally by truce-iced
    Custom(M),             // your messages — handle in update()
}
```

Parameter messages (knob drags, toggle clicks) are routed automatically.
You only handle `Message::Custom(...)` in your `update()` method — and
if you don't have custom messages, you don't need `update()` at all
(the default is a no-op).

## EditorHandle

For programmatic parameter changes from `update()`:

```rust
fn update(&mut self, msg: Message<Msg>, params: &ParamState<MyParams>, ctx: &EditorHandle) -> Task<Message<Msg>> {
    match msg {
        Message::Custom(Msg::ResetGain) => {
            ctx.begin_edit(P::Gain.into());
            ctx.set_param(P::Gain.into(), 0.5);
            ctx.end_edit(P::Gain.into());
        }
        _ => {}
    }
    Task::none()
}
```

## Theming

Override `theme()` on your `IcedPlugin`:

```rust
fn theme(&self) -> iced::Theme {
    truce_iced::theme::truce_dark_theme()  // default
}
```

## Screenshot testing

```rust
#[test]
fn gui_snapshot_iced() {
    let params = Arc::new(MyParams::new());
    let (pixels, w, h) = truce_iced::snapshot::render_iced_screenshot::<MyParams, MyEditor>(
        params,
        (WINDOW_W, WINDOW_H),   // same constants as your editor
        2.0,                     // scale
        Some(("JetBrains Mono", truce_gui::font::JETBRAINS_MONO)),
    );
    truce_test::assert_gui_snapshot_raw("my_plugin_iced_default", &pixels, w, h, 0);
}
```

See [screenshot testing](screenshot-testing.md) for details.

## Complete example

See `examples/gain-iced/` for a working plugin with custom header,
knobs, XY pad, level meter, `.el()` usage, and screenshot test.
