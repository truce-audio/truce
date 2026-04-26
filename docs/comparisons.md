# Comparisons

Last updated 2026-04-24.

## Overview

| | truce | JUCE | nih-plug | iPlug2 | DPF |
|---|---|---|---|---|---|
| Language | Rust | C++ | Rust | C++ | C++ |
| License | MIT / Apache-2.0 | AGPLv3 / commercial | ISC | iPlug2 (BSD-like) | ISC |
| First release | 2026 | 2004 | 2022 | 2018 (rewrite of iPlug) | 2014 |
| Maturity | Early | Production-proven | Active | Active | Active |

## Plugin Format Support

| | truce | JUCE | nih-plug | iPlug2 | DPF |
|---|---|---|---|---|---|
| CLAP | Yes | Community fork | Yes | Yes | Yes |
| VST3 | Yes | Yes | Yes | Yes | Yes |
| VST2 | Yes | Deprecated | No | Yes | Yes |
| AU v2 | Yes | Yes | No | Yes | No |
| AU v3 | Yes | Yes | No | No | No |
| AAX | Yes | Yes | No | Yes | No |
| LV2 | Yes | No | No | No | Yes |
| LADSPA / DSSI | No | No | No | No | Yes |
| Standalone | Yes (cpal) | Yes | Yes | Yes | Yes (JACK) |
| Web Audio | No | No | No | Yes (Emscripten) | No |
| Formats total | 7 | 4 | 2 | 6 | 7 |

## GUI

| | truce | JUCE | nih-plug | iPlug2 | DPF |
|---|---|---|---|---|---|
| Built-in GUI | Yes (6 widget types) | Yes (comprehensive) | No | Yes (IGraphics) | Yes (DGL) |
| GPU rendering | wgpu (Metal/DX12/Vulkan) | OpenGL | N/A | Skia, NanoVG, or Canvas | OpenGL, Cairo, NanoVG, Vulkan |
| Layout system | Grid + row, declarative | Manual or XML | N/A | Manual | Manual |
| Framework integrations | egui, iced, slint | None (JUCE only) | iced, vizia, egui | None (IGraphics only) | None (DGL only) |
| Raw window handle | Yes | No | Yes | No | No |
| CSS theming | No | No | vizia only | No | No |
| Retina/HiDPI | Automatic (logical-point pipeline) | Automatic | Manual | Automatic | Manual |
| Hot-reload GUI | Yes | No | No | No | No |
| Screenshot tests | Yes (headless) | No | No | No | No |

## Parameters

| | truce | JUCE | nih-plug | iPlug2 | DPF |
|---|---|---|---|---|---|
| Declaration | `#[derive(Params)]` | `AudioProcessorParameter` subclass | `#[derive(Params)]` | `IParam` registration | `initParameter()` override |
| Smoothing | Built-in (`smooth = "exp(5)"`) | Manual | Built-in | Manual | Manual |
| Ranges | Declarative (`range = "linear(0, 1)"`) | `NormalisableRange` | Declarative | `IParam::InitDouble` | `ParameterRanges` struct |
| Enum params | `#[derive(ParamEnum)]` | `AudioParameterChoice` | `#[derive(Enum)]` | `IParam::InitEnum` | `kParameterIsEnumeration` hint |
| Thread safety | Atomic (lock-free) | Atomic | Atomic | Atomic | Key-value strings |
| State serialization | Automatic | Manual or `ValueTree` | Automatic | Automatic | Manual (key-value states) |
| Units/formatting | Declarative (`unit = "dB"`) | Manual | Manual | Manual | Manual |

## Developer Experience

| | truce | JUCE | nih-plug | iPlug2 | DPF |
|---|---|---|---|---|---|
| Scaffolding | `cargo truce new`, `cargo truce new-workspace` | Projucer / CMake | Manual | Duplicate example | Duplicate example |
| Build system | Cargo | CMake / Projucer | Cargo | CMake / VS / Xcode | Make / CMake |
| Hot reload | Yes | No | No | No | No |
| Plugin validation | `cargo truce validate` | Manual | Manual | Manual | Manual |
| Bundle/sign/install | `cargo truce install` | IDE export | `cargo xtask bundle` | IDE export | `make install` |
| Signed installer | `cargo truce package` (`.pkg` + Inno Setup `.exe`, Authenticode + PACE) | Manual (pkgbuild, Inno Setup) | Manual | Manual | Manual |
| Test framework | `truce-test` (render, state, GUI) | `juce::UnitTest` | Manual | None | None |

## Audio Processing

| | truce | JUCE | nih-plug | iPlug2 | DPF |
|---|---|---|---|---|---|
| Buffer access | Zero-copy from host | Copy or alias | Zero-copy from host | Copy | Copy |
| MIDI support | Event list (MIDI 1.0 + 2.0) | MidiBuffer (1.0) | Event list (1.0) | IMidiMsg (1.0) | Event list (1.0) |
| Sample-accurate events | Yes | Yes | Yes | Yes | Yes |
| Side-chain | Yes (bus layouts) | Yes | Yes | Yes | Yes |
| Transport info | Yes | Yes | Yes | Yes | Yes |
| Tail reporting | Yes | Yes | Yes | Yes | Yes |
| Latency reporting | Yes | Yes | Yes | Yes | Yes |

## Platform Support

| | truce | JUCE | nih-plug | iPlug2 | DPF |
|---|---|---|---|---|---|
| macOS | All 7 formats | Yes | CLAP + VST3 | Yes | CLAP / VST3 / VST2 / LV2 / standalone |
| Windows | CLAP / VST3 / VST2 / AAX / LV2 / standalone | Yes | CLAP + VST3 | Yes | CLAP / VST3 / VST2 / LV2 / standalone |
| Linux | CLAP / VST3 / VST2 / LV2 / standalone | Yes | CLAP + VST3 | Partial | Yes (primary target) |
| iOS/Android | No | Yes | No | Yes (iOS) | No |
| Web | No | No | No | Yes | No |

## When to Use What

**truce** — You want Rust, maximum commercial-format coverage (AU,
AAX, and LV2 in one codebase is unique to truce), hot
reload, and a batteries-included experience — built-in GUI, `cargo truce
install` / `package` / `validate`, and signed cross-platform
installers (`.pkg` + Inno Setup `.exe`). Best for new Rust projects
that need AU *and* AAX *and* LV2 alongside CLAP/VST3.

**JUCE** — You need a production-proven C++ framework with the
largest ecosystem, comprehensive GUI toolkit, and commercial support.
The industry standard for professional plugin development.

**nih-plug** — You want Rust with a minimal, flexible core and don't
need AU, AAX, VST2, or LV2. Mature Rust plugin framework with strong
community and proven production use.

**iPlug2** — You want C++ with broad format coverage including Web
Audio, and a self-contained graphics system. Good for cross-platform
projects targeting web alongside desktop.

**DPF** — You're writing open-source Linux-first plugins, want LV2
support with LADSPA/DSSI alongside for older hosts, and don't need
AU or AAX. Well-established in the FOSS audio ecosystem (Ardour,
LMMS, Zynthian, Mod Devices). C++ with a lean API and multiple
GUI backends (OpenGL, Cairo, NanoVG, Vulkan). Most DPF-based
plugins are Linux-primary but the framework runs on macOS and
Windows too.

## GUI Framework Comparison (truce-specific)

| Framework | Crate | Rendering | Paradigm | Widgets |
|-----------|-------|-----------|----------|---------|
| Built-in (CPU) | `truce-gui` | tiny-skia, logical-point contract | Layout-driven | Knob, slider, toggle, selector, dropdown, meter, XY pad |
| Built-in (GPU) | `truce-gpu` | wgpu (Metal/DX12/Vulkan/GL) | Layout-driven | Same widget set as CPU backend |
| egui | `truce-egui` | wgpu via baseview | Immediate mode | Knob, slider, toggle, selector, meter, XY pad |
| iced | `truce-iced` | wgpu (native NSView on macOS, baseview on Windows / Linux) | Elm architecture | Knob, slider, toggle, meter, XY pad + full iced widgets |
| Slint | `truce-slint` | Software renderer + blit (CGLayer on macOS, wgpu elsewhere) | Declarative `.slint` DSL | Knob, slider, toggle, selector, meter |
| Raw | `truce-core` | Bring your own | Any | BYO |

All truce backends share the same logical-point coordinate contract:
widget coordinates and event positions are in logical points; the
backend multiplies by the display scale factor internally for
physical-pixel rasterization. 

See [GUI backends](reference/gui/README.md) for detailed integration docs.
