# truce-gui-types

Lightweight GUI types for the truce audio plugin framework.

## Overview

`truce-gui-types` carries the trait + data surface that GUI
backends build on - `GridLayout`, `RenderBackend` (the trait -
not its impl), `WidgetType`, `WidgetRegion`, `InteractionState`,
`Theme` / `Color`, `ParamSnapshot`, the `grid!` and `layout!`
macros. Crates that only need to *describe* layouts and react
to platform-translated input events depend on this crate; the
heavy machinery (tiny-skia rasterisation, baseview windowing,
truce-font / fontdue) stays in `truce-gui`.

## Crate split

```
truce-core          ← AudioBuffer, Editor trait, EventList, ...
   ↓
truce-gui-types     ← this crate - light data + traits
   ↓
truce-plugin        ← PluginLogic / PluginLogic64 / PluginLogicCore
   ↓                ↘
truce-gui           truce-egui / truce-iced / truce-slint  ← alt GUI backends
(BuiltinEditor,     each depends on truce-gui-types but not truce-gui
 CpuBackend, font,
 baseview, ...)
```

A slint-only plugin's dep tree contains `truce-plugin →
truce-gui-types → truce-core` - `tiny-skia`, `baseview`,
`fontdue`, `truce-font` don't appear unless the plugin also
reaches into `truce-gui::BuiltinEditor`.

## Key types

- **`GridLayout` / `PluginLayout`** - declarative widget layouts
- **`RenderBackend`** - trait alternative GUI backends impl (the
  built-in `CpuBackend` in `truce-gui` is one impl;
  `truce-gpu::GpuBackend` is another)
- **`WidgetType` / `WidgetKind` / `WidgetRegion`** - widget
  enumeration + hit-test regions
- **`InteractionState` / `InputEvent`** - input dispatch primitives
  (the `BaseviewTranslator` that maps baseview events into these
  lives in `truce-gui`)
- **`Theme` / `Color`** - visual theme (tiny-skia conversions live
  on `ColorExt` in `truce-gui`)
- **`ParamSnapshot`** - per-frame view of params for widget code

## Macros

- **`layout!`** - declarative DSL for `PluginLayout`
- **`grid!`** - declarative DSL for `GridLayout`

Part of [truce](https://github.com/truce-audio/truce).
