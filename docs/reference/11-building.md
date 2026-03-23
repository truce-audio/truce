## Build, install, standalone

### Configuration

Before building, copy `truce.toml.example` to `truce.toml` and edit:

```toml
[macos]
signing_identity = "-"           # or your Developer ID certificate
deployment_target = "11.0"

[vendor]
name = "My Company"
id = "com.mycompany"
au_manufacturer = "MyCo"         # 4-char code

[[plugin]]
name = "My Effect"
suffix = "effect"
crate = "my-effect"              # cargo package name
au_type = "aufx"                 # aufx = effect, aumu = instrument
au_subtype = "MyFx"              # 4-char AU v2 subtype code
au3_subtype = "MyF3"             # 4-char AU v3 subtype (optional, defaults to au_subtype)
au_tag = "Effects"
```

The `[vendor]` section defines your company identity. Each `[[plugin]]`
entry defines a plugin to build and install. The AU fields (`au_type`,
`au_subtype`, `au_manufacturer`) must be 4-character codes that uniquely
identify your plugin in the AU system. `au3_subtype` is optional — if
omitted, it defaults to `au_subtype` (v2 and v3 share the same subtype).

### Building and installing with xtask

The recommended way to build and install plugins:

```sh
# Build and install all formats (CLAP + VST3 + VST2 + AU v2 + AU v3 + AAX)
cargo xtask install

# Install specific formats
cargo xtask install --clap       # CLAP only (no sudo needed)
cargo xtask install --vst3       # VST3 only
cargo xtask install --vst2       # VST2 only
cargo xtask install --au2        # AU v2 only (.component, needs Developer ID)
cargo xtask install --au3        # AU v3 only (.appex, requires Xcode)
cargo xtask install --aax        # AAX only (requires AAX SDK)
cargo xtask install --gpu        # enable wgpu GPU rendering backend
cargo xtask install -p gain      # Single plugin, all formats
cargo xtask install --au3 -p gain # AU v3 only, just Gain

# Install without rebuilding
cargo xtask install --no-build

# Check what's installed
cargo xtask status

# Run AU validation
cargo xtask validate

# Clear all AU/DAW caches (useful when things get stuck)
cargo xtask clean
```

### Manual building

You can also build directly with cargo:

```sh
# Build the cdylib (CLAP + VST3)
cargo build --release -p truce-example-gain

# Output:
#   macOS:   target/release/libtruce_example_gain.dylib
#   Linux:   target/release/libtruce_example_gain.so
#   Windows: target/release/truce_example_gain.dll
```

### Manual installing

**CLAP:**

```sh
# macOS
cp target/release/libtruce_example_gain.dylib \
   ~/Library/Audio/Plug-Ins/CLAP/Gain.clap

# Linux
cp target/release/libtruce_example_gain.so \
   ~/.clap/Gain.clap
```

**VST3 (requires proper bundle structure):**

```sh
# macOS
mkdir -p ~/Library/Audio/Plug-Ins/VST3/Gain.vst3/Contents/MacOS
cp target/release/libtruce_example_gain.dylib \
   ~/Library/Audio/Plug-Ins/VST3/Gain.vst3/Contents/MacOS/Gain
```

**AU v2 (macOS only):**

```sh
# Use xtask (handles plist, signing, cache clearing)
cargo xtask install --au2
```

**AU v3 (macOS only, requires Xcode):**

```sh
cargo xtask install --au3
```

### Host compatibility

| Format | Reaper | Logic Pro | GarageBand | Ableton | FL Studio | Pro Tools | Custom GUI |
|--------|--------|-----------|------------|---------|-----------|-----------|------------|
| CLAP | ✅ | N/A | N/A | N/A | N/A | N/A | ✅ knobs |
| VST3 | ✅ | N/A | N/A | ✅ | ✅ | N/A | ✅ knobs |
| VST2 | ✅ | N/A | N/A | ✅ | ✅ | N/A | ✅ knobs |
| AU v2 | ✅ | ✅ | ✅ (no GUI*) | ✅ | N/A | N/A | ✅ knobs |
| AU v3 | N/A | ✅ | ✅ (no GUI*) | ✅ | N/A | N/A | ✅ knobs |
| AAX | N/A | N/A | N/A | N/A | N/A | ✅ | ✅ knobs |

\* GarageBand does not show custom GUI for any third-party plugin.
See the per-format sections above for host details and cache management.

Use `cargo xtask install` for all formats, or `cargo xtask install -p gain --au3`
for a specific plugin and format.

### Standalone mode

The standalone binary is built from `src/main.rs` in the same
crate. No separate standalone crate needed.

```sh
# Build and run the synth standalone (opens a window, play QWERTY keys)
cargo build --release -p truce-example-synth --features standalone
./target/release/synth-standalone
```

The standalone entry point imports from the same crate:

```rust
use truce_example_gain::{Gain, GainParams, gui_layout};

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

### Validation

```sh
# Run all validators (auval + pluginval)
cargo xtask validate

# AU validation only
cargo xtask validate --auval

# VST3 validation only (requires pluginval installed)
cargo xtask validate --pluginval

# Run the 80+ in-process regression tests
cargo xtask test
```

### CI example (GitHub Actions)

```yaml
name: Build Plugin
on: [push]

jobs:
  build-mac:
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: aarch64-apple-darwin, x86_64-apple-darwin
      - run: cargo build --release -p my-gain
      - uses: actions/upload-artifact@v4
        with:
          name: macos-plugins
          path: target/release/libmy_gain.dylib
```

---

## What to read next

- `examples/gain/` — stereo gain with pan and bypass (what this tutorial builds)
- `examples/eq/` — 3-band parametric EQ with biquad filters
- `examples/synth/` — polyphonic synth with filter, envelope, and GUI
- `examples/transpose/` — MIDI effect: real-time note transposition
- `examples/arpeggio/` — MIDI effect: arpeggiator with pattern, rate, and octave range
- [Hot reload guide](09-hot-reload.md) — hot-reload setup (`--features dev`), troubleshooting, and internals
- API reference: `cargo doc --open`

---

[← Previous](10-state.md) | [Next →](12-egui.md) | [Index](README.md)
