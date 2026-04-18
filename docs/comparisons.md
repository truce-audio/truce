# Comparisons

## Overview

| | truce | JUCE | nih-plug | iPlug2 |
|---|---|---|---|---|
| Language | Rust | C++ | Rust | C++ |
| License | MIT / Apache-2.0 | AGPLv3 / commercial | ISC | iPlug2 (BSD-like) |
| First release | 2025 | 2004 | 2022 | 2018 (rewrite of iPlug) |
| Maturity | Early | Production-proven | Active | Active |

## Plugin Format Support

| | truce | JUCE | nih-plug | iPlug2 |
|---|---|---|---|---|
| CLAP | Yes | Community fork | Yes | Yes |
| VST3 | Yes | Yes | Yes | Yes |
| VST2 | Yes | Deprecated | No | Yes |
| AU v2 | Yes | Yes | No | Yes |
| AU v3 | Yes | Yes | No | No |
| AAX | Yes | Yes | No | Yes |
| Standalone | Yes (cpal) | Yes | No | Yes |
| Web Audio | No | No | No | Yes (Emscripten) |
| Formats total | 6 | 4 | 2 | 6 |

## GUI

| | truce | JUCE | nih-plug | iPlug2 |
|---|---|---|---|---|
| Built-in GUI | Yes (6 widget types) | Yes (comprehensive) | No | Yes (IGraphics) |
| GPU rendering | wgpu (Metal/DX12/Vulkan) | OpenGL | N/A | Skia, NanoVG, or Canvas |
| Layout system | Grid + row, declarative | Manual or XML | N/A | Manual |
| Framework integrations | egui, iced, slint | None (JUCE only) | iced, vizia, egui | None (IGraphics only) |
| Raw window handle | Yes | No | Yes | No |
| CSS theming | No | No | vizia only | No |
| Retina/HiDPI | Automatic | Automatic | Manual | Automatic |
| Hot-reload GUI | Yes (layout only) | No | No | No |
| Screenshot tests | Yes (headless) | No | No | No |

## Parameters

| | truce | JUCE | nih-plug | iPlug2 |
|---|---|---|---|---|
| Declaration | `#[derive(Params)]` | `AudioProcessorParameter` subclass | `#[derive(Params)]` | `IParam` registration |
| Smoothing | Built-in (`smooth = "exp(5)"`) | Manual | Built-in | Manual |
| Ranges | Declarative (`range = "linear(0, 1)"`) | `NormalisableRange` | Declarative | `IParam::InitDouble` |
| Enum params | `#[derive(ParamEnum)]` | `AudioParameterChoice` | `#[derive(Enum)]` | `IParam::InitEnum` |
| Thread safety | Atomic (lock-free) | Atomic | Atomic | Atomic |
| State serialization | Automatic | Manual or `ValueTree` | Automatic | Automatic |
| Units/formatting | Declarative (`unit = "dB"`) | Manual | Manual | Manual |

## Developer Experience

| | truce | JUCE | nih-plug | iPlug2 |
|---|---|---|---|---|
| Scaffolding | `cargo truce new` | Projucer / CMake | Manual | Duplicate example |
| Build system | Cargo | CMake / Projucer | Cargo | CMake / VS / Xcode |
| Hot reload | Yes (DSP + GUI layout) | No | No | No |
| Plugin validation | `cargo truce validate` | Manual | Manual | Manual |
| Bundle/sign/install | `cargo truce install` | IDE export | Manual | IDE export |
| Signed installer | `cargo truce package` (`.pkg` + Inno Setup `.exe` with Authenticode + PACE) | Manual (pkgbuild, Inno Setup) | Manual | Manual |
| Test framework | `truce-test` (render, state, GUI) | `juce::UnitTest` | Manual | None |

## Audio Processing

| | truce | JUCE | nih-plug | iPlug2 |
|---|---|---|---|---|
| Buffer access | Zero-copy from host | Copy or alias | Zero-copy from host | Copy |
| MIDI support | Event list | MidiBuffer | Event list | IMidiMsg |
| Sample-accurate events | Yes | Yes | Yes | Yes |
| Side-chain | Yes (bus layouts) | Yes | Yes | Yes |
| Transport info | Yes | Yes | Yes | Yes |
| Tail reporting | Yes | Yes | Yes | Yes |
| Latency reporting | Yes | Yes | Yes | Yes |

## Platform Support

| | truce | JUCE | nih-plug | iPlug2 |
|---|---|---|---|---|
| macOS | Yes (all formats) | Yes | Yes | Yes |
| Windows | Yes (CLAP/VST3/VST2/AAX) | Yes | Yes | Yes |
| Linux | Yes (CLAP/VST3) | Yes | Yes | Partial |
| iOS/Android | No | Yes | No | Yes (iOS) |
| Web | No | No | No | Yes |

## When to Use What

**truce** — You want Rust, maximum format coverage, hot reload, and a
batteries-included experience — including built-in GUI and `cargo truce
package` for signed cross-platform installers (`.pkg` + Inno Setup
`.exe`). Best for new Rust projects that need AU and AAX alongside
CLAP/VST3.

**JUCE** — You need a production-proven C++ framework with the largest
ecosystem, comprehensive GUI toolkit, and commercial support. The
industry standard for professional plugin development.

**nih-plug** — You want Rust with a minimal, flexible core and don't
need AU or AAX. Mature Rust plugin framework with strong community
and proven production use.

**iPlug2** — You want C++ with broad format coverage including Web Audio,
and a self-contained graphics system. Good for cross-platform projects
targeting web alongside desktop.

## GUI Framework Comparison (truce-specific)

| Framework | Crate | Rendering | Paradigm | Widgets |
|-----------|-------|-----------|----------|---------|
| Built-in | `truce-gui` | tiny-skia CPU or wgpu GPU | Layout-driven | Knob, slider, toggle, selector, meter, XY pad |
| egui | `truce-egui` | wgpu via baseview | Immediate mode | Knob, slider, toggle, selector, meter, XY pad |
| Iced | `truce-iced` | wgpu (native NSView on macOS, baseview on Windows) | Elm architecture | Knob, slider, toggle, meter, XY pad |
| Slint | `truce-slint` | Software renderer + CG blit on macOS, wgpu blit on Windows | Declarative `.slint` DSL | Knob, slider, toggle, selector, meter |
| Raw | `truce-core` | Bring your own | Any | BYO |

All GUI backends share consistent knob/meter visuals (pointer line,
hover ring, dB-scaled meters, blue fill with red clip indicator).

See [GUI backends](gui/README.md) for detailed integration docs.
