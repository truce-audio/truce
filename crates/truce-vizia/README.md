# truce-vizia

[Vizia](https://github.com/vizia/vizia) GUI backend for truce audio plugins.

Provides `ViziaEditor`, which implements `truce_core::Editor` using vizia +
baseview for embedded plugin GUIs. Vizia handles windowing, event dispatch,
and rendering (Skia/GL) internally — no custom platform shim needed.

## Key types

- **`ViziaEditor`** — the `Editor` implementation
- **`ParamModel` / `ParamEvent`** — reactive parameter bindings
- **`ParamNormLens` / `ParamBoolLens` / `ParamFormatLens` / `MeterLens`** — vizia lenses for parameter values
