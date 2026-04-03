## Build, install, standalone

### Configuration

Before building, create a `truce.toml` in your project root:

```toml
[vendor]
name = "My Company"
id = "com.mycompany"
au_manufacturer = "MyCo"         # 4-char code

[[plugin]]
name = "My Effect"
suffix = "effect"
crate = "my-effect"              # cargo package name
category = "effect"              # "effect", "instrument", or "midi"
fourcc = "MyFx"                  # 4-char code (AU subtype, CLAP feature ID, etc.)
au3_subtype = "MyF3"             # 4-char AU v3 subtype (optional, defaults to fourcc)
au_tag = "Effects"
```

The `[vendor]` section defines your company identity. Each `[[plugin]]`
entry defines a plugin to build and install. The `fourcc` field and
AU fields (`au_manufacturer`) must be 4-character codes that
uniquely identify your plugin. The `category` field determines the plugin type and host integration:

- `"effect"` — audio effect (CLAP: audio-effect, VST3: Fx, AU: aufx)
- `"instrument"` — synthesizer/sampler (CLAP: instrument, VST3: Instrument|Synth, AU: aumu)
- `"midi"` — MIDI note effect like transpose or arpeggiator (CLAP: note-effect, VST3: Fx|Event, AU: aumi)

`au3_subtype` is optional — if omitted, it defaults to `fourcc` (v2 and v3 share the same code).

### Build environment

Build-machine settings (signing, SDK paths) are configured via
environment variables, not `truce.toml`. This keeps secrets and
machine-specific paths out of the repo.

| Variable | Purpose | Default |
|---|---|---|
| `TRUCE_SIGNING_IDENTITY` | Code signing identity | `"-"` (ad-hoc) |
| `TRUCE_INSTALLER_SIGNING_IDENTITY` | Installer signing identity (.pkg) | none |
| `AAX_SDK_PATH` | AAX SDK root directory | none |

Set these in your shell profile or in `.cargo/config.toml` (gitignored):

```toml
[env]
TRUCE_SIGNING_IDENTITY = "Developer ID Application: Your Name (TEAMID)"
AAX_SDK_PATH = "/path/to/aax-sdk"
```

`cargo truce` reads `.cargo/config.toml` directly, so this works for
both `cargo truce install` and `cargo truce package`.

All three can be overridden in `truce.toml` under `[macos]` if needed,
but env vars are preferred.

### Building and installing with xtask

The recommended way to build and install plugins:

```sh
# Build and install all formats (CLAP + VST3 + VST2 + AU v2 + AU v3 + AAX)
cargo truce install

# Install specific formats
cargo truce install --clap       # CLAP only (no sudo needed)
cargo truce install --vst3       # VST3 only
cargo truce install --vst2       # VST2 only
cargo truce install --au2        # AU v2 only (.component, needs Developer ID)
cargo truce install --au3        # AU v3 only (.appex, requires Xcode)
cargo truce install --aax        # AAX only (requires AAX SDK)
cargo truce install -p gain      # Single plugin, all formats
cargo truce install --au3 -p gain # AU v3 only, just Gain

# Install without rebuilding
cargo truce install --no-build

# Check what's installed
cargo truce status

# Run AU validation
cargo truce validate

# Clear all AU/DAW caches (useful when things get stuck)
cargo truce clean
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
cargo truce install --au2
```

**AU v3 (macOS only, requires Xcode):**

```sh
cargo truce install --au3
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

Use `cargo truce install` for all formats, or `cargo truce install -p gain --au3`
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
use truce_example_gain::{Gain, GainParams};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--render-gui") {
        let params = std::sync::Arc::new(GainParams::new());
        let gain = Gain::new(std::sync::Arc::clone(&params));
        truce_standalone::render_gui_png(params, gain.layout(), "gain-gui.png");
        return;
    }

    if args.iter().any(|a| a == "--no-gui") {
        truce_standalone::run::<Gain>();
        return;
    }

    let params = std::sync::Arc::new(GainParams::new());
    let gain = Gain::new(std::sync::Arc::clone(&params));
    truce_standalone::run_with_gui::<Gain>(gain.layout());
}
```

### Validation

```sh
# Run all validators (auval + pluginval)
cargo truce validate

# AU validation only
cargo truce validate --auval

# VST3 validation only (requires pluginval installed)
cargo truce validate --pluginval

# Run the 80+ in-process regression tests
cargo truce test
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

[← Previous](10-state.md) | [Index](README.md)
