# truce-aax

AAX format wrapper for the truce audio plugin framework.

## Overview

Bridges a truce `PluginExport` implementation to the Avid AAX plugin API used
by Pro Tools. Instead of linking against the AAX SDK, the Rust code exports C
ABI functions that a pre-built AAX template binary loads at runtime via
`dlopen`. This means no AAX SDK dependency exists in the Rust source.

This crate is not depended on directly -- the `truce-xtask` build system
selects it automatically when bundling AAX plugins.

## What it handles

- C ABI export functions matching the AAX template's expected interface
- Audio processing and parameter bridge
- State save/restore
- Custom native NSView rendering on macOS (avoids Pro Tools autorelease pool
  crashes that occur with standard compositor-based approaches)

## Architecture

The pre-built AAX template binary implements the real AAX SDK interfaces and
loads the Rust dylib at runtime. The Rust side only knows about the C bridge
types defined in `truce_aax_bridge.h`, keeping the build free of proprietary
SDK dependencies.

Part of [truce](https://github.com/truce-audio/truce).
