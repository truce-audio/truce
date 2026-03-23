# Tutorials

Step-by-step guides for building audio plugins with truce.

**New to truce?** Start with the [Quickstart](../quickstart.md) —
zero to hearing your plugin in 5 minutes.

Then work through these tutorials in order:

| # | Tutorial | What you'll learn |
|---|----------|-------------------|
| 1 | [Setup](01-setup.md) | Install Rust, Xcode, and the build tools |
| 2 | [First Plugin](02-first-plugin.md) | Scaffold, build, and install a gain effect |
| 3 | [Plugin Trait](03-plugin-trait.md) | The `PluginLogic` trait — every method explained |
| 4 | [Parameters](04-parameters.md) | `#[param(...)]` attributes, ranges, smoothing, formatting |
| 5 | [Processing Audio](05-processing.md) | Effects, instruments, MIDI, sample-accurate events, transport |
| 6 | [Channel Layouts](06-channels.md) | Mono, stereo, sidechain, instruments |
| 7 | [Building a Synth](07-synth.md) | Full polyphonic synth with ADSR, filter, and GUI |
| 8 | [GUI](08-gui.md) | Built-in GUI, alternative backends (egui, vizia, iced, raw) |
| 9 | [Hot Reload](09-hot-reload.md) | Edit DSP, rebuild, hear changes without restarting the DAW |
| 10 | [State](10-state.md) | Saving extra state beyond parameters |
| 11 | [Building & Installing](11-building.md) | All formats, build commands, CI, validation |

Each tutorial builds on the previous one. Start wherever matches
your experience level.
