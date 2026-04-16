# Quickstart: Your First Plugin in 5 Minutes

From nothing to hearing your plugin in a DAW. No prior audio
plugin experience needed. Just Rust.

---

## Prerequisites

- **Rust 1.75+** (`rustup update`)
- **macOS**: `xcode-select --install`
- **Windows**: MSVC build tools (Visual Studio 2019+ with "Desktop development with C++"). Installs require an Administrator command prompt because plugin directories (Common Files, Program Files) are system-wide.
- A DAW that loads CLAP or VST3 plugins (Reaper is free to evaluate
  and the easiest to test with)

---

## Step 1: Scaffold

```bash
# Install the scaffolding tool (one-time)
cargo install --git https://github.com/truce-audio/truce cargo-truce

# Create a new plugin project
cargo truce new my-gain
cd my-gain
```

This creates a standalone project:

```
my-gain/
├── Cargo.toml          # crate with cdylib + rlib
├── truce.toml          # vendor info, plugin IDs, AU metadata
└── src/lib.rs          ← your plugin code lives here
```

---

## Step 2: Look at the Code

Open `src/lib.rs`. You'll see three things:

**1. Parameters** — what the user controls:

```rust
#[derive(Params)]
pub struct MyGainParams {
    #[param(name = "Gain", range = "linear(-60, 6)",
            unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,
}
```

One line per parameter. The `#[param(...)]` attribute defines
everything: name, range, unit, smoothing. IDs are auto-assigned
by field order (0, 1, 2, ...). The derive macro generates the rest.

**2. Processing** — what happens to the audio:

```rust
fn process(&mut self, buffer: &mut AudioBuffer, _events: &EventList,
           _context: &mut ProcessContext) -> ProcessStatus {
    for i in 0..buffer.num_samples() {
        let gain = db_to_linear(self.params.gain.smoothed_next() as f64) as f32;
        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            out[i] = inp[i] * gain;
        }
    }
    ProcessStatus::Normal
}
```

For each sample, read the smoothed gain value (in dB), convert to
linear, multiply each channel. That's it. The framework handles
threading, buffer management, and format differences.

**3. The export macro** — makes it a plugin:

```rust
truce::plugin! {
    logic: MyGain,
    params: MyGainParams,
}
```

One macro. Generates CLAP + VST3 entry points, state serialization,
parameter hosting, and GUI. Defaults to stereo bus layout. For
instruments or custom layouts, add `bus_layouts: [...]`.

---

## Step 3: Build and Install

```bash
cargo truce install --clap
```

This builds your plugin as a CLAP bundle and installs it to
`~/Library/Audio/Plug-Ins/CLAP/`. No sudo needed for CLAP.

You should see:

```
Building CLAP + VST3...
Installing CLAP: My Gain → ~/Library/Audio/Plug-Ins/CLAP/My Gain.clap
Done.
```

---

## Step 4: Load in a DAW

1. Open Reaper (or your DAW)
2. Scan for new plugins (Reaper: Options → Preferences → Plug-ins →
   VST/CLAP → Re-scan)
3. Insert the plugin on a track: Track → FX → search "My Gain"
4. Play audio through it
5. Turn the Gain knob — you should hear the volume change

You should see the plugin's GUI with a knob:

```
┌──────────────────────┐
│  MY GAIN        V0.1 │
├──────────────────────┤
│        ◎             │
│       Gain           │
│      0.0 dB          │
└──────────────────────┘
```

---

## Step 5: Edit and Rebuild

Change something in `src/lib.rs`. For example, add a pan parameter:

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

Update the layout to show it:

```rust
fn layout(&self) -> truce_gui::layout::GridLayout {
    use truce_gui::layout::{GridLayout, knob, slider, widgets};
    GridLayout::build("MY GAIN", "V0.1", 2, 50.0, vec![widgets(vec![
        knob(P::Gain, "Gain"),
        slider(P::Pan, "Pan"),
    ])])
}
```

Rebuild:

```bash
cargo truce install --clap
```

Close and reopen the plugin in your DAW. You now have a knob and a slider.

---

## Step 6: Hot Reload (Optional)

Tired of closing and reopening the plugin? Enable hot reload:

```bash
# One-time: install the hot-reload shell
cargo truce install --clap --dev

# Then iterate:
cargo watch -x "build -p my-gain"
```

Now every time you save `lib.rs`, the plugin reloads in ~2 seconds
without restarting the DAW. Edit DSP, save, hear the change.

When you're done iterating, build the final static version:

```bash
cargo truce install --clap   # no --dev = static, zero overhead
```

---

## Step 7: Package for Distribution (Optional)

When you're ready to hand the plugin to someone else, `cargo truce
install` isn't enough — it only installs on your machine. You want a
single signed installer file.

```bash
cargo truce package                    # all default-feature formats
cargo truce package --formats clap,vst3 # subset
cargo truce package --no-sign          # skip signing (dev builds)
```

You'll find the installer in `dist/`:

- **macOS**: `dist/MyGain-0.1.0-macos.pkg` — a `.pkg` installer with
  format-selection checkboxes, Developer ID signing, and (if configured)
  notarization + stapling.
- **Windows**: `dist/My Gain-0.1.0-windows-x64.exe` — an Inno Setup
  installer with per-format components, Authenticode signing, and a
  registered uninstaller.

Users double-click it and your plugin is installed. No Rust toolchain
required on their end.

### Signing

For distribution you need a code-signing identity so SmartScreen
(Windows) or Gatekeeper (macOS) doesn't flag the installer as "unknown
publisher." Configure it in `truce.toml`:

```toml
[macos]
signing_identity = "Developer ID Application: Your Name (TEAMID)"

[macos.packaging]
installer_identity = "Developer ID Installer: Your Name (TEAMID)"
notarize = true

[windows.signing]
# Easiest: Azure Trusted Signing (~$120/yr, no hardware token)
azure_account = "YourSigningAccount"
azure_profile = "YourCertificateProfile"
# Or: existing cert in Windows cert store, by SHA1 thumbprint
# sha1 = "abc123..."
# Or: .pfx file with password from env
# pfx_path = "path/to/cert.pfx"      # + set TRUCE_PFX_PASSWORD
```

Without any credentials, `cargo truce package` still runs — it just
emits unsigned binaries and prints a warning. `--no-sign` silences it
for dev builds.

Requires [Inno Setup 6](https://jrsoftware.org/isinfo.php) on Windows
(auto-discovered, doesn't need to be on `%PATH%`). `cargo truce doctor`
tells you what's missing.

---

## What Just Happened

With one crate, one file, and one macro, you built a plugin that:

- Loads in any DAW supporting CLAP (Reaper, Bitwig, etc.)
- Has a parameter with smoothing (no zipper noise)
- Renders a GUI with interactive knobs
- Saves and restores state when the DAW session is saved
- Passes clap-validator (if installed)

To also build VST3, AU, and AAX:

```bash
cargo truce install                # all formats
cargo truce install --vst3         # VST3 (for Ableton, FL Studio)
cargo truce install --au2          # AU v2 (for Logic Pro)
cargo truce install --au3          # AU v3 (for Logic Pro, Ableton)
cargo truce install --aax          # AAX (for Pro Tools, needs SDK)
```

---

## Common Next Steps

**Add more parameters:**
```rust
#[param(name = "Bypass", flags = "automatable | bypass")]
pub bypass: BoolParam,
```

Bool params auto-detect as toggle switches. Enum params auto-detect
as click-to-cycle selectors.

**Add meters and more widgets:**
```rust
fn layout(&self) -> truce_gui::layout::GridLayout {
    use truce_gui::layout::{GridLayout, knob, slider, toggle, meter, widgets};
    GridLayout::build("MY GAIN", "V0.1", 3, 50.0, vec![widgets(vec![
        knob(P::Gain, "Gain"),
        slider(P::Pan, "Pan"),
        toggle(P::Bypass, "Bypass"),
        meter(&[P::MeterLeft, P::MeterRight], "Level").rows(2),
    ])])
}
```

**Handle MIDI** (for instruments):
```rust
fn process(&mut self, buffer: &mut AudioBuffer, events: &EventList,
           _ctx: &mut ProcessContext) -> ProcessStatus {
    for event in events.iter() {
        match &event.body {
            EventBody::NoteOn { note, velocity, .. } => {
                // Start a voice
            }
            EventBody::NoteOff { note, .. } => {
                // Release the voice
            }
            _ => {}
        }
    }
    // Render audio...
    ProcessStatus::Normal
}
```

**Run tests:**
```bash
cargo truce test        # in-process plugin tests
cargo truce validate    # auval + pluginval + clap-validator
```

---

## Where to Go From Here

- [Reference](reference/) — parameters, processing, synth, GUI, hot reload,
  MIDI, channel layouts, state, custom formatting
- [Layout](layout.md) — all widget types, grid layouts, layout DSL
- [Hot Reload](reference/09-hot-reload.md) — detailed hot-reload setup
- [GUI Backends](gui/) — egui, iced, raw window handle
- [Examples](../examples/) — gain, gain-iced, EQ, synth, transpose, arpeggio
- API reference: `cargo doc --open`
