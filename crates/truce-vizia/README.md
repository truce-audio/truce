# truce-vizia

Vizia GUI backend for truce audio plugins.

## Overview

Provides `ViziaEditor`, an implementation of `truce_core::Editor` that renders
with [Vizia](https://github.com/vizia/vizia)'s reactive, CSS-styled UI. You
build your editor from a setup closure that populates a `vizia::Context`, and
bind widgets to plugin parameters through a `ParamLens`. A set of ready-made
parameter widgets (knob, slider, toggle, dropdown, level meter, XY pad) ships
in `truce_vizia::widgets`.

Use this backend when you want Vizia's retained-mode, stylesheet-driven UI
with reactive data binding.

Desktop-only (Windows / Linux / macOS). Vizia's skia + GL stack has no iOS
bindings; iOS plugins use the built-in `truce-gui`, `truce-egui`, or
`truce-slint` backend. Vizia is pinned to a `baseview` fork so vizia-backed
plugins inherit the AAX / Pro Tools teardown fix. Screenshot tests work here
too: `Editor::screenshot` drives Vizia against a CPU-backed Skia surface, so
`cargo truce screenshot` needs no OS window or GL context.

## Key types

- **`ViziaEditor`** -- the `Editor` implementation; `with_stylesheet`,
  `with_font`, `min_size` / `max_size` builders
- **`ParamLens`** -- reactive bridge for reading/writing truce params and
  meters from Vizia widgets
- **`widgets`** -- `param_knob`, `param_slider`, `param_toggle`,
  `param_dropdown`, `level_meter`, `param_xy_pad`
- **`PluginContext`** -- re-exported from `truce-core`

## Usage

```rust
use truce_vizia::widgets::param_knob;
use truce_vizia::{ParamLens, ViziaEditor};

fn editor(&self) -> Option<Box<dyn Editor>> {
    Some(Box::new(ViziaEditor::new(
        self.params.clone(),
        (WIDTH, HEIGHT),
        |cx, lens: ParamLens<MyParams>| {
            param_knob(cx, lens.clone(), MyParams::Gain, "Gain");
        },
    )))
}
```

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
