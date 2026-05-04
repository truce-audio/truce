# Standalone

A standalone binary mode of your plugin: drives the DSP against your
OS audio driver directly and opens your plugin's editor in its own
window. Not a plugin format a DAW loads — a way to run the same crate
as an app.

## Status

Shipped on all three platforms. Every documented example ships a
standalone binary.

| Platform | Audio backend | GUI |
|----------|---------------|-----|
| macOS | CoreAudio (via cpal) | ✅ |
| Windows | WASAPI (via cpal) | ✅ |
| Linux | ALSA or PipeWire-JACK (via cpal) | ✅ (X11; Wayland via XWayland) |

## Enable

Plugins scaffolded with `cargo truce new` (or `new --workspace`) ship
the standalone host enabled out of the box — `Cargo.toml` lists
`standalone` in `default = [...]` and `src/main.rs` is pre-written.
If you opted out at scaffold time with `--no-standalone`, or you're
adding standalone to an existing crate, do the two mechanical
additions below. No `lib.rs` edits.

**`Cargo.toml`:**

```toml
[[bin]]
name = "<crate_name>-standalone"
path = "src/main.rs"
required-features = ["standalone"]

[features]
standalone = ["dep:truce-standalone"]

[dependencies]
truce-standalone = { workspace = true, features = ["gui"], optional = true }
```

`<crate_name>` is your crate's `[package].name` (e.g. `truce-example-gain`,
yielding `truce-example-gain-standalone`). `cargo truce run` looks for the
binary at `target/<profile>/<crate_name>-standalone(.exe)`, so the
`[[bin]] name` must match this convention.

**`src/main.rs`:**

```rust
use my_plugin::Plugin;

fn main() {
    truce_standalone::run::<Plugin>();
}
```

## Run

```sh
cargo truce run -p <crate>             # build + stage + launch
cargo truce run -p <crate> -- --help   # pass flags through to the binary
```

`cargo truce run` handles feature + bin selection and stages the
binary into `target/bundles/{Plugin}.standalone/` alongside every
other truce-produced artifact.

## CLI flags

```
<plugin>-standalone [OPTIONS]

  --headless               Run audio only; no window
  --list-devices           List audio output + input devices and exit
  --list-midi              List MIDI input devices and exit
  --output <name>          Audio output device (substring match)
  --input <name>           Audio input device (for effect plugins)
  --input-enabled <on|off> Enable mic input at launch (default: off).
                           Press `I` in the window to toggle live.
  --output-enabled <on|off> Enable speaker output at launch (default: on).
                           Toggle live from the Plugin menu (Cmd+O / Ctrl+O).
  --sample-rate <hz>       e.g. 44100, 48000, 96000
  --buffer <frames>        Audio buffer size
  --midi-input <name>      MIDI input device (substring match)
  --bpm <n>                Transport BPM (default 120)
  --state <path>           Load plugin state from this file on launch
  -h, --help               Show this message
```

### `playback` feature: WAV in / WAV out

The optional `playback` feature on `truce-standalone` adds three
flags for piping `.wav` files in and out of the plugin without an
audio interface — useful for snapshot regression tests, batch
rendering, and CI runs on headless build agents. Enable from your
plugin crate with:

```toml
[features]
standalone-playback = ["standalone", "truce-standalone/playback"]
```

…then build the binary with `--features standalone-playback`.

```
  --input-file <path>      Decode <path>.wav and feed it into the
                           plugin's input bus. One-shot — plays
                           once, then the file channel goes silent.
                           Mic + file sum when both are enabled.
                           Linear-interp resample if the file's SR
                           differs from the device's; channel-count
                           mismatches are soft-warned and adapted.
  --output-file <path>     Capture the plugin's output bus to
                           <path>.wav (32-bit float, pre-mute).
                           Implies --headless. Real-time by default
                           (cpal still drives the audio thread);
                           pair with --no-playback for offline.
  --no-playback            Bypass cpal entirely; render as fast as
                           the CPU allows. Requires both
                           --input-file and --output-file
                           (otherwise ignored with a warn).
```

Common shapes:

```sh
# Real-time capture: hear it while it records.
truce-example-gain-standalone --input-file in.wav --output-file out.wav

# Offline render — sub-real-time, no audio device touched.
# This is the CI / batch recipe.
truce-example-gain-standalone --no-playback --input-file in.wav --output-file out.wav
```

In offline mode the runner inherits the input WAV's sample rate
and channel count by default (override with `--sample-rate` if
needed); output WAV is always 32-bit float at the resolved SR.
Mute (`--output-enabled off`) silences the speakers but **does
not** affect what `--output-file` captures — bounce-to-disk
behaviour matches what every DAW does.

## In-window hotkeys

- **SPACE** — toggle transport play / stop
- **Ctrl-S / Cmd-S** — quick-save plugin state to
  `$XDG_DATA_HOME/truce/<slug>/quicksave-<ts>.state`
- **I** — toggle mic input (effect plugins only). First press on
  macOS triggers the system permission prompt.
- **Cmd-O / Ctrl-O** — toggle audio output (mute / unmute). Plugin
  keeps processing — meters, transport, MIDI all still tick — only
  the speaker output is zeroed.
- **Z / X** — shift QWERTY-MIDI octave down / up
- **A S D F G H J K L ;** — white keys (C D E F G A B C D E)
- **W E T Y U O P** — black keys

The QWERTY-to-MIDI mapping is keyed on physical key positions, so
AZERTY / Dvorak / etc. map to the same piano keys.

## Settings precedence

Each setting resolves first-match-wins:

1. CLI flag (`--output "…"`)
2. Environment variable (`TRUCE_STANDALONE_OUTPUT="…"`)
3. Plugin-author defaults via `run_with::<P>(Defaults { … })`
4. Compiled runtime default (input off, output on, cpal-picked
   devices)

Plugin authors can pin `input_enabled` / `output_enabled` defaults
in code without giving up CLI / env override:

```rust
use truce_standalone::{run_with, Defaults};

fn main() {
    run_with::<my_plugin::Plugin>(Defaults {
        input_enabled: Some(true),  // effect plugin wants mic on by default
        ..Defaults::default()
    });
}
```

Other settings (device names, sample rate, buffer size, MIDI
input, BPM, state path) are intentionally CLI/env-only — those
are per-machine concerns the plugin author shouldn't be pinning
in code.

## MIDI

`midir`-based input with substring matching on port names. A
background thread polls at 1 Hz for hot-plug; disconnect falls back
to QWERTY, reconnect is silent.

Supported: MIDI 1.0 note on / off (velocity-0 note-on decodes as
note-off), CC, pitch bend, channel pressure. No sysex, no MPE, no
MIDI 2.0.

## Transport

Minimal — sufficient for LFOs, tempo-synced delays, arpeggiators. Not
a DAW timeline.

- `tempo`: set via `--bpm` or config; default 120
- `playing`: SPACE toggles; default stopped
- `position_beats`: advances while playing

Atomic-backed — UI-thread toggles don't block the audio thread.

## Integration tests

The same engine that backs `--no-playback` also drives in-process
audio tests via [`truce_test::PluginDriver`](../reference/audio-testing.md).
No cpal, no window, no devices — instantiate the plugin, feed
scripted audio + MIDI for a fixed duration, capture the output:

```rust
use std::time::Duration;
use truce_test::{assertions, driver};

let result = driver!(Plugin)
    .sample_rate(48_000.0)
    .duration(Duration::from_secs(3))
    .script(|s| {
        s.note_on(60, 0.8);
        s.wait_ms(100);
        s.note_off(60);
    })
    .run();

assertions::assert_no_nans(&result);
assertions::assert_nonzero(&result);
assertions::assert_silence_after(&result, Duration::from_millis(2_500));
```

Opt in from the plugin crate:

```toml
[dev-dependencies]
truce-test = { workspace = true }
```

Good for tail-silence / release-decay tests, sustained-load stability,
clipping guards, and MIDI-recorded regression tests. See
[`../reference/audio-testing.md`](../reference/audio-testing.md) for
the full builder surface — input shapes, state-file loading,
per-block meters, output-event capture.

## Limitations

- **Not a distributable**. Hosts don't load standalones, and there's
  no `.app` / `.exe` / AppImage packaging yet — distribute from
  `cargo build --release` output manually.
- **Wayland** unparented top-level windows still lag X11; XWayland is
  the validated path.
- **No parameter automation record / replay**.
- **State files have no migration layer**: bumping `STATE_VERSION`
  invalidates saved `.state` files; the plugin logs a clear error.

## See also

- [`../reference/shipping.md`](../reference/shipping.md) — full CLI reference
- [`README.md`](README.md) — format index
