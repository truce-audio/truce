# truce-xtask

Build system core for truce audio plugins.

## Overview

Implements the non-scaffolding half of `cargo truce`: compilation, bundling
into platform-specific formats, code signing, installation to system plugin
directories, packaging into signed installers, and host validation. Every
supported format passes through — CLAP, VST3, VST2, LV2, AU v2, AU v3, AAX.

On macOS, produces universal binaries (arm64 + x86_64) via `lipo`. On Windows,
stages per-arch `.dll`s inside VST3 / AAX bundle subdirectories and gates
CLAP / VST2 single-file installs via Inno Setup `Check:` predicates.

## Commands

| Command | Description |
|---------|-------------|
| `install` | Build + sign + install into the standard plugin directories. User-scope on macOS (CLAP) and Linux; sudo / admin on macOS `/Library/...` and Windows system paths. |
| `build` | Same build + sign pipeline as `install`, but writes signed bundles to `target/bundles/` without touching system paths. |
| `package` | Produce a distributable installer: `.pkg` + notarization on macOS, Inno Setup `.exe` + Authenticode + PACE wraptool for AAX on Windows. |
| `run` | Build a plugin's `--features standalone` binary, stage it into `target/bundles/{Name}.standalone`, and launch. |
| `test` | Run per-plugin integration tests (render, state round-trip, GUI snapshot, bus config). |
| `status` | Show installed plugin versions and bundle paths. |
| `validate` | Run `auval` (AU v2 + v3), `pluginval` (VST3), `clap-validator` (CLAP). Skips any tool that isn't on `PATH`. |
| `remove` | Uninstall a specific plugin from every format's install directory. |
| `clean` | Drop scratch under `target/tmp/`, clear AU / DAW caches. |
| `nuke` | Uninstall every truce plugin + restart `pkd` / `AudioComponentRegistrar`. Recovery tool. |
| `doctor` | Preflight: toolchain, SDKs, signing identities, rustup targets. |
| `log` | Tail recent unified-log entries from loaded plugins. |

Scaffolding (`new`, `new-workspace`) lives in `cargo-truce` directly —
not here.

## Architecture

Not used directly. `cargo-truce` re-exports `truce_xtask::run(args)`
for most commands; the binary crate owns `new` and `new-workspace`
because they're entry-point-only (no cross-plugin shared state).

Part of [truce](https://github.com/truce-audio/truce).
