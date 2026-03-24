## Your first plugin in 5 minutes

Scaffold a new plugin:

```bash
cargo truce new my-gain
```

Or create manually:

```
my-gain/
├── Cargo.toml
└── src/
    └── lib.rs
```

### Cargo.toml

```toml
[package]
name = "my-gain"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]

[features]
default = ["clap", "vst3"]
clap = ["dep:truce-clap", "dep:clap-sys"]
vst3 = ["dep:truce-vst3"]
dev = ["truce/dev"]

[dependencies]
truce = { workspace = true }
truce-clap = { workspace = true, optional = true }
truce-vst3 = { workspace = true, optional = true }
clap-sys = { version = "0.5", optional = true }
truce-gui = { workspace = true }
```

No build.rs needed — `plugin_info!()` reads `truce.toml` directly.
The `cdylib` crate type produces the shared library that DAW hosts
load. The default build produces CLAP + VST3. AU requires a separate
build with `--features au` (see below). The `rlib` allows the
`[[bin]]` target and tests to link against the same crate. Everything
lives in one place.

### src/lib.rs

Everything -- parameters, plugin logic, the `plugin!` macro, and
the GUI layout -- lives in a single `lib.rs`.

```rust
use truce::params::{BoolParam, FloatParam};
use truce::prelude::*;
use truce_params_derive::Params;

// --- Parameters ---
// #[param(...)] attributes define all metadata declaratively.
// The derive macro generates new(), Default, the Params trait impl,
// and a GainParamsParamId enum with a variant per parameter.

#[derive(Params)]
pub struct GainParams {
    #[param(name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,

    #[param(name = "Pan", range = "linear(-1, 1)",
            unit = "pan", smooth = "exp(5)")]
    pub pan: FloatParam,

    #[param(name = "Bypass", short_name = "Byp",
            flags = "automatable | bypass")]
    pub bypass: BoolParam,
}

// Use the generated param ID enum for type-safe references
use GainParamsParamId as P;

// --- Plugin ---

pub struct Gain {
    pub params: Arc<GainParams>,
}

impl Gain {
    pub fn new(params: Arc<GainParams>) -> Self {
        Self { params }
    }
}

impl PluginLogic for Gain {
    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        if self.params.bypass.value() {
            return ProcessStatus::Normal;
        }

        for i in 0..buffer.num_samples() {
            let gain_db = self.params.gain.smoothed_next();
            let pan = self.params.pan.smoothed_next();
            let gain_linear = db_to_linear(gain_db as f64) as f32;

            let pan_angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
            let gain_l = gain_linear * pan_angle.cos();
            let gain_r = gain_linear * pan_angle.sin();

            buffer.output(0)[i] *= gain_l;
            if buffer.num_output_channels() >= 2 {
                buffer.output(1)[i] *= gain_r;
            }
        }

        ProcessStatus::Normal
    }

    fn layout(&self) -> truce_gui::layout::PluginLayout {
        truce_gui::layout!("MY GAIN", "V0.1", 80.0, {
            row {
                knob(P::Gain, "Gain")
                knob(P::Pan, "Pan")
            }
        })
    }
}

// --- Export (one macro, all formats) ---

truce::plugin! { logic: Gain, params: GainParams }
```

Key structural points:

- **`#[derive(Params)]` + `#[param(...)]`** generates the constructor,
  `Default`, the entire `Params` trait impl, and a `GainParamsParamId`
  enum (`#[repr(u32)]`) with typed variants for each parameter
  (and meter, if `#[meter]` fields are present). IDs are auto-assigned
  by field order. No manual boilerplate — no `pub const ID_*` constants needed.
- **`PluginLogic`** is the trait you implement. It covers `reset()`,
  `process()`, `layout()`, and optional lifecycle methods. `new()` is
  an inherent method on your struct (not part of the trait) — it takes
  `Arc<Params>` shared with the shell.
- **`truce::plugin!`** replaces the old `PluginExport` impl and
  per-format export macros (`export_clap!`, `export_vst3!`, etc.).
  One macro handles all formats.
- **GUI layout** is defined inside `layout()` on `PluginLogic`, so
  both the plugin and standalone binary can use it.

### src/main.rs (standalone entry point)

The standalone binary imports from the same crate using its library
name. No separate standalone crate needed.

```rust
use my_gain::{Gain, GainParams, gui_layout};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--render-gui") {
        let params = std::sync::Arc::new(GainParams::new());
        truce_standalone::render_gui_png(params, gui_layout(), "gain-gui.png");
        return;
    }

    if args.iter().any(|a| a == "--no-gui") {
        truce_standalone::run::<Gain>();
        return;
    }

    truce_standalone::run_with_gui::<Gain>(gui_layout());
}
```

### Build and install

The easiest way to build and install:

```sh
# Build and install all formats
cargo truce install

# Or install specific formats
cargo truce install --clap
cargo truce install --au2
cargo truce install --aax          # AAX (requires AAX SDK + Pro Tools Developer)
```

Or manually:

```sh
# Build CLAP + VST3 (default features)
cargo build --release -p my-gain
cp target/release/libmy_gain.dylib ~/Library/Audio/Plug-Ins/CLAP/MyGain.clap

# Build VST2 (separate binary, no CLAP/VST3 symbols)
cargo build --release -p my-gain --no-default-features --features vst2

# Build AU v2 (separate binary)
cargo build --release -p my-gain --no-default-features --features au

# Then open your DAW and scan for "My Gain".
```

Each format is a Cargo feature: `default = ["clap", "vst3"]`,
with `vst2` and `au` as separate builds. This avoids symbol
conflicts between format entry points.

---

---

[← Previous](01-setup.md) | [Next →](03-plugin-trait.md) | [Index](README.md)
