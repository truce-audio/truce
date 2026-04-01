# Iced Integration

[Iced](https://github.com/iced-rs/iced) is a retained-mode GUI framework
following the Elm architecture. `truce-iced` wraps it so you can use
iced's layout system and canvas widgets inside a plugin window.

## Getting started

Add the dependencies:

```toml
[dependencies]
truce-iced = { workspace = true }
iced = { version = "0.13", default-features = false, features = ["canvas", "wgpu"] }
```

The simplest approach is auto mode — generate a UI from your layout
with no custom code:

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

This renders the same widgets as the built-in GUI but through iced's
rendering pipeline.

## Custom UI

For full control, implement `IcedPlugin`. Here's a minimal example:

```rust
use std::sync::Arc;
use truce_iced::{
    knob, meter, IcedEditor, IcedPlugin, IntoElement,
    Message, ParamState,
};
use iced::widget::{Column, Row, text};
use iced::Element;
use MyParamsParamId as P;

#[derive(Debug, Clone)]
pub enum Msg {}

pub struct MyEditor;

impl IcedPlugin<MyParams> for MyEditor {
    type Message = Msg;

    fn new(_params: Arc<MyParams>) -> Self { Self }

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

Wire it up:

```rust
fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
    Some(Box::new(IcedEditor::<MyParams, MyEditor>::new(
        Arc::new(self.params.clone()),
        (400, 300),
    )))
}
```

A few things to note:

- `type Message = Msg` is your custom message enum. Use `()` if you
  don't need custom messages.
- `new()` is required. `update()` defaults to a no-op if you don't
  override it.
- `.el()` converts a truce-iced widget into an iced `Element`. Import
  `IntoElement` to use it.

## The `.el()` shorthand

Iced's type system requires explicit conversions when pushing widgets
into rows and columns. Without `.el()` you'd write:

```rust
.push(Into::<Element<'a, Message<Msg>>>::into(knob(P::Gain, params).label("Gain")))
```

With `.el()`:

```rust
.push(knob(P::Gain, params).label("Gain").size(60.0).el())
```

Import `IntoElement` from `truce_iced` and call `.el()` on any widget.

## Reading and writing parameters

```rust
// Read
params.get(P::Gain)          // normalized 0.0-1.0
params.get_plain(P::Gain)    // plain value
params.label(P::Gain)        // formatted string
params.meter(P::MeterLeft)   // meter level

// Write (click)
params.set_immediate(P::Gain, 0.75)

// Write (drag)
params.begin_gesture(P::Gain)
params.set_value(P::Gain, new_value)
params.end_gesture(P::Gain)
```

## Widgets

```rust
use truce_iced::{knob, param_slider, param_toggle, param_selector, xy_pad, meter};

knob(P::Gain, params).label("Gain").size(60.0)
param_slider(P::Pan, params).label("Pan")
param_toggle(P::Bypass, params).label("Bypass")
param_selector(P::Mode, params).label("Mode")
xy_pad(P::Pan, P::Gain, params).label("XY").size(130.0)
meter(&[P::MeterLeft, P::MeterRight], params).size(16.0, 200.0)
```

All use builder pattern. Call `.el()` to push into iced layouts.

## Handling custom messages

If your UI has buttons, tabs, or other interactive elements, define
messages and handle them in `update()`:

```rust
#[derive(Debug, Clone)]
pub enum Msg {
    ResetGain,
    TabChanged(usize),
}

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

Parameter messages (from knob drags, toggle clicks) are handled
automatically — you never see them in `update()`.

## Custom state

If your plugin has persistent state beyond parameters, use
`StateBinding<T>` in your `IcedPlugin` model:

```rust
#[derive(State, Default)]
pub struct MyState {
    pub instance_name: String,
}

pub struct MyEditor {
    state: StateBinding<MyState>,
}

impl IcedPlugin<MyParams> for MyEditor {
    type Message = ();

    fn new(_params: Arc<MyParams>) -> Self {
        Self { state: StateBinding::default() }
    }

    fn view<'a>(&'a self, params: &'a ParamState<MyParams>) -> Element<'a, Message<()>> {
        text(&self.state.get().instance_name).into()
    }

    fn state_changed(&mut self) {
        self.state.sync();
    }
}
```

`state_changed` is called when the DAW restores state (preset recall,
undo, session load). It re-reads the custom state from the plugin so
the UI stays in sync.

If your plugin only uses `#[param]` fields, you don't need any of this —
parameter values sync automatically through `ParamState`.

## Screenshot testing

```rust
#[test]
fn gui_snapshot_iced() {
    let params = Arc::new(MyParams::new());
    let (pixels, w, h) = truce_iced::snapshot::render_iced_screenshot::<MyParams, MyEditor>(
        params,
        (WINDOW_W, WINDOW_H),
        2.0,
        Some(("JetBrains Mono", truce_gui::font::JETBRAINS_MONO)),
    );
    truce_test::assert_gui_snapshot_raw("my_plugin_iced_default", &pixels, w, h, 0);
}
```

See [screenshot testing](screenshot-testing.md) for more.

## Example

`examples/gain-iced/` has a complete plugin with custom header, knobs,
XY pad, meter, `.el()` usage, and screenshot test.
