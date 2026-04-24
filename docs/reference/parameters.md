# 4. Parameters

Declare your plugin's knobs, switches, and meters in one struct.
`#[derive(Params)]` plus `#[param(...)]` attributes generate the
plumbing — storage, host-visible IDs, a typed enum, display
formatting, and smoothing.

## The basic shape

```rust
use truce::prelude::*;

#[derive(Params)]
pub struct MyParams {
    #[param(name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,

    #[param(name = "Pan", range = "linear(-1, 1)",
            unit = "pan", smooth = "exp(5)")]
    pub pan: FloatParam,

    #[param(name = "Bypass", flags = "automatable | bypass")]
    pub bypass: BoolParam,
}
```

The derive generates:

- `MyParams::new()` and a `Default` impl.
- A full `Params` trait impl (count, IDs, formatting, smoothing,
  state collection).
- A `MyParamsParamId` enum (`#[repr(u32)]`) with one variant per
  parameter: `Gain = 0`, `Pan = 1`, `Bypass = 2` — typed IDs you
  can pass to the GUI layout and to `context.set_meter`.

IDs auto-assign from field order. If you need to rename a field
after release, **keep the same `id`** — change the name, not the
ID, or host automation and saved presets break.

## Typed IDs in practice

Alias the generated enum once and reuse it:

```rust
use MyParamsParamId as P;

// GUI layout:
knob(P::Gain, "Gain");
slider(P::Pan, "Pan");
toggle(P::Bypass, "Bypass");

// Meters:
context.set_meter(P::MeterL, buffer.output_peak(0));
```

Typos are compile errors. Rename-refactor is safe.

## Parameter types

| Field type | Widget default | Notes |
|------------|----------------|-------|
| `FloatParam` | knob | Continuous. Supports smoothing and custom formatting. |
| `BoolParam` | toggle | On / off. Auto-detected as a toggle widget. |
| `IntParam` | knob | Integer steps within a range. |
| `EnumParam<T>` | selector | Click-to-cycle; `T` is a `#[derive(ParamEnum)]` enum. |
| `MeterSlot` | meter | Read-only, written from `process()`, drawn by the GUI. |

### Enum parameters

```rust
#[derive(ParamEnum)]
pub enum Waveform { Sine, Saw, Square, Triangle }

#[param(name = "Waveform")]
pub waveform: EnumParam<Waveform>,
```

The range is inferred from the variant count — don't pass
`range`. Use `#[name = "..."]` on a variant to override its display
text (`#[name = "Up/Down"] UpDown` → displays as `"Up/Down"`, the
Rust name stays `UpDown`).

### Meters

Meters are not parameters — they flow audio-thread → UI-thread
instead of host → plugin. Declare them as `MeterSlot` fields with
`#[meter]`:

```rust
#[derive(Params)]
pub struct MyParams {
    #[param(name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,

    #[meter] pub meter_l: MeterSlot,
    #[meter] pub meter_r: MeterSlot,
}
```

Meter IDs start at 256 (parameters are 0..255). Both kinds share
the generated `ParamId` enum — `P::Gain`, `P::MeterL`, `P::MeterR`
all work.

Write from `process()`, draw in `layout()`:

```rust
// process():
context.set_meter(P::MeterL, buffer.output_peak(0));
context.set_meter(P::MeterR, buffer.output_peak(1));

// layout():
meter(&[P::MeterL, P::MeterR], "Level").rows(3)
```

The write is realtime-safe (atomic); the GUI reads the latest
value every frame.

## Attribute reference

Every key that `#[param(...)]` accepts:

| Key | Example | Notes |
|-----|---------|-------|
| `id` | `id = 0` | Optional. Auto-assigned by field order if omitted. Stable across releases — never change. |
| `name` | `name = "Gain"` | **Required.** Display name in host and GUI. |
| `short_name` | `short_name = "Gn"` | Abbreviated name for narrow strips. Defaults to `name`. |
| `range` | `range = "linear(-60, 6)"` | Value mapping. Inferred for `BoolParam` and `EnumParam<T>`. |
| `default` | `default = 0.0` | Default value in plain units. Defaults to range min. |
| `unit` | `unit = "dB"` | Display unit: `dB`, `Hz`, `ms`, `s`, `%`, `pan`, `st`. Shapes the default formatter. |
| `smooth` | `smooth = "exp(5)"` | Smoothing style + time in ms. See below. |
| `group` | `group = "Filter"` | Parameter group shown by the host (CLAP module path, VST3 unit, AU group). |
| `flags` | `flags = "automatable \| bypass"` | Combination of: `automatable`, `hidden`, `readonly`, `bypass`. |
| `format` | `format = "format_cutoff"` | Name of a method on the params struct that converts a `f64` value to `String`. |
| `parse` | `parse = "parse_cutoff"` | Method that converts a host text-input `&str` back to `f64`. |

### Range types

```
range = "linear(-60, 6)"        // linear between min and max
range = "log(20, 20000)"        // logarithmic (frequency, time constants)
range = "exp(20, 20000)"        // exponential
range = "discrete(1, 16)"       // integer steps
```

(`BoolParam` ranges are implicit `0..1`. `EnumParam<T>` ranges are
inferred from `T`'s variant count.)

### Smoothing

Host automation usually arrives block-rate. Smoothing
interpolates between successive target values so there's no zipper
noise on continuous parameters.

```
smooth = "none"            // instant jump. Right for toggles, enums, voice counts.
smooth = "linear(20)"      // linear ramp over 20 ms. Right for pan and mix.
smooth = "exp(5)"          // exponential one-pole, 5 ms. Right for gain and filter cutoff.
```

Call `params.set_sample_rate(sr)` + `params.snap_smoothers()` in
`reset()`. Pull a smoothed value per sample with `smoothed_next()`:

```rust
fn reset(&mut self, sample_rate: f64, _: usize) {
    self.params.set_sample_rate(sample_rate);
    self.params.snap_smoothers();
}

fn process(&mut self, buffer: &mut AudioBuffer, _: &EventList,
           _: &mut ProcessContext) -> ProcessStatus {
    for i in 0..buffer.num_samples() {
        let g = self.params.gain.smoothed_next();
        // ...
    }
    ProcessStatus::Normal
}
```

Smoother methods take `&self` (atomics inside), so they work
through `Arc<Params>` without `&mut`.

## Shared ownership (`Arc<Params>`)

The shell owns the `Arc<MyParams>` and passes a clone to
`YourPlugin::new()`. GUI closures can also clone the `Arc`. Host
automation writes atomically; every reader sees the latest value
without locking.

```rust
pub struct MyPlugin {
    params: Arc<MyParams>,
}

impl MyPlugin {
    pub fn new(params: Arc<MyParams>) -> Self { Self { params } }
}
```

One copy. No `RwLock`, no `Mutex`, no listener callbacks.

---

## Appendix: groups, nesting, custom formatting

Everything below is opt-in. Skim or skip on first read.

### Groups

Group strings show up as a host-side folder — CLAP module paths,
VST3 units, AU parameter groups.

```rust
#[param(name = "Cutoff", group = "Filter", range = "log(20, 20000)",
        default = 8000, unit = "Hz", smooth = "exp(5)")]
pub cutoff: FloatParam,

#[param(name = "Resonance", group = "Filter", range = "linear(0, 1)",
        smooth = "exp(5)")]
pub resonance: FloatParam,
```

### Nested structs

Split a wide parameter set into sub-structs with `#[nested]`:

```rust
#[derive(Params)]
pub struct PluginParams {
    #[nested] pub filter:   FilterParams,
    #[nested] pub envelope: EnvelopeParams,
}

#[derive(Params)]
pub struct FilterParams {
    #[param(id = 10, name = "Cutoff", group = "Filter",
            range = "log(20, 20000)", unit = "Hz")]
    pub cutoff: FloatParam,
}
```

**Assign explicit `id` values when nesting.** Auto-IDs are
per-struct, so without `id =` the nested structs will collide at
0, 1, 2, … Pick a non-overlapping ID block per struct (10–19 for
filter, 20–29 for envelope, etc.).

Nested params are flattened for the host — it sees one list.

### Custom formatting

Most plugins get by with the default formatter (chosen from
`unit`). When you need conditional display — Hz vs. kHz, semitones,
dotted-note durations — point `format` at a method on your params
struct:

```rust
#[derive(Params)]
pub struct SynthParams {
    #[param(name = "Cutoff", range = "log(20, 20000)", unit = "Hz",
            format = "format_cutoff")]
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

`parse` is the inverse, for host text input (`"440 Hz"` → `440.0`).

## What's next

- **[Chapter 5 → processing.md](processing.md)** — put these
  parameters to work in `process()`.
- **[Chapter 6 → gui.md](gui.md)** — wire parameters into widgets
  via typed `ParamId`s.
