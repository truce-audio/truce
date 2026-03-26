# Comparisons

## Framework Comparison

| | truce | JUCE | nih-plug | iPlug2 |
|---|---|---|---|---|
| Language | Rust | C++ | Rust | C++ |
| License | MIT / Apache-2.0 | AGPLv3 / $40-175/mo | ISC | IPLUG2 (BSD-like) |
| CLAP | Yes | Community only | Yes | Yes |
| VST3 | Yes | Yes | Yes | Yes |
| AU | Yes (v2 + v3) | Yes | No | Yes (v2) |
| AAX | Yes | Yes | No | Yes |
| VST2 | Yes | Deprecated | No | Yes |
| Web Audio | No | No | No | Yes (via Emscripten) |
| Hot reload | Yes (`--features dev`) | No | No | No |
| Built-in GUI | Yes (6 widget types, GPU) | Yes (comprehensive) | BYO | Yes (IGraphics) |
| GUI frameworks | egui, iced, slint, raw | JUCE GUI only | iced, vizia, egui | IGraphics, Skia, NanoVG |
| Declarative params | `#[derive(Params)]` | Macros + classes | `#[derive(Params)]` | Manual registration |
| Formats total | 6 | 4 | 2 | 6 |
| Standalone | Yes (cpal) | Yes | No | Yes |
| Screenshot tests | Yes (headless GPU) | No | No | No |

## GUI Framework Comparison

| Framework | Crate | Rendering | Paradigm | Knobs/Meters |
|-----------|-------|-----------|----------|-------------|
| Built-in | `truce-gui` | wgpu (GPU) | Layout-driven | Yes (6 widgets) |
| egui | `truce-egui` | wgpu via baseview | Immediate mode | Yes (provided) |
| Iced | `truce-iced` | wgpu/Metal | Elm architecture | Yes (provided) |
| Slint | `truce-slint` | Software + wgpu blit | Declarative `.slint` DSL | Yes (provided) |
| Raw | `truce-core` | Bring your own | Any | BYO |

See [GUI backends](gui/README.md) for detailed integration docs.
