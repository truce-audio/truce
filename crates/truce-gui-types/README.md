# truce-gui-types

Lightweight GUI types for the truce audio plugin framework.

## Overview

`truce-gui-types` carries the trait + data surface that GUI
backends build on - `GridLayout`, `RenderBackend` (the trait -
not its impl), `WidgetType`, `WidgetRegion`, `InteractionState`,
`Theme` / `Color`, `ParamSnapshot`, the `grid!` and `layout!`
macros. Crates that only need to *describe* layouts and react
to platform-translated input events depend on this crate; the
heavy machinery (tiny-skia rasterization, baseview windowing,
truce-font / skrifa) stays in the renderer crates (`truce-cpu`,
`truce-gpu`) and `truce-gui`.

## Crate split

```
truce-core          <- AudioBuffer, Editor trait, EventList, ...
   |
truce-gui-types     <- this crate - light data + traits
   |
truce-plugin        <- PluginLogic / PluginLogic64 / PluginLogicCore
   |       \
truce-gui   truce-egui / truce-iced / truce-slint  <- alt GUI backends
(BuiltinEditor,   each depends on truce-gui-types but not truce-gui
 baseview)
   |       \
truce-cpu  truce-gpu  <- RenderBackend impls, pulled by truce-gui's
(tiny-skia, (wgpu)       cpu (default) / gpu features
 skrifa)
```

A slint-only plugin's dep tree contains `truce-plugin ->
truce-gui-types -> truce-core` - `tiny-skia`, `baseview`,
`skrifa`, `truce-font` don't appear unless the plugin also
depends on `truce-gui`.

## Key types

- **`GridLayout` / `PluginLayout`** - declarative widget layouts
- **`RenderBackend`** - trait the renderer backends implement (the
  `CpuBackend` in `truce-cpu` is one impl; `truce_gpu::WgpuBackend`
  is another)
- **`WidgetType` / `WidgetKind` / `WidgetRegion`** - widget
  enumeration + hit-test regions
- **`InteractionState` / `InputEvent`** - input dispatch primitives
  (the `BaseviewTranslator` that maps baseview events into these
  lives in `truce-gui`)
- **`Theme` / `Color`** - visual theme (tiny-skia conversions live
  on `ColorExt` in `truce-cpu`)
- **`ParamSnapshot`** - per-frame view of params for widget code

## Macros

- **`layout!`** - declarative DSL for `PluginLayout`
- **`grid!`** - declarative DSL for `GridLayout`

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
