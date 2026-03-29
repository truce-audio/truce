# truce-egui

egui GUI backend for truce audio plugins.

## Overview

Provides `EguiEditor`, an implementation of `truce_core::Editor` that renders
using [egui](https://github.com/emilk/egui)'s immediate-mode UI via
egui-wgpu. This gives plugin developers full access to egui's widget library,
layout system, and ecosystem while retaining truce's parameter binding and
host integration. Supports custom fonts and themes.

Use this backend when you want fine-grained control over your plugin's UI
using egui's immediate-mode paradigm.

## Key types

- **`EguiEditor`** -- the `Editor` implementation
- **`EditorUi`** -- trait you implement to define your plugin's UI
- **`ParamState`** -- parameter state bridge for reading/writing truce params from egui widgets

## Usage

```rust
struct MyUi;

impl EditorUi for MyUi {
    fn ui(&mut self, ctx: &egui::Context, params: &ParamState) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add(egui::Slider::new(&mut params.get("gain"), -60.0..=0.0));
        });
    }
}
```

Part of [truce](https://github.com/truce-audio/truce).
