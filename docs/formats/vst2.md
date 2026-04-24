# VST2

VST2 is the legacy Steinberg plugin format. It's been superseded by
VST3 but is still the format of choice for older Reaper projects,
Ableton Live 10 and earlier, and FL Studio's "prefer VST2" setting.
Truce ships clean-room C bindings (no Steinberg SDK headers) so the
`truce-vst2` crate itself is MIT/Apache-2.0 licensed.

## Status

Opt-in. Working on all three platforms. Tested in Reaper on
macOS, Windows, and Linux; Ableton Live 11 on macOS; FL Studio on
Windows.

## ⚠️ Licensing

Steinberg **deprecated the VST2 SDK in October 2018** and no longer
issues new VST2 license agreements. Distributing VST2 plugins may
require an existing Steinberg agreement.

Truce's clean-room C shim (`truce-vst2/shim/vst2_shim.c`) uses no
Steinberg SDK headers and carries no Steinberg copyright, so the
crate itself is redistributable under MIT/Apache-2.0. What is still
licensed by Steinberg:

- The **VST trademark** and logo.
- The VST2 identifier codes you register (if any).
- Hosting / display of VST2 plug-ins in Steinberg's own products.

If you're shipping a VST2 plugin commercially in 2026, consult
counsel. For personal use, internal tooling, or open-source plugins
targeting hobbyist users the risk is low — but this doc is not
legal advice.

Enable this feature only if you understand the licensing
implications.

## Enable

VST2 is **opt-in**. Either add it to `[features].default` in
`Cargo.toml` or pass `--vst2` to `cargo truce`.

```toml
[features]
default = ["clap", "vst3", "vst2"]
vst2 = ["dep:truce-vst2"]
```

```sh
cargo truce install --vst2
```

## Requirements

- A C compiler. Xcode CLI tools (macOS), MSVC (Windows), or
  GCC/Clang (Linux). The shim is a single `.c` file.

No Steinberg SDK required.

## Install paths

| Platform | Path |
|----------|------|
| macOS | `~/Library/Audio/VST/{Name}.dylib` |
| Windows | `%PROGRAMFILES%\Steinberg\VstPlugins\{Name}.dll` (admin) |
| Linux | `~/.vst/{Name}.so` |

On Windows the default Steinberg `VstPlugins` path is what Reaper
and most hosts scan out of the box. It's under `Program Files`, so
`cargo truce install --vst2` requires an Administrator prompt.

Unlike VST3, VST2 has no bundle hierarchy — a single shared library
file is the plugin.

## Signing

- **macOS**: codesigned with `$TRUCE_SIGNING_IDENTITY` on install.
- **Windows**: Authenticode-signed at packaging time via `signtool`.
- **Linux**: no signing.

## Build / install / package

```sh
cargo truce install --vst2
cargo truce build --vst2
cargo truce package --formats vst2
```

## Validate

`cargo truce validate` runs `pluginval` against installed VST2
bundles (strictness 5), same as VST3.

## Hosts

| Host | Platform | Status |
|------|----------|--------|
| Reaper | macOS / Windows / Linux | working (Linux: plugin installs correctly; Reaper's browser may not surface it — investigation open) |
| Ableton Live 10 / 11 | macOS / Windows | working |
| FL Studio | Windows | working |
| Logic Pro | — | AU only, no VST2 |
| Pro Tools | — | AAX only, no VST2 |

## Gotchas

- **Four-char plugin ID**: `fourcc` in `truce.toml` becomes the VST2
  unique ID (cast to `int32`). Must be stable across releases — hosts
  key project state on it. First character is traditionally
  uppercase ASCII to stay above the VST2 "manufacturer-reserved"
  cutoff.
- **No MIDI 2.0**: VST2 has no UMP pipeline. Truce plugins will only
  see MIDI 1.0 events when loaded via VST2, even if the plugin's
  `EventBody` enum supports MIDI 2.0 variants on other formats.
- **No sample-accurate automation**: VST2's parameter model is
  block-rate. If your plugin relies on `ParamChange` events landing
  between audio samples (CLAP, VST3), VST2 will only deliver them at
  block boundaries.
- **Reaper-Linux browser bug**: VST2 bundles install to `~/.vst/`
  correctly, but Reaper for Linux may not surface them in its plugin
  browser. Workaround: symlink into Reaper's custom VST search path.
