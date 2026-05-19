# cargo-truce

Cargo subcommand for building truce audio plugins.

## Overview

The `cargo truce` CLI. Handles scaffolding new plugin projects,
building and bundling for all supported formats, code signing,
installing into the host's plug-in directories, and validating with
host-specific tools.

## Installation

```sh
cargo install cargo-truce
```

## Commands

```sh
cargo truce new my-plugin          # scaffold a single-plugin project (clap+vst3+standalone by default)
cargo truce new my-plugin --no-standalone  # … without the standalone host bin
cargo truce new my-ws --workspace gain reverb  # scaffold a multi-plugin workspace
cargo truce install                # build + bundle + sign + install (per-user by default)
cargo truce install --system       # install for all users (sudo / admin)
cargo truce install --clap         # single format only
cargo truce build                  # bundle into target/bundles/ without installing
cargo truce package                # build a signed .pkg / .exe in target/dist/
cargo truce uninstall              # remove installed plugins (mirrors install scope flags)
cargo truce validate               # run auval (AU) + pluginval (VST3) + clap-validator
cargo truce doctor                 # check toolchain, SDKs, signing certs, install paths
cargo truce run                    # build and launch standalone
cargo truce screenshot             # render every plugin's GUI to target/screenshots/
cargo truce status                 # show installed plugin versions
```

## Supported formats

CLAP, VST3, VST2, LV2, Audio Unit v2 + v3 (macOS), and AAX (Pro Tools,
macOS / Windows).

## Library API

cargo-truce ships as both a binary and a library (`cargo_truce` crate).
The library half (`cargo_truce::run`, `cargo_truce::scaffold::*`) is the
engine for the build/install/package pipelines; the binary is a thin
arg-parsing shell that drives it. Embedding the engine in your own
tooling is supported but mostly intended for internal use - most
plugin authors only need the `cargo truce` CLI.

Part of [truce](https://github.com/truce-audio/truce).
