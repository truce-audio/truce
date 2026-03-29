# truce-vst3

VST3 format wrapper for the truce audio plugin framework.

## Overview

Bridges a truce `PluginExport` implementation to the VST3 plugin API. Uses a
C++ shim that implements the real VST3 COM interfaces with correct vtable
layout, ensuring binary compatibility with all VST3 hosts. All plugin logic is
delegated to Rust via C FFI callbacks.

This crate is activated by the `vst3` feature on the `truce` crate and is not
typically depended on directly.

## What it handles

- COM class factory and module entry point
- `IEditController` -- parameter editing and GUI
- `IAudioProcessor` -- audio processing callbacks
- `IPlugView` -- platform-native editor window embedding
- State persistence via `IBStream`
- Bus arrangement and channel layout negotiation

## Architecture

The C++ shim (compiled via `cc`) owns the COM objects and forwards every call
to Rust through a C FFI boundary. This avoids reimplementing COM vtables in
Rust while keeping all plugin logic in safe Rust code.

Part of [truce](https://github.com/truce-audio/truce).
