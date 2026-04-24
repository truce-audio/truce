# 2. Your first plugin

Scaffold, build, install, load, iterate. End state: a stereo gain
plugin with a GUI loaded in your DAW.

Prerequisites: [chapter 1 → install.md](install.md).

## Scaffold

```sh
cargo truce new my-gain
cd my-gain
```

`cargo truce new` writes a minimal, self-contained project:

```
my-gain/
├── Cargo.toml       crate with cdylib crate-type + default clap/vst3 features
├── truce.toml       vendor identity + this plugin's metadata (name, IDs, AU codes)
└── src/
    └── lib.rs       the whole plugin — params, DSP, GUI, export macro
```

No `build.rs`. No standalone binary crate. No separate GUI crate.
One file per concern.

**Shipping a suite?** Use `new-workspace` instead to create one
Cargo workspace with a shared `truce.toml` and one sub-crate per
plugin:

```sh
cargo truce new-workspace studio gain reverb delay
cargo truce new-workspace studio gain synth arp \
    --vendor "Studio Audio" --vendor-id com.studio \
    --type:synth=instrument --type:arp=midi
```

You get `studio/plugins/{gain,synth,arp}/`, each with its own
`lib.rs`, plus one `truce.toml` with three `[[plugin]]` entries.
Every `cargo truce` command below works workspace-wide; add
`-p <name>` to target one plugin.

## Tour the generated code

`src/lib.rs` has three parts. Open it alongside this section.

### 1. Parameters — what the user controls

```rust
#[derive(Params)]
pub struct MyGainParams {
    #[param(name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,
}
```

One line of attributes per parameter. `#[derive(Params)]` generates
the `new()` constructor, a `MyGainParamsParamId` enum with typed
variants, and the `Params` trait impl. Parameter IDs auto-assign by
field order (`Gain = 0`, then 1, 2, ...). See
[chapter 4 → parameters.md](parameters.md) for the full attribute
reference.

### 2. Plugin logic — what happens to the audio

```rust
use MyGainParamsParamId as P;

pub struct MyGain { params: Arc<MyGainParams> }

impl MyGain {
    pub fn new(params: Arc<MyGainParams>) -> Self { Self { params } }
}

impl PluginLogic for MyGain {
    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
    }

    fn process(&mut self, buffer: &mut AudioBuffer, _events: &EventList,
               _ctx: &mut ProcessContext) -> ProcessStatus {
        for i in 0..buffer.num_samples() {
            let gain = db_to_linear(self.params.gain.smoothed_next() as f64) as f32;
            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * gain;
            }
        }
        ProcessStatus::Normal
    }

    fn layout(&self) -> truce_gui::layout::GridLayout {
        use truce_gui::layout::{GridLayout, knob, widgets};
        GridLayout::build("MY GAIN", "V0.1", 1, 80.0, vec![widgets(vec![
            knob(P::Gain, "Gain"),
        ])])
    }
}
```

`PluginLogic` is the one trait you implement. `reset()` is called
when the host knows the sample rate and block size; `process()` is
called on the audio thread for every block; `layout()` returns a
declarative description of the GUI. Only `reset` and `process` are
required. See [chapter 3 → plugin-anatomy.md](plugin-anatomy.md).

### 3. The export macro — makes it a plugin

```rust
truce::plugin! {
    logic: MyGain,
    params: MyGainParams,
}
```

Generates all format entry points (CLAP, VST3, VST2, LV2, AU v2/v3,
AAX via Cargo features), state serialization, parameter hosting,
and the hot-reload shell. One macro. Default bus layout is stereo;
add `bus_layouts: [...]` for instruments, sidechains, or
mono/mono.

## Tour the generated config

### `truce.toml`

```toml
[vendor]
name = "My Company"
id = "com.mycompany"
au_manufacturer = "MyCo"

[[plugin]]
name = "My Gain"
suffix = "my-gain"
crate = "my-gain"
category = "effect"
fourcc = "MyG1"
```

`truce.toml` is the single source of truth for plugin identity
across every format. `truce-build` reads it at compile time so
`truce::plugin!` doesn't need any of this in code. Full schema in
[chapter 8 → shipping.md#truce-toml-reference](shipping.md#truce-toml-reference).

Per-developer secrets (signing identity, AAX SDK path, notary
credentials) go in `.cargo/config.toml` (gitignored), **not** here.

### `Cargo.toml` features

```toml
[features]
default = ["clap", "vst3"]
clap = ["truce/clap"]
vst3 = ["truce/vst3"]
vst2 = ["truce/vst2"]
lv2  = ["truce/lv2"]
au   = ["truce/au"]
aax  = ["truce/aax"]
dev  = ["truce/dev"]
```

Scaffolded plugins enable CLAP + VST3 by default. Add more formats
to `default`, or opt in per-command (`cargo truce install --vst2`).
Per-format detail (SDKs, env vars, install paths, signing) is in
[docs/formats/](../formats/).

## Build and install

```sh
cargo truce install
```

This builds the crate, bundles each enabled format, codesigns on
macOS, and drops bundles into the user- or system-scope plugin
directories for your OS. You should see something like:

```
CLAP: ~/Library/Audio/Plug-Ins/CLAP/My Gain.clap
VST3: /Library/Audio/Plug-Ins/VST3/My Gain.vst3
```

Explicit format selection works too:

```sh
cargo truce install --clap
cargo truce install --vst3 --lv2
```

Install destinations per platform live in
[docs/formats/README.md](../formats/README.md).

## Load in a DAW

1. Open your DAW (Reaper is a good first test — free trial, loads
   CLAP / VST3 / VST2 / LV2).
2. Rescan plugins (Reaper: `Options → Preferences → Plug-ins →
   VST/CLAP → Re-scan`).
3. Insert **My Gain** on a track.
4. Play audio; drag the knob. Volume should change.

Expected:

```
┌──────────────────────┐
│  MY GAIN        V0.1 │
├──────────────────────┤
│        ◎             │
│       Gain           │
│      0.0 dB          │
└──────────────────────┘
```

## Edit and rebuild

Add a pan parameter. In `src/lib.rs`:

```rust
#[derive(Params)]
pub struct MyGainParams {
    #[param(name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,

    #[param(name = "Pan", range = "linear(-1, 1)",
            unit = "pan", smooth = "exp(5)")]
    pub pan: FloatParam,
}
```

Use it in `process()`:

```rust
for i in 0..buffer.num_samples() {
    let gain = db_to_linear(self.params.gain.smoothed_next() as f64) as f32;
    let pan  = self.params.pan.smoothed_next();
    let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
    let (l, r) = (gain * angle.cos(), gain * angle.sin());
    buffer.output(0)[i] *= l;
    if buffer.num_output_channels() >= 2 {
        buffer.output(1)[i] *= r;
    }
}
```

Show it in the GUI:

```rust
fn layout(&self) -> GridLayout {
    GridLayout::build("MY GAIN", "V0.1", 2, 50.0, vec![widgets(vec![
        knob(P::Gain, "Gain"),
        knob(P::Pan,  "Pan"),
    ])])
}
```

Rebuild:

```sh
cargo truce install
```

Close and reopen the plugin in your DAW. You now have two knobs.

## Skip the rescan — hot reload

Closing and reopening the plugin for every edit gets old fast. Turn
on hot reload:

```sh
cargo truce install --dev      # one-time: installs the hot-reload shell
cargo watch -x "build -p my-gain"
```

Every save, the plugin reloads in ~2 seconds with no DAW restart,
no window close. When you're done iterating, ship the static
release build:

```sh
cargo truce install            # no --dev = static, zero overhead
```

Full story in [chapter 7 → hot-reload.md](hot-reload.md).

## What's next

- **Parameters you need today** — boolean, int, enum, groups,
  meters, custom formatting → [chapter 4 → parameters.md](parameters.md).
- **Non-trivial processing** — MIDI, transport, sample-accurate
  events, instruments → [chapter 5 → processing.md](processing.md).
- **A richer UI** — more widgets, `section()`, switching to
  egui/iced/Slint → [chapter 6 → gui.md](gui.md).
- **Shipping to users** — signed `.pkg` / `.exe` installers →
  [chapter 8 → shipping.md](shipping.md).
- **Real examples** — `examples/gain`, `examples/eq`,
  `examples/synth`, `examples/transpose`, `examples/arpeggio`,
  `examples/tremolo` in the repo.
