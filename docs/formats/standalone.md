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

Two mechanical additions to your plugin crate. No `lib.rs` edits.

**`Cargo.toml`:**

```toml
[[bin]]
name = "<bundle_id>-standalone"
path = "src/main.rs"
required-features = ["standalone"]

[features]
standalone = ["dep:truce-standalone"]

[dependencies]
truce-standalone = { workspace = true, features = ["gui"], optional = true }
```

`<bundle_id>` here is your plugin's `bundle_id` field from `truce.toml`
(e.g. `gain-standalone`).

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
  --sample-rate <hz>       e.g. 44100, 48000, 96000
  --buffer <frames>        Audio buffer size
  --midi-input <name>      MIDI input device (substring match)
  --bpm <n>                Transport BPM (default 120)
  --state <path>           Load plugin state from this file on launch
  -h, --help               Show this message
```

## In-window hotkeys

- **SPACE** — toggle transport play / stop
- **Ctrl-S / Cmd-S** — quick-save plugin state to
  `$XDG_DATA_HOME/truce/<slug>/quicksave-<ts>.state`
- **Z / X** — shift QWERTY-MIDI octave down / up
- **A S D F G H J K L ;** — white keys (C D E F G A B C D E)
- **W E T Y U O P** — black keys

The QWERTY-to-MIDI mapping is keyed on physical key positions, so
AZERTY / Dvorak / etc. map to the same piano keys.

## Settings precedence

Each setting resolves first-match-wins:

1. CLI flag (`--output "…"`)
2. Environment variable (`TRUCE_STANDALONE_OUTPUT="…"`)
3. Config file
4. cpal / midir default

Config-file location:

| OS | Path |
|----|------|
| macOS | `~/Library/Application Support/truce/standalone.toml` |
| Linux | `$XDG_CONFIG_HOME/truce/standalone.toml` |
| Windows | `%APPDATA%\truce\standalone.toml` |

```toml
default_output = "External Headphones"
default_input = "Built-in Microphone"
default_sample_rate = 48000
default_buffer = 512
default_midi_input = "IAC Bus 1"
default_bpm = 120.0
```

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

`truce-standalone`'s `in_process` module runs the plugin headlessly
in memory — no cpal, no window, no devices. Captures output audio
and meter readings; drives MIDI via a scripting helper.

```rust
use std::time::Duration;
use truce_test::in_process;

let result = in_process::run::<Plugin>(
    in_process::InProcessOpts::default()
        .sample_rate(48_000.0)
        .midi(|m| { m.note_on(60, 0.8); m.wait_ms(100); m.note_off(60); })
        .duration(Duration::from_secs(3)),
);

in_process::assert_no_nans(&result);
in_process::assert_nonzero(&result);
in_process::assert_silence_after(&result, Duration::from_millis(2_500));
```

Opt in from the plugin crate:

```toml
[dev-dependencies]
truce-test = { workspace = true, features = ["in-process"] }
```

Good for tail-silence / release-decay tests, sustained-load stability,
clipping guards, and MIDI-recorded regression tests.

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
