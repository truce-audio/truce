# truce-params

Parameter system for the truce audio plugin framework.

Provides the types and utilities for declaring, smoothing, and formatting
plugin parameters:

- **Parameter types** — `FloatParam`, `IntParam`, `BoolParam`, `EnumParam`
- **`ParamRange`** — defines value ranges and mapping curves
- **`Smoother` / `SmoothingStyle`** — per-sample parameter smoothing
- **`ParamInfo`** — metadata (name, unit, flags) for host integration
- **`format_param_value`** — human-readable display formatting
