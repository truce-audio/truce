# truce-au

Audio Unit v3 format wrapper for the truce audio plugin framework.

## Overview

Bridges a truce `PluginExport` implementation to Apple's Audio Unit v3 API.
Uses an Objective-C shim (compiled via `cc`) that implements an `AUAudioUnit`
subclass, with all plugin logic delegated to Rust through C FFI callbacks.

This crate only builds on macOS and is not typically depended on directly --
the `truce-xtask` build system selects it automatically when bundling AU
plugins.

## What it handles

- `AUAudioUnit` subclass registration
- Audio render block bridging
- Parameter tree construction from truce parameter metadata
- Factory presets and user preset state
- GUI view hosting via `AUViewController`
- Support for effects, instruments, and MIDI processor component types

## Architecture

The Objective-C shim owns the `AUAudioUnit` instance and calls into Rust for
processing, parameter access, and state management. AU type codes (`aufx`,
`aumu`, `aumi`) are derived from `truce.toml` metadata.

Part of [truce](https://github.com/truce-audio/truce).
