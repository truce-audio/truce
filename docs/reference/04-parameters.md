## Parameters deep dive

### Param types

Use `#[param(...)]` attributes on fields. The `#[derive(Params)]`
macro generates:
- The constructor (`new()`) and `Default` impl
- The `Params` trait impl
- A `{StructName}ParamId` enum (`#[repr(u32)]` with `From<T> for u32`)
  containing a variant for each parameter

For example, `#[derive(Params)] pub struct MyParams { ... }` generates
`MyParamsParamId` with variants like `MyParamsParamId::Gain`. Use a
type alias for convenience: `use MyParamsParamId as P;`

```rust
#[derive(Params)]
pub struct MyParams {
    // Continuous float with smoothing
    #[param(id = 0, name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,

    // Boolean toggle (bypass flag)
    #[param(id = 1, name = "Bypass", flags = "automatable | bypass")]
    pub bypass: BoolParam,

    // Integer range
    #[param(id = 2, name = "Voices", range = "discrete(1, 16)", default = 8)]
    pub voices: IntParam,

    // Enum (dropdown in host UI)
    #[param(id = 3, name = "Waveform", range = "enum(4)", default = 1)]
    pub waveform: EnumParam<Waveform>,
}
```

Enums used as parameters must implement the `ParamEnum` trait:

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Waveform {
    Sine,
    Saw,
    Square,
    Triangle,
}

impl ParamEnum for Waveform {
    fn from_index(index: usize) -> Self {
        match index {
            0 => Self::Sine,
            1 => Self::Saw,
            2 => Self::Square,
            3 => Self::Triangle,
            _ => Self::Saw,
        }
    }
    fn to_index(&self) -> usize {
        *self as usize
    }
    fn name(&self) -> &'static str {
        match self {
            Self::Sine => "Sine",
            Self::Saw => "Saw",
            Self::Square => "Square",
            Self::Triangle => "Triangle",
        }
    }
    fn variant_count() -> usize {
        4
    }
    fn variant_names() -> &'static [&'static str] {
        &["Sine", "Saw", "Square", "Triangle"]
    }
}
```

### Attribute reference

The `#[param(...)]` attribute supports these keys:

| Key | Example | Notes |
|-----|---------|-------|
| `id` | `id = 0` | **Required.** Stable integer ID — never change after release. |
| `name` | `name = "Gain"` | **Required.** Display name in host UI. |
| `short_name` | `short_name = "Gn"` | Abbreviated name. Defaults to `name`. |
| `range` | `range = "linear(-60, 6)"` | Value mapping (see below). |
| `default` | `default = 0.0` | Default value in plain units. Defaults to range min. |
| `unit` | `unit = "dB"` | Display unit: `dB`, `Hz`, `ms`, `s`, `%`, `pan`, `st`. |
| `smooth` | `smooth = "exp(5)"` | Smoothing: `none`, `linear(ms)`, `exp(ms)`. |
| `group` | `group = "Filter"` | Parameter group in host UI. |
| `flags` | `flags = "automatable \| bypass"` | `automatable`, `hidden`, `readonly`, `bypass`. |

### Range types

```
range = "linear(-60, 6)"      // linear mapping
range = "log(20, 20000)"       // logarithmic (frequency, time constants)
range = "discrete(0, 16)"     // integer steps
range = "enum(4)"             // enum with 4 variants
```

These map to the underlying `ParamRange` enum (`Linear`, `Logarithmic`, `Discrete`, `Enum`).

### Smoothing modes

Smoothing prevents audible zipper noise when parameters change.
Specified via the `smooth` key in `#[param(...)]`:

```rust
// No smoothing -- value jumps instantly.
// Good for: enums, booleans, voice count.
SmoothingStyle::None

// Linear ramp over N milliseconds.
// Good for: pan, mix/blend.
SmoothingStyle::Linear(20.0)

// Exponential (one-pole) smoothing over N milliseconds.
// Good for: gain (dB), filter frequency -- anything perceptual.
SmoothingStyle::Exponential(5.0)
```

Smoother methods (`snap_smoothers()`, `set_sample_rate()`,
`smoothed_next()`) all take `&self` — the smoother uses atomic fields
internally, so no `&mut self` is needed.

Reading smoothed values in the process callback:

```rust
fn process(&mut self, buffer: &mut AudioBuffer, ..) -> ProcessStatus {
    // Option A: current smoothed value without advancing
    let gain = self.params.gain.smoothed();

    // Option B: one value per sample (smooth during transitions)
    for i in 0..buffer.num_samples() {
        let gain = self.params.gain.smoothed_next();
        // use gain for this sample
    }
}
```

### Custom formatting

Use the `format` attribute to specify a method for custom display:

```rust
#[derive(Params)]
pub struct SynthParams {
    #[param(id = 0, name = "Cutoff", range = "log(20, 20000)",
            unit = "Hz", format = "format_cutoff")]
    pub cutoff: FloatParam,
}

impl SynthParams {
    fn format_cutoff(&self, value: f64) -> String {
        if value >= 1000.0 {
            format!("{:.1} kHz", value / 1000.0)
        } else {
            format!("{:.0} Hz", value)
        }
    }
}
```

You can also override `format_value` directly on the `Params` trait
for full control:

```rust
use SynthParamsParamId as P;

fn format_value(&self, id: u32, value: f64) -> Option<String> {
    match id {
        id if id == P::Cutoff.into() && value >= 1000.0 => {
            Some(format!("{:.1} kHz", value / 1000.0))
        }
        id if id == P::Cutoff.into() => Some(format!("{:.0} Hz", value)),
        _ => None,  // fall back to default formatting
    }
}
```

### Parameter groups

Groups organize parameters in the host's UI. Use the `group` key:

```rust
#[param(id = 1, name = "Cutoff", group = "Filter",
        range = "log(20, 20000)", default = 8000, unit = "Hz")]
pub cutoff: FloatParam,
```

The group string maps to CLAP module paths, VST3 units, and AU
parameter grouping.

---


---

[← Previous](03-plugin-trait.md) | [Next →](05-processing.md) | [Index](README.md)
