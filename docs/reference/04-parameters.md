## Parameters deep dive

### Declaring parameters

Use `#[derive(Params)]` on a struct with `#[param(...)]` attributes on
each field. The derive macro generates:

- `new()` constructor and `Default` impl
- The full `Params` trait implementation
- A `{StructName}ParamId` enum with a variant per parameter
  (`#[repr(u32)]` with `From<T> for u32`)

```rust
use truce::prelude::*;

#[derive(Params)]
pub struct GainParams {
    #[param(id = 0, name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,

    #[param(id = 1, name = "Pan", range = "linear(-1, 1)",
            unit = "pan", smooth = "exp(5)")]
    pub pan: FloatParam,

    #[param(id = 2, name = "Bypass", flags = "automatable | bypass")]
    pub bypass: BoolParam,
}
```

This generates `GainParamsParamId` with variants `Gain`, `Pan`,
`Bypass`. Use a type alias for convenience:

```rust
use GainParamsParamId as P;

// Type-safe param references in layout and meters:
GridWidget::knob(P::Gain, "Gain");
GridWidget::slider(P::Pan, "Pan");
GridWidget::toggle(P::Bypass, "Bypass");
```

### Parameter types

| Type | Field type | Widget | Notes |
|------|-----------|--------|-------|
| Float | `FloatParam` | Knob or slider | Continuous, supports smoothing |
| Bool | `BoolParam` | Toggle | On/off, auto-detected as toggle widget |
| Int | `IntParam` | Knob | Integer steps within a range |
| Enum | `EnumParam<T>` | Selector | Click-to-cycle, requires `T: ParamEnum` |

### Enum parameters

Enums need `#[derive(ParamEnum)]` — the derive generates all required
trait methods automatically:

```rust
#[derive(Clone, Copy, PartialEq, Eq, ParamEnum)]
pub enum Waveform {
    Sine,
    Saw,
    Square,
    Triangle,
}
```

Use `#[name = "..."]` on a variant to override its display name:

```rust
#[derive(Clone, Copy, PartialEq, Eq, ParamEnum)]
pub enum ArpPattern {
    Up,
    Down,
    #[name = "Up/Down"]
    UpDown,
    Random,
}
```

The range is auto-inferred from the variant count — no `range` key
needed in `#[param(...)]`:

```rust
#[param(id = 3, name = "Pattern")]
pub pattern: EnumParam<ArpPattern>,
```

### Attribute reference

| Key | Example | Notes |
|-----|---------|-------|
| `id` | `id = 0` | **Required.** Stable integer ID — never change after release. |
| `name` | `name = "Gain"` | **Required.** Display name in host UI. |
| `short_name` | `short_name = "Gn"` | Abbreviated name for narrow displays. Defaults to `name`. |
| `range` | `range = "linear(-60, 6)"` | Value mapping. Auto-inferred for `BoolParam` and `EnumParam`. |
| `default` | `default = 0.0` | Default value in plain units. Defaults to range min. |
| `unit` | `unit = "dB"` | Display unit: `dB`, `Hz`, `ms`, `s`, `%`, `pan`, `st`. |
| `smooth` | `smooth = "exp(5)"` | Smoothing style and time in ms. |
| `group` | `group = "Filter"` | Parameter group in host UI. |
| `flags` | `flags = "automatable \| bypass"` | Flags: `automatable`, `hidden`, `readonly`, `bypass`. |
| `format` | `format = "format_cutoff"` | Custom format method name (see below). |
| `parse` | `parse = "parse_cutoff"` | Custom parse method for text input → value. |

### Range types

```
range = "linear(-60, 6)"       // linear mapping between min and max
range = "log(20, 20000)"        // logarithmic (frequency, time constants)
range = "discrete(1, 16)"       // integer steps
range = "enum(4)"                // enum with 4 variants (usually auto-inferred)
```

These map to `ParamRange::Linear`, `Logarithmic`, `Discrete`, `Enum`.

### Smoothing

Smoothing prevents audible zipper noise when parameters change.
Specified via the `smooth` key:

```
smooth = "none"           // instant jump (enums, bools, voice count)
smooth = "linear(20)"     // linear ramp over 20ms (pan, mix/blend)
smooth = "exp(5)"         // exponential one-pole, 5ms (gain, filter cutoff)
```

All smoother methods take `&self` — atomics internally, works through
`Arc<Params>` without `&mut`:

```rust
fn process(&mut self, buffer: &mut AudioBuffer, _events: &EventList,
           _ctx: &mut ProcessContext) -> ProcessStatus {
    for i in 0..buffer.num_samples() {
        let gain = self.params.gain.smoothed_next();
        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            out[i] = inp[i] * db_to_linear(gain as f64) as f32;
        }
    }
    ProcessStatus::Normal
}
```

Call `set_sample_rate()` and `snap_smoothers()` in `reset()`:

```rust
fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
    self.params.set_sample_rate(sample_rate);
    self.params.snap_smoothers();
}
```

### Shared ownership

Parameters are shared between the shell and the plugin via
`Arc<Params>`. The plugin receives `Arc<MyParams>` in `new()`:

```rust
pub struct Gain {
    params: Arc<GainParams>,
}

impl Gain {
    pub fn new(params: Arc<GainParams>) -> Self {
        Self { params }
    }
}
```

The host writes automation values to the atomic params. The plugin
reads them via `smoothed_next()` or `value()`. One copy, no sync
needed.

### Meter IDs

Meter values (levels, gain reduction) use separate IDs from
parameters. Define them as a `#[repr(u32)]` enum:

```rust
#[repr(u32)]
#[derive(Clone, Copy)]
pub enum Meter { Left = 100, Right = 101 }
impl From<Meter> for u32 { fn from(m: Meter) -> u32 { m as u32 } }
```

Write from `process()`, display in the GUI:

```rust
// In process():
context.set_meter(Meter::Left, buffer.output_peak(0));
context.set_meter(Meter::Right, buffer.output_peak(1));

// In layout():
GridWidget::meter(&[Meter::Left.into(), Meter::Right.into()], "Level")
```

Using a separate enum prevents mixing parameter and meter IDs at
compile time.

### Custom formatting

Use the `format` attribute to specify a method for display:

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

Without `format`, the default formatter uses the `unit` to pick a
sensible format (e.g., `"-12.5 dB"`, `"440 Hz"`, `"100%"`).

### Parameter groups

Groups organize parameters in the host's UI:

```rust
#[param(id = 1, name = "Cutoff", group = "Filter",
        range = "log(20, 20000)", default = 8000, unit = "Hz",
        smooth = "exp(5)")]
pub cutoff: FloatParam,

#[param(id = 2, name = "Resonance", group = "Filter",
        range = "linear(0, 1)", smooth = "exp(5)")]
pub resonance: FloatParam,
```

The group string maps to CLAP module paths, VST3 units, and AU
parameter grouping.

### Nested params

For plugins with many parameters, split into sub-structs using
`#[nested]`:

```rust
#[derive(Params)]
pub struct PluginParams {
    #[nested]
    pub filter: FilterParams,

    #[nested]
    pub envelope: EnvelopeParams,
}

#[derive(Params)]
pub struct FilterParams {
    #[param(id = 10, name = "Cutoff", group = "Filter",
            range = "log(20, 20000)", unit = "Hz")]
    pub cutoff: FloatParam,
}
```

Nested params are flattened — the host sees all parameters in one
list. IDs must be globally unique across all nested structs.

---

[← Previous](03-plugin-trait.md) | [Next →](05-processing.md) | [Index](README.md)
