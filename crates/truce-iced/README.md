# truce-iced

Iced GUI backend for truce audio plugins.

## Overview

Provides an alternative GUI backend using
[Iced](https://github.com/iced-rs/iced)'s retained-mode widget toolkit with
its Elm-inspired architecture. Use this when you want a declarative,
message-driven UI with Iced's layout engine and widget ecosystem.

`AutoPlugin` can auto-generate a parameter UI from a `GridLayout`, while
`IcedPlugin` gives full control for custom designs.

## Key types

- **`IcedEditor`** -- the `Editor` implementation
- **`IcedPlugin`** -- trait for defining a fully custom iced UI (view, update, message)
- **`AutoPlugin`** -- auto-generated UI from parameter definitions and `GridLayout`

## Usage

```rust
struct MyIcedUi;

impl<P: Params> IcedPlugin<P> for MyIcedUi {
    type Message = MyMessage;

    fn new(params: Arc<P>) -> Self { /* build initial model */ }

    fn view<'a>(&'a self, params: &'a ParamCache<P>) -> Element<'a, Message<MyMessage>> {
        // Build your iced widget tree here
    }

    fn update(
        &mut self,
        message: Message<MyMessage>,
        params: &ParamCache<P>,
        ctx: &PluginContext<P>,
    ) -> Task<Message<MyMessage>> {
        // Handle messages
        Task::none()
    }
}
```

Part of [truce](https://github.com/truce-audio/truce).
