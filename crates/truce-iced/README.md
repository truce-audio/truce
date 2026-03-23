# truce-iced

[Iced](https://github.com/iced-rs/iced) GUI backend for truce plugins.

An alternative GUI backend that replaces the built-in tiny-skia/wgpu renderer
with iced, giving plugin authors access to a full retained-mode widget toolkit.

## Key types

- **`IcedEditor`** — the `Editor` implementation
- **`IcedPlugin`** — trait for defining your plugin's iced UI
- **`AutoPlugin`** — auto-generated UI from parameter definitions
