# truce-egui

[egui](https://github.com/emilk/egui)-based GUI backend for truce audio plugins.

Provides `EguiEditor`, an implementation of `truce_core::Editor` that renders
using egui's immediate-mode UI via egui-wgpu. Gives plugin developers access to
egui's full widget library, layout system, and ecosystem while retaining truce's
parameter binding and host integration.

## Key types

- **`EguiEditor`** — the `Editor` implementation
- **`EditorUi`** — trait for defining your plugin's UI
- **`ParamState`** — parameter state bridge for egui widgets
