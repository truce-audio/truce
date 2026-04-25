# CLAP

CLAP (CLever Audio Plug-in) is the open plugin standard from Bitwig
and u-he, licensed under MIT. It's the format truce is most at home
in — native Rust, no C++ shim, first-class parameter modulation.

## Status

Production. Shipped in the scaffold defaults. Tested in Reaper on
macOS, Windows, and Linux.

## Enable

Already on in scaffolded plugins. Otherwise:

```toml
[features]
default = ["clap", "vst3"]
clap = ["dep:truce-clap", "dep:clap-sys"]
```

## Requirements

- Nothing beyond a working Rust toolchain. No SDK, no env vars, no
  external signing for user-scope installs.

## Install paths

User-scope on all three platforms — no admin / sudo needed:

| Platform | Path |
|----------|------|
| macOS | `~/Library/Audio/Plug-Ins/CLAP/{Name}.clap` |
| Windows | `%COMMONPROGRAMFILES%\CLAP\{Name}.clap` (admin) |
| Linux | `~/.clap/{Name}.clap` |

Windows writes under `Common Files`, which does need an Administrator
prompt; macOS and Linux write user-scope.

On every platform the `.clap` bundle is just the built cdylib
renamed with a `.clap` extension. No `Contents/` hierarchy, no
`Info.plist`, no resources.

## Signing

- **macOS**: `cargo truce install` codesigns with
  `$TRUCE_SIGNING_IDENTITY` (ad-hoc `-` by default). CLAP doesn't
  require Developer ID for local installs.
- **Windows**: no signing for `install`; `cargo truce package` wraps
  the bundle in an Authenticode-signed installer.
- **Linux**: no signing.

## Build / install / package

```sh
cargo truce install --clap       # build + install just CLAP
cargo truce install              # all default-enabled formats (CLAP is
                                  # on by default)
cargo truce build --clap         # bundle into target/bundles/ without
                                  # installing
cargo truce package --formats clap   # signed installer with only CLAP
```

## Validate

`cargo truce validate` runs [clap-validator] in-process if it's on
your `PATH`, or set `CLAP_VALIDATOR` to point at the binary. The
validator exercises `init` / `activate` / `start_processing` /
`process` / `deactivate` / `destroy` lifecycles, parameter queries,
state round-trips, and buffer polarity.

[clap-validator]: https://github.com/free-audio/clap-validator

## Hosts

| Host | Status |
|------|--------|
| Reaper (macOS / Windows / Linux) | primary testbed |
| Bitwig Studio | should work (native CLAP support); validation pending |
| MultitrackStudio | should work; validation pending |

## Gotchas

- **Parameter modulation** (`ParamMod` events) is CLAP-specific.
  Plugins that consume `EventBody::ParamMod` will see modulation in
  CLAP hosts and nothing from VST3/AU/AAX hosts (those formats don't
  expose CLAP-style per-voice modulation).
- **`clap_id`** from `truce.toml` (or auto-derived from vendor + plugin
  `bundle_id`) is what CLAP hosts use to identify your plugin. Do not
  change it after release — automation and preset associations are
  keyed on it.
