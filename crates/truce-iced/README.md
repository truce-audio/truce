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

impl IcedPlugin for MyIcedUi {
    type Message = MyMessage;

    fn view(&self, params: &ParamState) -> Element<MyMessage> {
        // Build your iced widget tree here
    }

    fn update(&mut self, message: MyMessage, params: &mut ParamState) {
        // Handle messages
    }
}
```

Part of [truce](https://github.com/truce-audio/truce).
