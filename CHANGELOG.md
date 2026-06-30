# Changelog

Notable changes per release.

## 1.0.2

- **iOS AU v3 builds pass App Store Connect signing validation.** The AU extension's embedded copy of the plugin framework was left unsigned by the submission certificate, so App Store upload rejected it; `cargo truce package --ios` now signs the appex's nested framework inside-out along with the container's copy.

## 1.0.1

- **`IcedPlugin::needs_redraw` keeps streaming editors live.** The iced editor's idle gate skips repaints when no UI input, param, or meter changed - which left a plugin streaming realtime data to its editor outside that system (a lock-free queue drained in `view()`) showing it late, only when a stray UI event forced a repaint. A plugin can now return `true` from `needs_redraw()` while it has new data, so the editor repaints and drains promptly and idles again when there's nothing new. `truce-example-midi-inspector` uses it so live MIDI scrolls in without lag.
- **`truce-example-envelope` emits sample-accurate CC.** The envelope follower pushed one control-change per block at `sample_offset: 0` carrying the block's final value; it now emits on each value change at the exact sample offset, so the CC stream stays smooth (no per-block `@0` bursts or up-to-a-block latency) and tracks level changes within a block.

## 1.0.0

### Breaking

- **Auto-assigned parameter IDs are now a stable hash of the field name** instead of a declaration-order counter. Reordering or inserting parameters - including inside `#[nested]` groups - no longer shifts later IDs, so host automation and presets keep mapping to the right parameter across plugin versions. Explicit `#[param(id = N)]` still wins. *Migration:* a plugin already shipped with the old order-based IDs must add `#[params(id_scheme = "ordinal")]` to keep them, or its hosts' saved automation and presets will rebind to the wrong parameters.
- **`truce-iced` no longer depends on the `iced` umbrella crate.** It uses iced's sub-crates directly (`iced_core` / `iced_widget` / `iced_renderer` / `iced_wgpu` / `iced_runtime` / `iced_futures`), re-exported as `truce_iced::iced`, which drops the transitive `iced_winit` dependency whose desktop-only keyboard API broke the iOS build. *Migration:* plugins that imported types from the `iced` crate import the same ones from `truce_iced::iced` instead.
- **Removed the deprecated `selector` / `param_selector` widget.** The `selector()` layout builder and each backend's `param_selector` (egui / iced / vizia / slint), deprecated since 0.56, are gone along with the underlying `Selector` widget. *Migration:* use `dropdown()` / `param_dropdown` - the same single-choice control, click-to-open.
- **Removed the deprecated `FloatParam::read_block`.** The const-`N` block read, deprecated since 0.53, is gone. *Migration:* use `read_into(&mut scratch[..n])`, which advances the smoother by exactly the number of samples consumed.

### Other changes

- **iced editors run on iOS.** `truce-iced` renders through a `CAMetalLayer`-backed `UIView` under AU v3 - touch input plus a soft keyboard for focused `text_input` - so iced plugins build and run on iOS like the egui backend. `gain-iced`, `gui-zoo-iced`, and `midi-inspector` are now iOS examples.
- **AU v3 factory presets ship on iOS.** `cargo truce install --ios` / `package --ios` now bundle a plugin's `presets/` library into the embedded framework's `Presets/`, so AU v3 hosts list them on iOS like macOS. iOS frameworks are shallow bundles, so the presets sit in a flat `Presets/` directory rather than `Resources/Presets/` (a `Resources/` subdir makes iOS installd reject the framework).
- **New `truce-example-midi-inspector` (iced).** An audio effect with MIDI in/out that passes the track's audio through untouched and decodes every event truce can deliver - MIDI 1.0 + 2.0 channel voice, SysEx (with manufacturer id + hex), transport, and param automation - into a live scrolling log (newest first), with a raw line for anything not yet richly interpreted. Forwards MIDI through (toggle with the `MIDI Thru` param) and demonstrates streaming *structured* realtime data from `process()` to an editor via a lock-free ring carried in a `#[skip]` params field.
- **`#[derive(Params)]` supports `#[skip]` fields.** A field marked `#[skip]` is not a parameter: it's plugin-owned state that the editor reaches through the `Arc<Params>` both sides already hold (e.g. a lock-free queue of audio-thread events). The derive `Default`-initializes it in `new()` and excludes it from parameter ids, infos, state, and count.
- **CLAP and LV2 state save/load are now panic-guarded** like VST3 / VST2 / AU / AAX: a panic in a user's `save_state` / `load_state` reports failure to the host instead of unwinding across the `extern "C"` boundary and aborting it.
- **Rendering performance fixes on Windows.** Embedded editors run their frame loop on the host's GUI thread, where the iced / egui backends repainted every tick and blocked on a vsync present - so a heavy editor (GUI Zoo) made the host (REAPER) laggy and could lock out other plugin windows. Editors now skip frames when nothing changed, present non-blocking on Windows, and skip rendering while the host window is hidden or minimized.
- **Smoother editor repaints on Windows.** Editors no longer render in slow, bursty spurts inside busy DAWs - the frame loop is now driven by a steady high-resolution timer (via the `baseview-truce` 0.1.1-truce.10 dependency), and the same fix closes a crash that heavy repainting could trigger. Applies to both the iced and egui backends.

## 0.64.0

- **Parameters can declare a default MIDI mapping** (`#[param(..., midi_cc = N)]`) - VST3, AU v2, and LV2 expose the binding to the host's MIDI controller assignment; CLAP / VST2 / AAX leave it host-driven.
- **VST3 accepts channel MIDI controller input** - CC, program change, channel pressure, pitch bend, and poly pressure now reach the plugin, decoded from VST3's legacy MIDI controller event forms. ([#145](https://github.com/truce-audio/truce/pull/145), by [@Boscop](https://github.com/Boscop))
- **VST3 bridges host-mapped MIDI controllers back into MIDI events.** A parameter bound via `#[param(midi_cc = N)]` / `#[param(midi_source = …)]` is the target a VST3 host routes the matching controller to (over `IMidiMapping`); that parameter change is now also delivered as the corresponding `EventBody` (pitch bend, CC, channel pressure, program change), so plugins reading MIDI events - not the parameter - respond to the pitch and mod wheels under VST3 as they already do on AU / CLAP.
- **The synth example responds to the pitch and mod wheels.** `truce-example-synth` now bends pitch (+/-2 semitones) and adds mod-wheel (CC1) vibrato.
- **SysEx input fixes.** VST2 and AAX no longer drop a queued input SysEx to the per-block event-list clear; AU now accepts SysEx input (v2 via `MusicDeviceSysEx`, v3 via UMP SysEx-7/8).
- **Plugin-process parameter changes notify the host.** A parameter the plugin changes during `process()` now updates the host's UI / automation on VST2, VST3, and AU (CLAP already did) - via `audioMasterAutomate`, the VST3 output parameter queue, and `AUEventListenerNotify` respectively. ([#147](https://github.com/truce-audio/truce/pull/147), VST2 by [@Boscop](https://github.com/Boscop))

## 0.63.3

- **`cargo truce --shell` finds `[profile.shell]` at the Cargo workspace root** - the preflight no longer rejects plugin crates that are members of a larger workspace. ([#148](https://github.com/truce-audio/truce/issues/148))
- **`truce-vizia` no longer risks messaging a freed parent `NSView`** on editor teardown - the per-frame macOS re-anchor stops once the editor closes.
- **`vizia` is now documented as not recommended for production** due to teardown/resize stability issues internal to vizia.

## 0.63.1

- **Slint editors receive keyboard input.** `truce-slint` translates native key events into Slint `WindowEvent` key events, so `LineEdit` / `TextInput` and `FocusScope` work in plugin editors (host focus permitting).
- **The slint `gui-zoo` example demos native Slint widgets** - button, checkbox, spinbox, slider, progress bar, image - plus a keyboard section, alongside the truce param widgets.
- **The vizia `gui-zoo` example demos native Vizia widgets** - button, checkbox, slider, image - plus a keyboard section, alongside the truce param widgets.

## 0.63.0

- **iced editors receive keyboard input.** `truce-iced` now forwards native key events into the iced widget tree (a focused `text_input` or a custom key-capture `Widget` reading `physical_key` / logical key), and `IcedPlugin::subscription()` (e.g. `iced::keyboard::listen`, `iced::event::listen_with`) now fires via an event pump. Keys arrive when the host grants the editor window focus. ([#134](https://github.com/truce-audio/truce/discussions/134))
- **`gui-zoo` examples (iced + egui) demo native framework widgets** - button, checkbox, radio, slider, progress bar, image, etc. - plus a keyboard section, alongside the truce param widgets.

## 0.62.0

- **AU v3 and standalone are one macOS app.** A plugin with a standalone bin ships a single `{name}.app` that is both the AU v3 container and the playable standalone host; the separate Standalone format collapses into it and the installer choice reads "AU3 + Standalone". Plugins without a standalone bin still ship the appex, in an informational stub app.
- **Fixed macOS standalone resize leaving a white margin around the editor;** non-resizable editor windows are now pinned so they can't be zoomed open.
- **`package --suite <name>` matches a suite by bundle id or display name** and errors on an unknown name instead of silently skipping the suite.

## 0.61.0

- **Reusable `#[nested]` param groups.** A nested `Params` struct's ids are rebased by a per-group base (auto-packed, or pinned with `#[nested(base = N)]`), so the same group type can be nested more than once without id clashes and nested groups need no per-param ids.
- **Nested meters are caught at construction.** Two `#[nested]` groups that each declare a meter would have aliased on one id; the derive now panics at construction instead of corrupting state silently.
- **Nested params construct through the generated `new()`.** A `Params` struct that mixes its own params with `#[nested]` groups now default-initializes the nested fields in `new()`. ([#137](https://github.com/truce-audio/truce/pull/137), by [@jedStevens](https://github.com/jedStevens))

## 0.60.1

- **`#[derive(Params)]` no longer trips a rust-analyzer error.** Parameters with a `range` (or numeric `default`) emitted suffixed literals like `60f64`, which rust-analyzer 1.96 rejected with "expected Expr" even though the code compiled; the derive now emits plain decimal literals. ([#135](https://github.com/truce-audio/truce/issues/135))

## 0.60.0

- **`midi_input` / `midi_output` capability flags.** New `[[plugin]]` truce.toml keys let an instrument or audio effect opt into MIDI input/output (or opt out), overriding the category default, consistently across every format.
- **AU MusicEffect for MIDI-driven effects.** An audio effect with `midi_input = true` now registers as an `aumf` MusicEffect, so AU hosts route MIDI to it instead of an `aufx`.
- **VST2 MIDI output fixed on 64-bit.** The outbound `VstEvents` block placed its pointer array at the wrong offset, so hosts read a garbage event pointer; plugins now emit MIDI correctly. ([#131](https://github.com/truce-audio/truce/issues/131))
- **VST3 emits non-note MIDI output.** Control change, pitch bend, aftertouch, channel pressure, and program change reach the host now, not just note on/off. ([#123](https://github.com/truce-audio/truce/issues/123))
- **CLAP advertises the MIDI note dialect.** Raw-MIDI output events (CC, pitch bend, SysEx) are no longer dropped by dialect-routing hosts.
- **SysEx payloads under sample-accurate chunking.** The chunking layer handed `process()` a timing-rebased event list whose SysEx pool was empty, so a plugin reading a SysEx payload would index out of bounds; the rebased list now carries its own copy of the payload.

## 0.59.0

- **Honor cargo's real target directory.** `cargo truce` now reads cargo's `target_directory` via `cargo metadata` instead of assuming `<plugin>/target`, fixing installs when the plugin crate is a member of a larger workspace (artifacts land in the workspace-root `target/`). ([#124](https://github.com/truce-audio/truce/issues/124))

## 0.58.4

- **Aspect-ratio editor resize fixes.** Aspect-locked editors stay on-ratio inside the host window without clipping or juddering across CLAP, VST3, AU, and LV2; corner drags track the ratio smoothly and LV2 honors the aspect ratio and `max_size`.
- **Skip rendering while the editor is occluded or detached (macOS).** A minimized or fully-covered window can't present, so every rendered frame queued a GPU drawable that never drained. Editors now bail before rendering when the host window is occluded or has no window attached, across every GUI backend (built-in CPU / GPU, egui, iced, slint). ([#126](https://github.com/truce-audio/truce/pull/126), by [@tothepoweroftom](https://github.com/tothepoweroftom))
- **`TRUCE_AZURE_ENDPOINT` for Windows code signing.** The Azure Trusted Signing endpoint was hardcoded to East US; set this to sign through another region's endpoint. Defaults to the previous value. ([#128](https://github.com/truce-audio/truce/pull/128), by [@tothepoweroftom](https://github.com/tothepoweroftom))
- **iced enum selector option count.** Fixed an off-by-one that left the iced enum selector (pick-list) with the wrong number of options. ([#129](https://github.com/truce-audio/truce/pull/129), by [@jedStevens](https://github.com/jedStevens))

## 0.58.3

- **Installers now ship your factory presets.** `cargo truce package` now carries presets for every format that has them, in both the per-plugin and suite installers, on macOS, Linux, and Windows: CLAP / AU / LV2 ride inside the bundle (sealed under the code signature), and VST3 presets are placed into the OS preset folder — merged in, so a user's own saved presets there are never wiped.

## 0.58.2

- **Standalone presets.** A Presets menu in the standalone host (macOS / Windows): pick a preset from Load, step through them with Previous / Next, and Save / Save As your own. Save (Cmd/Ctrl+S) updates the preset you're editing; Save As (Cmd/Ctrl+Shift+S) names a new one. On a factory preset Save is grayed and points you to Save As, so you never overwrite a stock sound by accident. Presets you save show up in the menu right away, and they land where your DAW reads them. Linux drives it all from the shortcuts. Launch straight into a sound with `--preset <name>`.
- **`.maximizable(true)` to allow window maximize in the standalone.** Resizable editors can no longer be maximized by default — maximizing would grow the window past the editor's `max_size` and leave an empty margin around the clamped GUI. Opt back in with `.maximizable(true)` (built-in grid, egui, iced, slint) for editors that render at any size; the underlying `Editor::can_maximize` hook drives it on Linux / macOS / Windows. When a window does end up bigger than the editor anyway, the GUI now sits centered instead of pinned to a corner, and the freed space is painted black rather than showing glitched leftovers.
- **LV2 presets now apply in REAPER.** REAPER listed presets but selecting it didn't change anything. LV2 presets now carry each parameter as a control-port value (`pset:value`), not only an opaque `state:state` blob, so port-based hosts apply them through the control ports — which updates the editor, the host's own parameter display, and the DSP together. State restore also gained a base64 fallback for hosts that hand back the preset's `xsd:base64Binary` literal undecoded (REAPER) instead of as a raw chunk.
- **Fixed-size LV2 editors no longer stretch in REAPER.** REAPER resizes an embedded LV2 X11 editor to fill its FX window, which bilinear-upscaled a non-resizable GUI (the built-in synth, etc.) into a blurry mess. The built-in editor now renders at its native size, centered, with black letterbox margins instead of stretching, and the wrapper pins the editor to its natural size at open. Resizable editors are unaffected.
- **VST3 factory presets rejected on Windows ("doesn't appear to be for this plugin").** Installed `.vstpreset` files stamped the plugin's class ID as a straight byte-for-byte hex dump. That matches how macOS / Linux hosts read it, but Windows hosts use the SDK's `COM_COMPATIBLE` `FUID::toString` ordering, which reads the first eight bytes as a little-endian GUID — so REAPER (and other Windows VST3 hosts) decoded a different class ID than the plugin reports and refused to load the preset. Emitted presets now match the host platform's ordering. Linux / macOS were unaffected.

## 0.58.1

- **Pin `time` to 0.3.47.** 0.3.48 was yanked from crates.io.

## 0.58.0

- **Presets.** Drop a `presets/` directory of `.preset` TOML files next to a plugin crate (params keyed by your `Params` struct's field names, resolved through the derive sidecars; numeric ids also accepted) and `cargo truce install` ships them as factory presets in every format: CLAP (native files + the preset-discovery / preset-load extensions), AU v2/v3 (factory list in Logic / GarageBand), VST3 (`.vstpreset` in the OS preset locations), LV2 (`pset:Preset` TTL). Every container wraps the same state envelope as session recall, which also powers the new `cargo truce preset` CLI: `convert` between any two formats, `pull` to turn presets saved in your DAW into library files (uuid-stable updates, param-name comments), `import` / `export` for pack zips, and `list` / `init`. Underneath, `truce_core::presets::PresetStore` manages factory / user / pack scopes with uuid identity, so renames never break host references and user copies override factory ones. `[plugin.presets]` `user_dir` optionally replaces the `truce/<vendor>/<plugin>` part of the user-scope path.
- **`cargo truce install --au2 --debug` installed a stale release dylib.** The AU installer now resolves the build profile like every other format.

## 0.57.2

- **CLAP editor clipped above the host's pane in REAPER.** The layout's top edge (header / first row) was drifting off the visible plug-in pane as the canvas grew. Editors now stay pinned to the top of the pane at every size. Applies to every GUI backend (built-in, egui, iced, slint, vizia).
- **LV2 editor opened at host's default size instead of natural.** Resizable LV2 plug-ins (GUI Zoo, etc.) used to take whatever pane size the host defaulted to. The wrapper now hints the editor's preferred size on open; hosts that honor it (REAPER, Ardour) start the pane at natural. User-drag resize from the host's window still works.
- **GPU editors use the device's actual wgpu limits.** Every wgpu-backed renderer (built-in GPU, egui, iced, slint) now requests `adapter.limits()` instead of `wgpu::Limits::downlevel_defaults()`. The downlevel cap of 2048 px per axis was clipping tall Retina canvases and crashing the host; modern GPUs report 8192+ and the layout renders at full size instead.
- **AU class-name collision warnings cleaned up.** Multiple installed Truce AU plug-ins no longer log `TruceAuFixedContainer is implemented in both ...` on host launch.

## 0.57.1

- **AU v2 editor clipped in Ableton (macOS).** Ableton was embedding the AU v2 view at a frame smaller than the editor's natural size, which left the editor's top edge (the title header and knob row) clipped off the visible plug-in window. The AU v2 shim now pins the container to the editor's `gui_get_size` regardless of what the host requests, so the full editor is visible in every host.
- **AU v3 resize wired up on macOS.** Logic Pro's host-driven resize now propagates to the editor (was a no-op before). The editor's first layout pass stays at natural size so opening the plug-in doesn't snap to Logic's full pane; subsequent layouts (real user resize) reflow the editor.
- **Host crash on quit (REAPER, Cubase).** A Rust panic in the editor's teardown chain (wgpu surface drop, `NSView` close) previously propagated across the AU FFI boundary as an unhandled Obj-C exception and aborted the host on quit. The AU v2 `destroy` / `gui_close` callbacks now swallow panics at the boundary - host stays alive through quit.
- **Cubase crash with built-in editor.** A pixmap allocation failure in the built-in `GridLayout` editor was panicking inside `render`, which surfaced as a host crash in Cubase. The allocation path now logs and skips the frame instead; the next frame retries.

## 0.57.0

- **Resizable editors.** Every GUI backend opts in with `.resizable(true).min_size((a, b)).max_size((a, b))` on the editor / layout, and the CLAP, VST3, AU, and LV2 wrappers round-trip host requests to it. Logical points everywhere except the built-in grid, which takes `(cols, rows)` cell counts (it snaps to whole cells anyway). Vizia plugin-form `set_size` is a known gap.
- **New `Editor` trait methods.** `size_increment() -> Option<(u32, u32)>` (interactive-resize granularity in logical points; the X11 standalone maps it onto `PResizeInc` so window-edge drags snap), `aspect_ratio() -> Option<(u32, u32)>` (`(num, denom)`; CLAP / VST3 / AU v3 / standalone honour it, VST2 / LV2 / AAX silently ignore), and `prefers_pow2() -> bool` (renderer wants power-of-two surface sizes). All have defaults so existing plugins compile unchanged. Matching builder methods on every editor type; iOS variants are no-op stubs.
- **`truce-vizia`: XY pad label moved to bottom.** Matches the other backends.
- **`truce-iced` widgets: `meter` and `xy_pad` accept `.width(Length)`, `.height(Length)`, `.fill()`.** Lets the widget stretch with the parent column / row instead of using a fixed pixel size.
- **`truce-vizia` widgets: `level_meter` height and `xy_pad` `w` / `h` now take `impl Into<Units>`.** Pass `Pixels(n)` for a fixed size, `Stretch(1.0)` to fill the parent layout. Breaking for callers passing `f32` directly: wrap in `Pixels(_)`.
- **`truce-slint` widgets: `Meter` and `XYPad` use `preferred-*` + `min-*` / `max-*` sizing.** A parent `VerticalLayout` / `HorizontalLayout` can stretch them; the floor sizes keep the widgets usable at the editor's `min_size`.
- **`baseview-truce 0.1.1-truce.8`.** Adds the macOS `setFrameSize:` `Resized` event + OpenGL drawable resize that host-driven editor resize depends on. To upstream to baseview.
- **Windows: wgpu editors pin the DX12 shader compiler to FXC.** Fixes a blank editor (egui, built-in `GpuEditor`) and a host crash (Slint) in Pro Tools. wgpu 29 defaults DX12 to a dynamically-loaded **DXC** (`dxcompiler.dll`); when the host process has already loaded its own incompatible `dxcompiler.dll` (Pro Tools does), `DxcCreateInstance` returns `E_NOINTERFACE` and the *entire* DX12 backend fails to initialise - the wgpu instance ends up with zero adapters, so surface creation fails (white editor, or a panic on Slint's `.expect`). The `truce-gpu`, `truce-gui`, `truce-egui`, and `truce-slint` editor instances now request `Dx12Compiler::Fxc` (`d3dcompiler_47.dll`, always present on Windows, never conflicts). iced (wgpu 0.19, FXC by default) and vizia (OpenGL) were never affected. The Windows editor instances also narrow to `Backends::DX12` (the only backend feature compiled in on Windows). Offscreen/headless screenshot paths are unchanged.
- **AAX: fix Pro Tools scan hang on Windows.** AAX registration is now lazy - triggered by the first `truce_aax_get_descriptor` query - instead of running from a `.CRT$XCU` library-load static initializer. On Windows the AAX shim `LoadLibraryA`s the cdylib from inside `GetEffectDescriptions` during scanning, so the old initializer ran plugin construction (via `has_editor_static`'s default `create().editor()`, which can touch user32 in editor setup) *under the Windows loader lock* and hung the scan. Now mirrors how VST3/CLAP register lazily on their first host entry point. No API change.


## 0.56.0

- **`truce-vizia`: `param_knob` layout matches the other backends.** Reordered the cell as knob → value → name; previously the name label sat between the knob and the value, which inverted the cross-backend convention.
- **`selector` / `param_selector` widgets deprecated.** Marked `#[deprecated(since = "0.56.0")]` across `truce-gui-types`, `truce-egui`, `truce-iced`, and `truce-vizia`; the slint `Selector` element gets the same notice in its widget library header (slint markup has no attribute equivalent). Use the corresponding `dropdown` / `param_dropdown` / `Dropdown` instead.

## 0.55.0

- **AU v3: sample-accurate parameter automation.** The Swift shim now decodes `AURenderEvent.parameter` / `.parameterRamp` into per-sample `ParamChange` events with the proper within-block offset, and the chunker subdivides the audio block at each automation point. AU v2 stays block-rate (its `AudioUnitSetParameter` API carries no sample-offset).
- **LV2: sample-accurate parameter automation.** The TTL now advertises each parameter as a `patch:writable` `lv2:Parameter`, and the wrapper decodes host-emitted `patch:Set` Objects from the input atom sequence into per-sample `ParamChange` events (the atom event's `time_frames` becomes the within-block `sample_offset`). The legacy `lv2:ControlPort` path stays so older LV2 hosts still update params at block rate.

## 0.54.0

- **New `vst3_subcategory` `truce.toml` key.** Emits the secondary VST3 "Plugin Type Categories" token (`Fx|Delay`, `Fx|Reverb`, `Instrument|Synth`, …). Without it, Cubase buckets the plugin under "Other". Optional; opt-in per plugin.

## 0.53.0

- **New `FloatParam::read_into(&mut [f32])` smoother API.** Slice-based block read; advances the smoother by exactly `out.len()`. Same one-atomic-pair amortization as `read_block`, runtime length.
- **Deprecated `FloatParam::read_block::<N>()`.** Always advanced by `N` regardless of consumed samples, silently stepping the smoothed value at the next block boundary whenever the host's block size wasn't a multiple of `N`. Audible as clicks on delay / LFO-rate / any timing-sensitive smoothed param. `read_into(&mut scratch[..n])` is the same code shape with the hazard removed.
- **New `truce_simd::math64` module.** f64 mirror of `truce_simd::math` (`db_to_linear_block`, `linear_to_db_block`, `exp2_block`, `log2_block`, `tanh_block`). `wide::f64x4` lanes.
- **`eq` example uses SIMD math64.** Output stage dB → linear runs through `math64::db_to_linear_block` instead of scalar `f64::powf` per sample.
- **Examples migrated** (`block-gain`, `block-saturate`, `eq`).

## 0.52.0

- **New GUI backend: `truce-vizia`.** Param-bound widgets, headless screenshot. Desktop only (no iOS, no Windows ARM64).
- **New examples: `truce-example-gain-vizia`, `truce-example-gui-zoo-vizia`.**
- **Sample-accurate parameter automation.** Param changes apply at their `sample_offset` instead of the start of the block; smoothers start ramping at the event sample. On by default. Tune via `[automation] min_subblock_samples` in `truce.toml` or opt out per-param with `#[param(chunk = false)]`.
- **`truce-iced`: `with_font(bytes)` matches egui / vizia.** Family name is now read from the TTF (was `with_font(family, bytes)`).
- **`cargo truce install` / `package`: dedupes duplicate archive members during macOS bundle link.** Fixes the `clang -bundle` duplicate-symbol failure plugins with `skia-bindings` (vizia) could hit.

## 0.49.23

- **`keyboard-types` 0.6 -> 0.7** workspace-wide.

## 0.49.22

- **New example: `truce-example-dasp-bitcrusher`.** Showcases `dasp_sample::Sample` bit-depth round-tripping for 8-bit / 16-bit quantization, with a sample-and-hold downsampler.
- **`truce-example-gui-zoo-iced`: added a Dropdown section** exercising the `param_dropdown` alias added in 0.49.21.

## 0.49.21

- **`truce-egui`: int sliders now snap and show plain integers.** `param_slider` adapts to `ParamRange::Discrete` (plain `min..=max`, integer step, integer label) instead of the normalized [0, 1] display.
- **`truce-egui`: multi-channel meters render past 2 bars.** The widget allocated a fixed 16 px width regardless of channel count, so bars beyond ~3 were clipped. Width now grows with channel count.
- **`truce-egui`: toggle height matches the selector** so a row mixing them bottom-anchors labels on the same baseline.
- **`truce-iced`: `param_dropdown` alias added** for `param_selector`, matching the egui / CPU APIs.
- **`truce-slint`: new `Dropdown` widget** (popup-style, wraps the std-widgets `ComboBox`). Available via `import { Dropdown } from "@truce"`.
- **`truce-slint`: pinned std-widgets style to `fluent` on every host OS.** The default picked Cupertino on macOS, whose `ComboBox` rendered its chevron region with a persistent accent-blue square.
- **`truce-slint`: `Toggle` height bumped 40 → 50** to align labels with `Selector` / `Dropdown` in mixed rows.
- **New examples: `truce-example-gui-zoo-egui` / `-iced` / `-slint`.** Each mirrors the CPU `gui-zoo`'s param set so every renderer's widget surface is exercised against the same shape.

## 0.49.20

- **GUI: discrete params now snap during drag and wheel.** `IntParam` / `EnumParam` slider, knob, and XY-pad drags previously emitted continuous normalized values; storage snapped on writeback but the in-flight edit left UI and audio reads briefly out of phase. `ParamSnapshot::snap_normalized` now snaps at emit time.
- **GUI: dropdown A→B switch now repaints on the CPU renderer.** The editor's repaint gate diffs `dropdown_is_open()` which stays `true` across a close-then-open inside one click; `dropdown_close` and `open_dropdown` now flag the dirty bit explicitly.
- **Layout: labeled sections now start strictly below the previous section's tallest widget.** Section breaks advanced `cursor_row` by 1, which packed the next section alongside a tall (`rows = N`) widget from the prior section. Now advances past `max_occupied_row`.
- **New `deg` unit.** `#[param(unit = "deg")]` (or `"°"`) prints e.g. `180.0°`. `ParamUnit::Degrees` slots into the existing variant set.
- **New example: `truce-example-gui-zoo`.** Passthrough plugin that exercises every built-in widget kind across mixed spans and positions, every `ParamUnit` variant, and the discrete-snap path. Lives in `examples/` and is wired into desktop + iOS screenshot CI.

## 0.49.19

- **`#[param(default = std::f64::consts::*)]` no longer trips `clippy::approx_constant` in the macro expansion.** The 0.49.18 parser resolved the path to its `f64` value but `quote!` re-emitted it as a literal. The derive now embeds the original path tokens verbatim while keeping the resolved `f64` for the compile-time range / shape checks.

## 0.49.18

- **`#[param(default = ...)]` now accepts `std::f64::consts::*`.** Also `core::f64::consts::*` and bare `f64::consts::*`. Lets plugins write `default = std::f64::consts::SQRT_2` instead of a literal.

## 0.49.17

- **`truce-gui`: fixed dropdown menu dirty-tracking bugs.** Scroll wheel, touch-drag scroll, and per-option hover highlights mutated popup state without flagging the repaint gate, so on the CPU renderer they only became visible when an unrelated event tripped a repaint. (GPU renderer was unaffected: it re-renders every frame.)

## 0.49.16

- **macOS packaging: CLAP/VST3/VST2 bundles are now ~half the size.** The bundle-bin link path inherited `-all_load` without a matching `-dead_strip`, so every staticlib object survived; AU/AAX cdylib links got `-dead_strip` for free via rustc. Added `-dead_strip` to the `clang -bundle` step.

## 0.49.15

- **`truce-gui`: fixed a dropdown popup positioning bug.** The popup no longer mis-anchors relative to its trigger widget.

## 0.49.14

- **Standalone: added MIDI device + channel selection** via a unified Settings menu on macOS/Windows, or `--midi-input` / `--midi-channel` (`omni` or `1`-`16`) on Linux.

## 0.49.13

- **VST2/AAX: extended the 0.49.9 `set_state` fix.** The same path that dropped GUI-edited custom state in CLAP/VST3/AU was present in VST2 and AAX; they now route editor bytes to `load_state` like the others. (LV2 is unaffected: its UI is out-of-process and never touches `set_state` directly.)

## 0.49.12

- **`truce-slint`: fixed a panic on HiDPI displays after a resize event.** The render buffer kept the pre-resize physical extents while slint's window adopted the new ones, so the next frame tripped slint's buffer-too-small check.

## 0.49.11

- **Standalone: fixed the macOS device menu** — the Input/Output Device submenus were grayed out and unopenable.
- **Standalone: added input/output channel selection** (mono channel or stereo pair) via the macOS/Windows menus or `--input-channels` / `--output-channels` (the CLI is the picker on Linux).

## 0.49.10

- **Fixed a use-after-free crash when a host closes the editor without calling `close()`** (seen in Ableton with several plugins loaded). The macOS frame timer kept firing against the freed editor; every editor backend now cancels its window on drop.
- **Fixed meters not updating in the built-in (CPU) editor.** The repaint gate only watched parameter changes, so a moving meter stayed frozen until an unrelated repaint (e.g. dragging a knob) fired; the editor now repaints when a meter value moves.

## 0.49.9

- **Fixed `#[derive(State)]` custom state not persisting when edited from the GUI.** The CLAP, VST3, and AU editor `set_state` paths silently dropped GUI edits; they now reach `load_state` correctly.

## 0.49.6

- **`truce-standalone`: the `gui` feature no longer pulls `truce-gpu`.** Standalone builds compile the GPU backend only when the plugin opts in via `truce-gui = { …, features = ["gpu"] }`.

## 0.49.4

- **Fixed a macOS stack-overflow crash in the built-in (CPU) GUI / standalone path.** Moving the cursor over a freshly-opened editor window aborted the process. Fixed in `baseview-truce 0.1.1-truce.4`.

## 0.49.2

- Housekeeping: minor README updates and safety fixes.
- Standalone: Disable window resizing on Windows for now (mirrors other OSes).
- Standalone: Wire `windows_icon` through to the window's `WM_SETICON` so the title bar and taskbar show the app icon.

## 0.49.0

- **Breaking (`PluginLogic`): the GUI surface collapses to a single `editor()` method.** The old `layout()`, `custom_editor()`, `render()`, `uses_custom_render()`, and `hit_test()` methods are gone — every plugin now returns its editor from one place:
 ```rust fn editor(&self) -> Box<dyn Editor> { /* ... */ } ```
 `editor()` is required (there is no headless auto-fallback). Migration steps below.
- **Renderer split: the built-in GUI now defaults to CPU (tiny-skia); wgpu is opt-in.** The CPU rasterizer moved into a new `truce-cpu` crate, a peer of `truce-gpu`. A layout-only plugin no longer compiles wgpu and its per-OS graphics backends unless it asks for them — smaller binaries and faster builds out of the box. Opt into GPU rendering with `truce-gui = { version = "0.49", features = ["gpu"] }`; the `gpu` path doesn't pull the CPU dependency tree, and vice-versa. Plain `truce-gui = "0.49"` keeps the CPU default with no change.
- The `truce` umbrella no longer pulls `truce-gui` transitively. Plugins using the built-in renderer declare `truce-gui` explicitly — newly scaffolded projects (`cargo truce new`) already do.

Both paths end with `.into_editor(...)` — a fluent terminal that
replaces the old `Box::new(...)` wrapper and keeps every `editor()`
impl looking the same.

### Migrating `layout()`

Move your `GridLayout` body into `editor()` and close it with
`.into_editor(&self.params)`:

```diff
-    fn layout(&self) -> GridLayout {
-        GridLayout::build(vec![widgets(vec![
-            knob(P::Gain, "Gain"),
-            knob(P::Pan, "Pan"),
-        ])])
-        .with_title("GAIN")
-    }
+    fn editor(&self) -> Box<dyn Editor> {
+        GridLayout::build(vec![widgets(vec![
+            knob(P::Gain, "Gain"),
+            knob(P::Pan, "Pan"),
+        ])])
+        .with_title("GAIN")
+        .into_editor(&self.params)
+    }
```

`.into_editor(&params)` comes from the `truce_gui::IntoLayoutEditor`
trait — add `use truce_gui::IntoLayoutEditor;` to your imports. It
picks the renderer from the `truce-gui` feature you enabled (`cpu`
by default, `gpu` if opted in), so the same `editor()` body covers
both. If your plugin previously depended only on `truce-gui-types`,
add `truce-gui`:

```diff
 [dependencies]
 truce-gui-types = { version = "0.49" }
+truce-gui       = { version = "0.49" }   # add features = ["gpu"] for wgpu
```

(`truce_gui::default_editor(params, layout)` is the equivalent
free function if you'd rather not import the trait.)

### Migrating `custom_editor()` (egui / iced / slint / hand-rolled)

Rename `custom_editor` to `editor` and return the `Box<dyn Editor>`
directly. The new method is non-optional, so drop the `Some(...)`
wrapper and finish the builder chain with `.into_editor()`:

```diff
-    fn custom_editor(&self) -> Option<Box<dyn Editor>> {
-        Some(Box::new(
-            EguiEditor::new(self.params.clone(), (W, H), gain_ui)
-                .with_visuals(truce_egui::theme::dark()),
-        ))
-    }
+    fn editor(&self) -> Box<dyn Editor> {
+        EguiEditor::new(self.params.clone(), (W, H), gain_ui)
+            .with_visuals(truce_egui::theme::dark())
+            .into_editor()
+    }
```

The zero-arg `.into_editor()` is the blanket `truce_core::IntoEditor`
helper — it's in `truce::prelude`, so no extra import. The egui /
iced / slint editor constructors are otherwise unchanged. These
backends supply their own renderer, so they don't need `truce-gui`'s
`cpu` / `gpu` features — a plugin using one of them can drop the
`truce-gui` dependency entirely.

## 0.48.11

- `cargo-truce`: Fix various install-path bugs on Windows. VST3 now installs to system scope by default — Ableton doesn't scan the per-user VST3 directory.
- `truce-gui` & `truce-gpu`: Minor built-in GUI rendering improvements.

## 0.48.10

- Minor housekeeping.
- `cargo-truce`: Replace non-Latin status glyphs in `cargo truce package` output with the same `[ OK ]` / `[FAIL]` ASCII tags used by `cargo truce doctor`. The Unicode `✓` / `✗` characters broke rendering in Windows 10 WSL terminals.

## 0.48.9

- Examples: Fix blocksize bug in EQ example.
- AAX: Set `UNSAFE_OBJC2_ALLOW_CLASS_OVERRIDE` when building AAX to prevent same-class-name crashes when multiple AAX plugins load into the same host process (e.g. Pro Tools loading two truce plugins). Details: <https://github.com/rust-windowing/raw-window-metal/issues/29>
- `cargo-truce`: Make `--ios` packaging behavior and naming scheme consistent with the other formats / OSes — iterates every plugin in the workspace (no longer errors when more than one is declared) and writes the artifact to `target/dist/<crate>-<version>-ios.ipa` next to the macOS `.pkg` / Windows `.exe` / Linux `.tar.gz`.

## 0.48.8

- **truce now fully published to crates.io.** `cargo truce new` scaffolds with a `truce = { version = "0.48" }` registry pin by default; the historical `git = "...", tag = "v0.48.7"` form stays available via `cargo truce new --github` for scaffolding against an unreleased checkout.
- New `truce-aax-bridge` crate carries the C ABI header so `cargo-truce` doesn't transitively pull the full `truce-aax` runtime stack.

### Migrating an existing plugin to the registry pin

In your plugin's `Cargo.toml`, replace each
`truce-* = { git = "https://github.com/truce-audio/truce", tag = "v0.48.7"[, ...] }`
line with `truce-* = { version = "0.48"[, ...] }`. Concretely:

```diff
 [dependencies]
-truce         = { git = "https://github.com/truce-audio/truce", tag = "v0.48.7" }
-truce-gui     = { git = "https://github.com/truce-audio/truce", tag = "v0.48.7" }
-truce-clap    = { git = "https://github.com/truce-audio/truce", tag = "v0.48.7", optional = true }
-truce-vst3    = { git = "https://github.com/truce-audio/truce", tag = "v0.48.7", optional = true }
-truce-standalone = { git = "https://github.com/truce-audio/truce", tag = "v0.48.7", features = ["gui"], optional = true }
+truce         = { version = "0.48" }
+truce-gui     = { version = "0.48" }
+truce-clap    = { version = "0.48", optional = true }
+truce-vst3    = { version = "0.48", optional = true }
+truce-standalone = { version = "0.48", features = ["gui"], optional = true }
```

Preserve any `optional = true` / `features = [...]` keys that
were already on the line. Workspace-mode plugin Cargo.tomls
(`truce-* = { workspace = true }`) need no change — only the
workspace root `[workspace.dependencies]` block flips. Then:

```
cargo update -p truce
cargo build
```

Cargo's caret resolver expands `"0.48"` to `>=0.48.0, <0.49.0`,
so you'll pick up any future `0.48.x` patch release without
re-editing.

## 0.48.4

- Standalone: Fix default device selection on Linux (broke after cpal 0.17 update in 0.48.x).

## 0.48.3

- `cargo-truce`: Improve iOS screenshot reliability, clear any stale `_truce_editor_frame.json` before launch, extend the first-paint poll timeout for cold CI runners, and hard-fail with a diagnostic when the editor never renders (instead of silently proceeding to a misleading "screenshot size mismatch").

## 0.48.2

- `truce-egui`: Migrate to egui 0.34 (bumps `wgpu` to 29 transitively).
- **Breaking (`truce-egui`):** `EditorUi::ui` and the `EguiEditor::new` closure now receive `&mut egui::Ui` instead of `&egui::Context` egui 0.34 deprecates `Context::run` plus the per-panel `show(ctx, …)` entry points in favor of `Context::run_ui` and `show_inside(ui, …)`.
- Bump MSRV to 1.92.
- For egui-0.33 parity with `nih-plug`, pin truce to `0.47.0`.

(0.48.0 and 0.48.1 were yanked, install 0.48.2.)

## 0.47.0

- `truce-egui`: Migrate to egui 0.33 (bumps `wgpu` to 27 transitively).
- `truce-gpu`: Declare wgpu graphics backends per-OS (fixes Linux).

## 0.46.0

- `truce-iced`: Migrate to iced 0.14.
- `truce-egui`: Add `param_dropdown` widget (stock click-to-open dropdown wrapping `egui::ComboBox`).
- `truce-egui`: `param_knob` snaps to discrete steps on enum / discrete params.
- Examples: Tremolo refreshed (compact transport line, fractional rate labels, dropdown polish).

## 0.45.4

- LV2: Fix install path on Windows.
- LV2: Fix param defaults (mirror other formats).
- Standalone: Disable window resizing on Linux (mirror other OSes).
- Examples: Fix `fourcc` collision between two example plugins.

## 0.45.3

- `cargo-truce`: Fix plugin-name to path sanitization mismatch between `build` / `install` and `package` (display names with filesystem-reserved characters like `Truce Dry/Wet` produced a ~15 KB empty installer because `productbuild`'s distribution.xml referenced the raw name while the on-disk `.pkg` files used the sanitized name).

## 0.45.2

- AAX: Fix ABI mismatch bug (broken since 0.43.0).
- AAX / `cargo-truce`: Add Pro Tools `pluginrunner` to `cargo truce validate` (used if present).
- LV2: Fix editor positioning quirks (some quirks remain but no showstoppers).
- `cargo-truce`: Update help with `--target-cpu`.
- `cargo-truce`: Thread `--target-cpu` args through `install`, `package`, and `run` commands (with sane defaults).
- `cargo-truce`: `status` no longer runs auval (too slow for the purpose); use `validate` for that.
- Stylistic sweep.

## 0.45.0

- New CI gate exercises every prior release's example crates against the current `truce` HEAD, catching backwards-incompatible changes before they ship.
- **Initial SIMD block operations.** New `truce-simd` crate ships `wide`-backed `scale_block` / `mul_block` / `mix_block` / `mac_block` / `copy_block` / `zero_block` plus `tanh_block` / `db_to_linear_block` / `linear_to_db_block` math helpers, with scalar fallbacks. Six new examples (`gain-simd`, `saturate`, `drywet`, `gate`, `widen`, `surround-meter`) demonstrate the shapes.
- **`cargo truce build` now defaults x86_64 builds to `-C target-cpu=x86-64-v3`** (AVX2 + FMA + BMI2) so the SIMD paths above activate without any per-developer config. New `--target-cpu <value>` flag accepts `baseline` (rustc default = SSE2-only), `v2` / `v3` / `v4`, `native` for the local-CPU dev-loop, or any literal rustc target-cpu name.
- Plugin display names that contain filesystem-reserved characters (e.g. `Truce Dry/Wet`) are now sanitized at the path-construction boundary, so the on-disk bundle lands at `Truce Dry-Wet.aaxplugin` while DAWs still display the raw name from the plist.

## 0.44.0

- **VST3 + CLAP on macOS now link as `MH_BUNDLE` instead of `MH_DYLIB`.** Fixes load under hosts that take the strict `CFBundleLoadExecutable` path (DawDreamer's JUCE-based VST3 host is the one we validated against). Most desktop DAWs have more forgiving loaders and weren't affected, but the strict path is the correct Mach-O shape for a bundle. Built from a Rust `staticlib` via `clang -bundle`. **Breaking change for pre-0.44.0 plugins:** the plugin crate's `[lib]` block needs `crate-type = ["cdylib", "staticlib", "rlib"]` (was `["cdylib", "rlib"]`). `cargo truce install` / `package` fails loudly with the exact one-line edit if the staticlib is missing.
- AU v3: Fix installs broken since 0.42.0.
- AU v2: Fix `PresentPreset` handler broken since 0.40, auval passes again across all bundled examples.
- `cargo truce package --formats <list>` now works on Linux, matching the existing macOS / Windows behavior. Internally drives the underlying `cargo truce build` invocation.
- CI hardening: every `cargo truce` subcommand (install, validate, package, uninstall, doctor, status, reset-au) now runs on macOS, Linux, and Windows on every PR. New scaffold-and-round-trip workflow exercises `cargo truce new` against single-effect, single-instrument, and mixed-workspace shapes.
- Doc sweep across the in-tree comments and rustdoc.

## 0.43.0

- **SysEx + UMP support (work in progress).** Initial plumbing for System Exclusive messages and MIDI 2.0 UMPs

## 0.42.1

- Params: `IntParam` value displays no longer pick up the `FloatParam` `{:.1}` / `{:.2}` formatters, so transpose's semitone knob now reads `0 st` instead of `0.0 st`. Internally, `ParamInfo` gained a `kind: ParamValueKind` field set by `#[derive(Params)]` from the field type.
- Example tidy: the `Mix` knobs on both fundsp reverbs and the `Depth` knob on tremolo now declare `unit = "%"`, so they render as `25%` / `0%` instead of `0.25` / `0.00`.

## 0.42.0

- **iOS support.** AU v3 plug-ins now build, install, and run on both the iOS Simulator (`cargo truce install --ios`) and tethered devices (`cargo truce install --ios-device`). Truce ships a Swift container app template with embedded editor, Play button, status label, info sheet, and a hamburger-menu landscape layout. New `[[plugin]]` knobs in `truce.toml`: `ios_icon_set`, `ios_orientations`, `ios_scale_editor_to_fit` (default `true`), `ios_minimum_os_version`, `ios_app_group`, `ios_url`. Touch input is pinned per-finger so multi-touch doesn't hijack an in-progress drag. `mute_preview_output` works on both standalone and the iOS container for analyzer-style plug-ins. Custom container apps and iced's iOS backend remain unsupported (latter blocked upstream). See the new [iOS chapter](/docs/guide/ios).
- iOS screenshot regression: `cargo truce screenshot --ios` captures the simulator's actual rendered output (the only path that sees iOS-specific compositing); `--check` gates baselines in CI.

## 0.41.0

- AAX: Fix knobs sync bug on log-ranged parameters. The C++ shim defaulted to a linear taper for every param's normalize / denormalize, so AAX would round-trip a log-ranged knob through `RenderAudio` into a different plain value than the editor wrote. Wire `range_type` through `TruceAaxParamInfo` so the shim picks the matching `AAX_ITaperDelegate` per param. ABI bump: `TRUCE_AAX_ABI_VERSION` to 2.
- Standalone: Drop the "(standalone)" suffix from the window title.
- baseview: bump to the latest revision.
- Workspace: README status updated to **stable**; `repository` / `homepage` metadata added to every crate's Cargo.toml for crates.io publishing readiness.

## 0.40.2

- Move example READMEs out to truce-website (no code impact).
- Wrap VST3 / VST2 / AU / AAX state-save and state-load callbacks in `catch_unwind`. A panic from user `save_state` / `load_state` used to unwind across the `extern "C"` FFI boundary back into the host UB on most toolchains, abort on others. The save paths now pre-zero the host's out pointers so a panic mid-write leaves the host seeing an empty blob rather than a stale buffer.

## 0.40.1

- AU v3: Wire `macos_icon` through the bundle template. When set in `truce.toml`, the per-plugin `.icns` is copied into the `.app`'s `Contents/Resources/` and `CFBundleIconFile` is added to the outer Info.plist, matching the standalone-host behavior.

## 0.40.0

- CLAP: Use the macOS bundle layout (`Contents/MacOS` + `Info.plist`). Fixes load in Bitwig ([#51](https://github.com/truce-audio/truce/issues/51)).
- CLAP: Wire stubs for `get_resize_hints`, `set_transient`, `suggest_title`, `set_size`, `adjust_size` so the custom-editor button appears in Bitwig.
- fundsp: New `truce-example-fundsp-reverb-worker` showing a background-thread graph rebuild with a lock-free swap into the audio thread `process()` stays allocation-free.
- fundsp: Rename the inline-rebuild example to `truce-example-fundsp-reverb-simple` (pedagogical, rt-unsafe).
- Follow stable Rust toolchain (unpin from 1.90).
- Dead-code removal, stylistic fixes.

## 0.39.3

- New example integrating with fundsp; added small helpers.
- AU v2: Fix registration bug causing GUI init issues.
- LV2: Fix URI mismatch between manifest and runtime.

## 0.39.2

- Consistent naming scheme for package installers across macOS, Windows, and Linux.

## 0.39.1

- Standalone on macOS: Fix audio input after install, was missing the audio-input entitlement.

## 0.39.0

- LV2: Add packaging support.
- Enable notarization for example plugins.
- Installer: Harden against permission issues from prior installs.
- Wire `macos_icon`, `windows_icon`, `welcome_bmp`, `welcome_html` for example plugins.
- Installer / packaging bug fixes.
- Bump MSRV to 1.90.

## 0.38.0

- LV2: Fix MIDI effect categorization.
- Improved precision ergonomics using fundsp-style preludes.
- **Breaking:** renamed `param.smoothed_next()` to `param.read()` to support consistent float precision use. Upgrade path is a mechanical.
- Minor fixes.

## Roadmap & Known Gaps

The roadmap (known gaps + future work) lives at [truce.audio/docs/roadmap](https://truce.audio/docs/roadmap).
