# cargo-truce

Cargo subcommand for building truce audio plugins.

## Overview

A thin wrapper around `truce-xtask` that provides the `cargo truce` CLI.
Handles scaffolding new plugin projects, building and bundling for all
supported formats, code signing, installation to system plugin directories,
and validation with host-specific tools.

## Installation

```sh
cargo install --git https://github.com/truce-audio/truce cargo-truce
```

## Commands

```sh
cargo truce new my-plugin          # scaffold a new plugin project
cargo truce new-workspace my-ws    # scaffold a multi-plugin workspace
cargo truce install                # build + bundle + sign + install all formats
cargo truce install --clap         # single format only
cargo truce validate               # run auval (AU) + pluginval (CLAP/VST3)
cargo truce doctor                 # check toolchain, SDKs, signing certs
cargo truce run                    # build and launch standalone
cargo truce status                 # show installed plugin versions
cargo truce nuke                   # remove installed plugins
```

## Supported formats

CLAP, VST3, VST2, Audio Unit (macOS), and AAX (Pro Tools).

Part of [truce](https://github.com/truce-audio/truce).
