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

impl Plugin for MyPlugin {
    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> {
        Some(Box::new(IcedEditor::from_layout(
            self.params.clone(),
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
    Message, ParamCache,
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
        params: &'a ParamCache<MyParams>,
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
        self.params.clone(),
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

`ParamCache<P>` is a per-tick read-only snapshot — iced's `view`
function can't have side effects, so the cache is what widgets read
from. The host bridge (gestures, automation writes) lives on
`PluginContext<P>` and is passed to `update()`:

```rust
// Read — from ParamCache (passed to view + update)
params.get(P::Gain)          // normalized 0.0-1.0
params.get_plain(P::Gain)    // plain value
params.label(P::Gain)        // formatted string
params.meter(P::MeterLeft)   // meter level

// Write — from PluginContext (passed to update)
ctx.automate(P::Gain, 0.75)        // begin + set + end (one shot)
ctx.begin_edit(P::Gain)            // gesture: start
ctx.set_param(P::Gain, new_value)  // gesture: in progress
ctx.end_edit(P::Gain)              // gesture: end
```

The built-in widgets (`knob`, `param_slider`, `param_toggle`, etc.)
emit their own `Message::Param(...)` variants — the iced runtime
forwards those to the host via the underlying `PluginContext`, so
direct writes are only needed for custom widgets and `Msg::*` handling.

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

fn update(
    &mut self,
    msg: Message<Msg>,
    params: &ParamCache<MyParams>,
    ctx: &PluginContext<MyParams>,
) -> Task<Message<Msg>> {
    match msg {
        Message::Custom(Msg::ResetGain) => {
            // `automate` collapses the begin / set / end triple
            // into one call for single-shot edits.
            ctx.automate(P::Gain, 0.5);
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
`StateBinding<T>` in your `IcedPlugin` model. Initialize it as
`default()` and sync on first update:

```rust
#[derive(State, Default)]
pub struct MyState {
    pub instance_name: String,
}

pub struct MyEditor {
    state: StateBinding<MyState>,
    initialized: bool,
}

impl IcedPlugin<MyParams> for MyEditor {
    type Message = ();

    fn new(_params: Arc<MyParams>) -> Self {
        Self { state: StateBinding::default(), initialized: false }
    }

    fn update(
        &mut self,
        _msg: Message<()>,
        _params: &ParamCache<MyParams>,
        ctx: &PluginContext<MyParams>,
    ) -> Task<Message<()>> {
        if !self.initialized {
            self.state = StateBinding::new(ctx.clone().dyn_erase());
            self.initialized = true;
        }
        Task::none()
    }

    fn view<'a>(
        &'a self,
        _params: &'a ParamCache<MyParams>,
    ) -> Element<'a, Message<()>> {
        text(&self.state.get().instance_name).into()
    }

    fn state_changed(&mut self) {
        self.state.sync();
    }
}
```

`state_changed()` is called when the DAW restores state (preset recall,
undo, session load). It re-reads custom state so the UI stays in sync.
To write state from the GUI:

```rust
self.state.update(|s| s.instance_name = new_name);
```

You can also access state directly via `ctx.get_state()` /
`ctx.set_state()` on the `PluginContext` passed to `update()`.

If your plugin only uses `#[param]` fields, you don't need any of this —
parameter values sync automatically through `ParamCache`.

## Screenshot testing

```rust
#[test]
fn gui_screenshot() {
    truce_test::screenshot!(Plugin, "screenshots/default.png").run();
}
```

See [screenshot testing](screenshot-testing.md) for more.

## Example

`examples/truce-example-gain-iced/` has a complete plugin with custom header, knobs,
XY pad, meter, `.el()` usage, and screenshot test.
