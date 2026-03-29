# truce-xtask

Build system core for truce audio plugins.

## Overview

Implements the core logic behind the `cargo truce` command. Handles
compilation, bundling into platform-specific formats, code signing,
installation to system plugin directories, and host validation for all
supported plugin formats (CLAP, VST3, VST2, AU, AAX).

On macOS, supports universal binary (arm64 + x86_64) compilation. On Windows,
handles DLL bundling and registry paths.

## Commands

| Command | Description |
|---------|-------------|
| `install` | Build, bundle, sign, and install to system plugin directories |
| `build` | Compile plugin binaries without bundling |
| `run` | Build and launch in standalone mode |
| `new` | Scaffold a new plugin project from template |
| `test` | Run plugin integration tests |
| `status` | Show installed plugin versions and paths |
| `clean` | Remove build artifacts |
| `nuke` | Remove installed plugins from system directories |
| `validate` | Run auval (AU) and pluginval (CLAP/VST3) |
| `doctor` | Check toolchain, SDKs, and signing certificates |
| `log` | Tail plugin log output |

## Architecture

This crate is not used directly. It is invoked through `cargo-truce`, which
provides the cargo subcommand interface.

Part of [truce](https://github.com/truce-audio/truce).
