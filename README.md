<p align="center">
  <a href="https://truce.audio/"><img src="https://truce.audio/branding/logo-banner.svg" alt="truce" width="480" /></a>
  <br/>
  <a href="https://truce.audio/"><b>https://truce.audio</b></a>
</p>

<p align="center">
  Build audio plugins in Rust. CLAP, VST3, LV2, AU v2, AU v3
  (macOS + iOS), AAX, and standalone from a single Rust codebase.
  Dead simple developer experience: in 5 minutes, you can load
  your own plugin in a DAW and test your custom DSP, MIDI, and
  GUI.
</p>

<p align="center">
  <a href="https://truce.audio/"><img src="https://img.shields.io/badge/status-stable-green" alt="Status"></a>
  <a href="https://crates.io/crates/cargo-truce"><img src="https://img.shields.io/crates/v/cargo-truce?logo=rust&logoColor=white" alt="crates.io"></a>
</p>

<p align="center">
  <a href="https://truce.audio/docs/guide/install/"><img src="https://img.shields.io/badge/getting_started-guide-purple?logo=readthedocs&logoColor=white" alt="Getting Started"></a>
  <a href="https://truce-audio.github.io/truce/"><img src="https://img.shields.io/badge/docs-rustdoc-purple?logo=rust&logoColor=white" alt="Docs"></a>
</p>

## Quick Start

```sh
# Install the CLI (one-time)
cargo install cargo-truce

# Scaffold a new plugin
cargo truce new my-plugin
cd my-plugin

# Run the plugin standalone — no DAW needed
cargo truce run

# Build and install
cargo truce install --clap
cargo truce install --vst3

# Open your DAW, scan for plugins, load "MyPlugin"
```

> Every `cargo truce` command builds in **release** mode by default; pass `--debug` for fast-compile iteration.

Other formats:

```sh
cargo truce install              # formats in your plugin's default features
cargo truce install --vst3       # VST3
cargo truce install --vst2       # VST2 (opt-in, legacy — see note below)
cargo truce install --lv2        # LV2
cargo truce install --au3        # AU v3 (macOS, requires Xcode)
cargo truce install --ios        # AU v3 on the booted iOS Simulator
cargo truce install --ios-device # AU v3 on a tethered iPhone / iPad
cargo truce install --aax        # AAX (requires AAX SDK)

cargo truce validate             # auval + pluginval + clap-validator on installed plugins
```

Build without installing:

```sh
cargo truce build                # bundle all formats into target/bundles/ without installing
cargo truce build --clap --vst3  # subset of formats
cargo truce build --shell        # hot-reload shell build

cargo truce run                  # launch the plugin standalone (no DAW)
cargo truce run -p my-plugin     # standalone for a specific crate
cargo truce screenshot --out screenshots/main.png            # render the editor to a file
cargo truce screenshot -p my-plugin --out screenshots/main.png   # multi-plugin: pick one
cargo truce screenshot --state s.pluginstate --out shots/cool.png   # load saved state first
cargo truce screenshot --check --out screenshots/main.png    # CI gate: diff against baseline

cargo truce package              # signed .pkg (macOS) or Inno Setup .exe (Windows)
                                 # → target/dist/<Plugin>-<version>-<platform>.{pkg,exe}
cargo truce package -p my-plugin --formats clap,vst3,aax   # subset
cargo truce package --no-sign                              # dev builds, skip signing
```

Scaffolded plugins default to **CLAP + VST3 + standalone**. VST2, AU, and AAX are
opt-in per plugin via `Cargo.toml` features. On Windows, `cargo truce
install` must be run from an Administrator command prompt (plugin
directories are system-wide).

## Examples

[**truce-analyzer**](https://github.com/truce-audio/truce-analyzer),
a real-time spectrum analyzer with diff overlay for debugging/reverse-engineering plugins:

<img src="examples/screenshots/analyzer_diff.png" width="600">

Smaller example plugins ship in-tree to cover the basics — gain,
EQ, synth, transpose, arpeggio, tremolo, plus three gain variants
showing the egui / iced / Slint backends. See
[truce.audio/docs/examples](https://truce.audio/docs/examples/) for the full
table with screenshots.

## Minimal Example

```rust
use truce::prelude::*;
use truce_gui_types::layout::{knob, widgets, GridLayout};

#[derive(Params)]
pub struct GainParams {
    #[param(name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,
}

use GainParamsParamId as P;

pub struct Gain { params: Arc<GainParams> }

impl Gain {
    pub fn new(params: Arc<GainParams>) -> Self { Self { params } }
}

impl PluginLogic for Gain {
    fn reset(&mut self, sr: f64, _bs: usize) {
        self.params.set_sample_rate(sr);
    }

    fn process(&mut self, buffer: &mut AudioBuffer, _events: &EventList,
               _ctx: &mut ProcessContext) -> ProcessStatus {
        for i in 0..buffer.num_samples() {
            let gain = db_to_linear(self.params.gain.read());
            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * gain;
            }
        }
        ProcessStatus::Normal
    }

    fn layout(&self) -> GridLayout {
        GridLayout::build(vec![widgets(vec![knob(P::Gain, "Gain")])])
    }
}

truce::plugin! { logic: Gain, params: GainParams }
```

> Switch the import to `truce::prelude64::*` to write `f64` DSP
> instead — `param.read()` returns `f64`, the audio buffer is
> `f64`, and the format wrapper widens/narrows at the block
> boundary. Same `impl PluginLogic` header on both precisions.

## Format Support

By platform:

| Format | macOS | Windows | Linux | iOS |
|--------|-------|---------|-------|-----|
| CLAP   | Yes   | Yes     | Yes   | —   |
| VST3   | Yes   | Yes     | Yes   | —   |
| VST2   | Yes   | Yes     | Yes   | —   |
| LV2    | Yes   | Yes     | Yes   | —   |
| AU v2  | Yes   | —       | —     | —   |
| AU v3  | Yes   | —       | —     | Yes |
| AAX    | Yes   | Yes     | —     | —   |

AU is Apple-only by design — v2 is the legacy macOS-only component,
v3 ships on both macOS (`.appex` extension) and iOS (`.appex` inside
a container `.app` for AUM, GarageBand, Logic Pro for iPad, Cubasis,
BeatMaker 3, Loopy Pro). LV2 is the native Linux format and also
builds on macOS and Windows — supports audio, MIDI, state, and UI
(X11UI on Linux, CocoaUI on macOS, WindowsUI on Windows). AAX
requires the Avid AAX SDK and PACE/iLok signing for retail Pro Tools
releases. VST2 is opt-in on all platforms — see note below. iOS only
hosts AU v3 by platform contract; every other format is unviable
there.

## Features

- **7 plugin formats** from one codebase (CLAP, VST3 default; VST2, LV2, AU v2, AU v3, AAX opt-in)
- **Cross-platform** — macOS, Windows, Linux, plus iOS via AU v3 with the same Rust DSP, params, and editor
- **Hot reload** — edit DSP/layout, rebuild, hear changes without restarting the DAW
- **Flexible GUI frameworks** — Built-in widgets, egui, iced, slint, or raw window handle
- **Declarative params** — `#[derive(Params)]` + `#[param(...)]` with smoothing, ranges, units
- **`truce::plugin!`** — one macro generates all format exports + GUI + state serialization
- **`cargo truce`** — scaffold, build, install, validate, package; `doctor` reports environment health
- **`cargo truce package`** — signed distributable installers on both platforms (`.pkg` with notarization on macOS; Inno Setup `.exe` with Authenticode on Windows)
- **Thread-safe params** — atomic storage, lock-free access from any thread
- **Automated tests** — render, state, params, GUI screenshots, binary validation
- **Automated validation** — `cargo truce validate` runs auval, pluginval, and clap-validator in one command

## Documentation

Full docs live at **[truce.audio](https://truce.audio/)** — install
guide, first-plugin walkthrough, params / processing / GUI / audio
testing / shipping / hot-reload reference, per-format gotchas
(CLAP, VST3, VST2, LV2, AU, AAX), and current status.

## Requirements

- Rust 1.92+ (`rustup update`).
- **macOS**: Xcode CLI tools (`xcode-select --install`). Full Xcode for AU v3 + iOS.
- **Windows**: MSVC build tools (Visual Studio 2019+ with the "Desktop
  development with C++" workload). Rust `x86_64-pc-windows-msvc`
  toolchain is required.
- **Linux**: X11 + Vulkan development headers and JACK (via the PipeWire
  shim on modern distros). 
- **iOS**: full Xcode, a booted iOS Simulator (`xcrun simctl boot ...`)
  for `--ios`, or a paired & trusted device + Apple Developer team ID
  + `.mobileprovision` for `--ios-device`.
- AAX: Avid AAX SDK (optional, obtain from [developer.avid.com](https://developer.avid.com)).

## Acknowledgements

truce drew inspiration from [**nih-plug**](https://github.com/robbert-vdh/nih-plug)
by Robbert van der Helm — the trailblazing Rust audio plugin framework
whose API design, thread-safe parameter model, and overall shape
informed countless decisions here. 

## License

**Dual-licensed under [Apache-2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT) at your option.** The SPDX identifier is
`Apache-2.0 OR MIT` — pick whichever fits your project. Build,
ship, and sell plug-ins, hosts, end-user audio software, and
internal SDKs under either license. No fees, no splash screen, no
revenue cap, no email needed.

Contributions are dual-licensed on the same terms unless you
explicitly state otherwise (standard Apache-2.0 §5 inbound = outbound).

### Additional terms — commercial frameworks

One carve-out, in [`ADDITIONAL_TERMS.md`](ADDITIONAL_TERMS.md):
if you're using truce as the core of a **commercial** plug-in
framework that you redistribute to other developers — anything sold,
subscription-gated, dual-licensed commercially, or bundled into a
paid offering — a separate Framework License is required, granted
by permission. Free, OSI-licensed framework projects on top of
truce are exempt and need no separate permission. Plug-in / host /
end-user-app authors and internal-SDK use are unaffected.

See [`ADDITIONAL_TERMS.md`](ADDITIONAL_TERMS.md) for the precise
boundary, the exemption criteria, and the request procedure.

