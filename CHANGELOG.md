# Changelog

Notable changes per release.

## 0.57.0

- **Resizable editors.** Every GUI backend opts in with
  `.resizable(true).min_size((a, b)).max_size((a, b))` on the
  editor / layout, and the CLAP, VST3, AU, and LV2 wrappers
  round-trip host requests to it. Logical points everywhere
  except the built-in grid, which takes `(cols, rows)` cell
  counts (it snaps to whole cells anyway). Vizia plugin-form
  `set_size` is a known gap.
- **`truce-vizia`: XY pad label moved to bottom.** Matches the
  other backends.
- **`baseview-truce 0.1.1-truce.8`.** Adds the macOS
  `setFrameSize:` `Resized` event + OpenGL drawable resize that
  host-driven editor resize depends on. To upstream to baseview.

## 0.56.0

- **`truce-vizia`: `param_knob` layout matches the other backends.**
  Reordered the cell as knob → value → name; previously the name
  label sat between the knob and the value, which inverted the
  cross-backend convention.
- **`selector` / `param_selector` widgets deprecated.** Marked
  `#[deprecated(since = "0.56.0")]` across `truce-gui-types`,
  `truce-egui`, `truce-iced`, and `truce-vizia`; the slint
  `Selector` element gets the same notice in its widget library
  header (slint markup has no attribute equivalent). Use the
  corresponding `dropdown` / `param_dropdown` / `Dropdown`
  instead.

## 0.55.0

- **AU v3: sample-accurate parameter automation.** The Swift shim
  now decodes `AURenderEvent.parameter` / `.parameterRamp` into
  per-sample `ParamChange` events with the proper within-block
  offset, and the chunker subdivides the audio block at each
  automation point. AU v2 stays block-rate (its
  `AudioUnitSetParameter` API carries no sample-offset).
- **LV2: sample-accurate parameter automation.** The TTL now
  advertises each parameter as a `patch:writable` `lv2:Parameter`,
  and the wrapper decodes host-emitted `patch:Set` Objects from
  the input atom sequence into per-sample `ParamChange` events
  (the atom event's `time_frames` becomes the within-block
  `sample_offset`). The legacy `lv2:ControlPort` path stays so
  older LV2 hosts still update params at block rate.

## 0.54.0

- **New `vst3_subcategory` `truce.toml` key.** Emits the secondary
  VST3 "Plugin Type Categories" token (`Fx|Delay`, `Fx|Reverb`,
  `Instrument|Synth`, …). Without it, Cubase buckets the plugin
  under "Other". Optional; opt-in per plugin.

## 0.53.0

- **New `FloatParam::read_into(&mut [f32])` smoother API.** Slice-based
  block read; advances the smoother by exactly `out.len()`. Same
  one-atomic-pair amortization as `read_block`, runtime length.
- **Deprecated `FloatParam::read_block::<N>()`.** Always advanced by `N`
  regardless of consumed samples, silently stepping the smoothed value
  at the next block boundary whenever the host's block size wasn't a
  multiple of `N`. Audible as clicks on delay / LFO-rate / any
  timing-sensitive smoothed param. `read_into(&mut scratch[..n])` is
  the same code shape with the hazard removed.
- **New `truce_simd::math64` module.** f64 mirror of `truce_simd::math`
  (`db_to_linear_block`, `linear_to_db_block`, `exp2_block`,
  `log2_block`, `tanh_block`). `wide::f64x4` lanes.
- **`eq` example uses SIMD math64.** Output stage dB → linear runs
  through `math64::db_to_linear_block` instead of scalar `f64::powf`
  per sample.
- **Examples migrated** (`block-gain`, `block-saturate`, `eq`).

## 0.52.0

- **New GUI backend: `truce-vizia`.** Param-bound widgets, headless
  screenshot. Desktop only (no iOS, no Windows ARM64).
- **New examples: `truce-example-gain-vizia`,
  `truce-example-gui-zoo-vizia`.**
- **Sample-accurate parameter automation.** Param changes apply at
  their `sample_offset` instead of the start of the block; smoothers
  start ramping at the event sample. On by default. Tune via
  `[automation] min_subblock_samples` in `truce.toml` or opt out
  per-param with `#[param(chunk = false)]`.
- **`truce-iced`: `with_font(bytes)` matches egui / vizia.** Family
  name is now read from the TTF (was `with_font(family, bytes)`).
- **`cargo truce install` / `package`: dedupes duplicate archive
  members during macOS bundle link.** Fixes the `clang -bundle`
  duplicate-symbol failure plugins with `skia-bindings` (vizia)
  could hit.

## 0.49.23

- **`keyboard-types` 0.6 -> 0.7** workspace-wide.

## 0.49.22

- **New example: `truce-example-dasp-bitcrusher`.** Showcases
  `dasp_sample::Sample` bit-depth round-tripping for 8-bit / 16-bit
  quantization, with a sample-and-hold downsampler.
- **`truce-example-gui-zoo-iced`: added a Dropdown section** exercising
  the `param_dropdown` alias added in 0.49.21.

## 0.49.21

- **`truce-egui`: int sliders now snap and show plain integers.**
  `param_slider` adapts to `ParamRange::Discrete` (plain `min..=max`,
  integer step, integer label) instead of the normalized [0, 1]
  display.
- **`truce-egui`: multi-channel meters render past 2 bars.** The
  widget allocated a fixed 16 px width regardless of channel count, so
  bars beyond ~3 were clipped. Width now grows with channel count.
- **`truce-egui`: toggle height matches the selector** so a row mixing
  them bottom-anchors labels on the same baseline.
- **`truce-iced`: `param_dropdown` alias added** for `param_selector`,
  matching the egui / CPU APIs.
- **`truce-slint`: new `Dropdown` widget** (popup-style, wraps the
  std-widgets `ComboBox`). Available via `import { Dropdown } from
  "@truce"`.
- **`truce-slint`: pinned std-widgets style to `fluent` on every host
  OS.** The default picked Cupertino on macOS, whose `ComboBox`
  rendered its chevron region with a persistent accent-blue square.
- **`truce-slint`: `Toggle` height bumped 40 → 50** to align labels
  with `Selector` / `Dropdown` in mixed rows.
- **New examples: `truce-example-gui-zoo-egui` / `-iced` / `-slint`.**
  Each mirrors the CPU `gui-zoo`'s param set so every renderer's
  widget surface is exercised against the same shape.

## 0.49.20

- **GUI: discrete params now snap during drag and wheel.** `IntParam` /
  `EnumParam` slider, knob, and XY-pad drags previously emitted
  continuous normalized values; storage snapped on writeback but the
  in-flight edit left UI and audio reads briefly out of phase.
  `ParamSnapshot::snap_normalized` now snaps at emit time.
- **GUI: dropdown A→B switch now repaints on the CPU renderer.** The
  editor's repaint gate diffs `dropdown_is_open()` which stays `true`
  across a close-then-open inside one click; `dropdown_close` and
  `open_dropdown` now flag the dirty bit explicitly.
- **Layout: labeled sections now start strictly below the previous
  section's tallest widget.** Section breaks advanced `cursor_row` by
  1, which packed the next section alongside a tall (`rows = N`) widget
  from the prior section. Now advances past `max_occupied_row`.
- **New `deg` unit.** `#[param(unit = "deg")]` (or `"°"`) prints e.g.
  `180.0°`. `ParamUnit::Degrees` slots into the existing variant set.
- **New example: `truce-example-gui-zoo`.** Passthrough plugin that
  exercises every built-in widget kind across mixed spans and
  positions, every `ParamUnit` variant, and the discrete-snap path.
  Lives in `examples/` and is wired into desktop + iOS screenshot CI.

## 0.49.19

- **`#[param(default = std::f64::consts::*)]` no longer trips
  `clippy::approx_constant` in the macro expansion.** The 0.49.18
  parser resolved the path to its `f64` value but `quote!` re-emitted
  it as a literal. The derive now embeds the original path tokens
  verbatim while keeping the resolved `f64` for the compile-time
  range / shape checks.

## 0.49.18

- **`#[param(default = ...)]` now accepts `std::f64::consts::*`.**
  Also `core::f64::consts::*` and bare `f64::consts::*`. Lets plugins
  write `default = std::f64::consts::SQRT_2` instead of a literal.

## 0.49.17

- **`truce-gui`: fixed dropdown menu dirty-tracking bugs.** Scroll
  wheel, touch-drag scroll, and per-option hover highlights mutated
  popup state without flagging the repaint gate, so on the CPU
  renderer they only became visible when an unrelated event tripped
  a repaint. (GPU renderer was unaffected: it re-renders every frame.)

## 0.49.16

- **macOS packaging: CLAP/VST3/VST2 bundles are now ~half the size.**
  The bundle-bin link path inherited `-all_load` without a matching
  `-dead_strip`, so every staticlib object survived; AU/AAX cdylib
  links got `-dead_strip` for free via rustc. Added `-dead_strip` to
  the `clang -bundle` step.

## 0.49.15

- **`truce-gui`: fixed a dropdown popup positioning bug.** The popup
  no longer mis-anchors relative to its trigger widget.

## 0.49.14

- **Standalone: added MIDI device + channel selection** via a unified
  Settings menu on macOS/Windows, or `--midi-input` / `--midi-channel`
  (`omni` or `1`-`16`) on Linux.

## 0.49.13

- **VST2/AAX: extended the 0.49.9 `set_state` fix.** The same path that
  dropped GUI-edited custom state in CLAP/VST3/AU was present in VST2
  and AAX; they now route editor bytes to `load_state` like the others.
  (LV2 is unaffected: its UI is out-of-process and never touches
  `set_state` directly.)

## 0.49.12

- **`truce-slint`: fixed a panic on HiDPI displays after a resize event.**
  The render buffer kept the pre-resize physical extents while slint's
  window adopted the new ones, so the next frame tripped slint's
  buffer-too-small check.

## 0.49.11

- **Standalone: fixed the macOS device menu** — the Input/Output Device
  submenus were grayed out and unopenable.
- **Standalone: added input/output channel selection** (mono channel or
  stereo pair) via the macOS/Windows menus or `--input-channels` /
  `--output-channels` (the CLI is the picker on Linux).

## 0.49.10

- **Fixed a use-after-free crash when a host closes the editor without
  calling `close()`** (seen in Ableton with several plugins loaded). The
  macOS frame timer kept firing against the freed editor; every editor
  backend now cancels its window on drop.
- **Fixed meters not updating in the built-in (CPU) editor.** The
  repaint gate only watched parameter changes, so a moving meter stayed
  frozen until an unrelated repaint (e.g. dragging a knob) fired; the
  editor now repaints when a meter value moves.

## 0.49.9

- **Fixed `#[derive(State)]` custom state not persisting when edited
  from the GUI.** The CLAP, VST3, and AU editor `set_state` paths
  silently dropped GUI edits; they now reach `load_state` correctly.

## 0.49.6

- **`truce-standalone`: the `gui` feature no longer pulls `truce-gpu`.**
  Standalone builds compile the GPU backend only when the plugin opts in
  via `truce-gui = { …, features = ["gpu"] }`.

## 0.49.4

- **Fixed a macOS stack-overflow crash in the built-in (CPU) GUI /
  standalone path.** Moving the cursor over a freshly-opened editor
  window aborted the process. Fixed in `baseview-truce 0.1.1-truce.4`.

## 0.49.2

- Housekeeping: minor README updates and safety fixes.
- Standalone: Disable window resizing on Windows for now (mirrors
  other OSes).
- Standalone: Wire `windows_icon` through to the window's
  `WM_SETICON` so the title bar and taskbar show the app icon.

## 0.49.0

- **Breaking (`PluginLogic`): the GUI surface collapses to a single
  `editor()` method.** The old `layout()`, `custom_editor()`,
  `render()`, `uses_custom_render()`, and `hit_test()` methods are
  gone — every plugin now returns its editor from one place:

  ```rust
  fn editor(&self) -> Box<dyn Editor> { /* ... */ }
  ```

  `editor()` is required (there is no headless auto-fallback).
  Migration steps below.
- **Renderer split: the built-in GUI now defaults to CPU
  (tiny-skia); wgpu is opt-in.** The CPU rasterizer moved into a
  new `truce-cpu` crate, a peer of `truce-gpu`. A layout-only
  plugin no longer compiles wgpu and its per-OS graphics backends
  unless it asks for them — smaller binaries and faster builds out
  of the box. Opt into GPU rendering with
  `truce-gui = { version = "0.49", features = ["gpu"] }`; the `gpu`
  path doesn't pull the CPU dependency tree, and vice-versa. Plain
  `truce-gui = "0.49"` keeps the CPU default with no change.
- The `truce` umbrella no longer pulls `truce-gui` transitively.
  Plugins using the built-in renderer declare `truce-gui`
  explicitly — newly scaffolded projects (`cargo truce new`)
  already do.

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

- `cargo-truce`: Fix various install-path bugs on Windows. VST3 now
  installs to system scope by default — Ableton doesn't scan the
  per-user VST3 directory.
- `truce-gui` & `truce-gpu`: Minor built-in GUI rendering improvements.

## 0.48.10

- Minor housekeeping.
- `cargo-truce`: Replace non-Latin status glyphs in `cargo truce
  package` output with the same `[ OK ]` / `[FAIL]` ASCII tags
  used by `cargo truce doctor`. The Unicode `✓` / `✗` characters
  broke rendering in Windows 10 WSL terminals.

## 0.48.9

- Examples: Fix blocksize bug in EQ example.
- AAX: Set `UNSAFE_OBJC2_ALLOW_CLASS_OVERRIDE` when building AAX
  to prevent same-class-name crashes when multiple AAX plugins
  load into the same host process (e.g. Pro Tools loading two
  truce plugins). Details:
  <https://github.com/rust-windowing/raw-window-metal/issues/29>
- `cargo-truce`: Make `--ios` packaging behavior and naming scheme
  consistent with the other formats / OSes — iterates every
  plugin in the workspace (no longer errors when more than one is
  declared) and writes the artifact to
  `target/dist/<crate>-<version>-ios.ipa` next to the macOS
  `.pkg` / Windows `.exe` / Linux `.tar.gz`.

## 0.48.8

- **truce now fully published to crates.io.** `cargo truce new` scaffolds
  with a `truce = { version = "0.48" }` registry pin by default;
  the historical `git = "...", tag = "v0.48.7"` form stays
  available via `cargo truce new --github` for scaffolding against
  an unreleased checkout.
- New `truce-aax-bridge` crate carries the C ABI header so
  `cargo-truce` doesn't transitively pull the full `truce-aax`
  runtime stack.

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

- Standalone: Fix default device selection on Linux (broke after
  cpal 0.17 update in 0.48.x).

## 0.48.3

- `cargo-truce`: Improve iOS screenshot reliability, clear any
  stale `_truce_editor_frame.json` before launch, extend the
  first-paint poll timeout for cold CI runners, and hard-fail
  with a diagnostic when the editor never renders (instead of
  silently proceeding to a misleading "screenshot size mismatch").

## 0.48.2

- `truce-egui`: Migrate to egui 0.34 (bumps `wgpu` to 29 transitively).
- **Breaking (`truce-egui`):** `EditorUi::ui` and the `EguiEditor::new`
  closure now receive `&mut egui::Ui` instead of `&egui::Context`
  egui 0.34 deprecates `Context::run` plus the per-panel `show(ctx, …)`
  entry points in favor of `Context::run_ui` and `show_inside(ui, …)`.
- Bump MSRV to 1.92.
- For egui-0.33 parity with `nih-plug`, pin truce to `0.47.0`.

(0.48.0 and 0.48.1 were yanked, install 0.48.2.)

## 0.47.0

- `truce-egui`: Migrate to egui 0.33 (bumps `wgpu` to 27 transitively).
- `truce-gpu`: Declare wgpu graphics backends per-OS (fixes Linux).

## 0.46.0

- `truce-iced`: Migrate to iced 0.14.
- `truce-egui`: Add `param_dropdown` widget (stock click-to-open
  dropdown wrapping `egui::ComboBox`).
- `truce-egui`: `param_knob` snaps to discrete steps on enum /
  discrete params.
- Examples: Tremolo refreshed (compact transport line, fractional
  rate labels, dropdown polish).

## 0.45.4

- LV2: Fix install path on Windows.
- LV2: Fix param defaults (mirror other formats).
- Standalone: Disable window resizing on Linux (mirror other OSes).
- Examples: Fix `fourcc` collision between two example plugins.

## 0.45.3

- `cargo-truce`: Fix plugin-name to path sanitization mismatch
  between `build` / `install` and `package` (display names with
  filesystem-reserved characters like `Truce Dry/Wet` produced a
  ~15 KB empty installer because `productbuild`'s distribution.xml
  referenced the raw name while the on-disk `.pkg` files used the
  sanitized name).

## 0.45.2

- AAX: Fix ABI mismatch bug (broken since 0.43.0).
- AAX / `cargo-truce`: Add Pro Tools `pluginrunner` to
  `cargo truce validate` (used if present).
- LV2: Fix editor positioning quirks (some quirks remain but no
  showstoppers).
- `cargo-truce`: Update help with `--target-cpu`.
- `cargo-truce`: Thread `--target-cpu` args through `install`,
  `package`, and `run` commands (with sane defaults).
- `cargo-truce`: `status` no longer runs auval (too slow for the
  purpose); use `validate` for that.
- Stylistic sweep.

## 0.45.0

- New CI gate exercises every prior release's example crates
  against the current `truce` HEAD, catching backwards-incompatible
  changes before they ship.
- **Initial SIMD block operations.** New `truce-simd` crate ships
  `wide`-backed `scale_block` / `mul_block` / `mix_block` /
  `mac_block` / `copy_block` / `zero_block` plus `tanh_block` /
  `db_to_linear_block` / `linear_to_db_block` math helpers, with
  scalar fallbacks. Six new examples (`gain-simd`, `saturate`,
  `drywet`, `gate`, `widen`, `surround-meter`) demonstrate the
  shapes.
- **`cargo truce build` now defaults x86_64 builds to
  `-C target-cpu=x86-64-v3`** (AVX2 + FMA + BMI2) so the SIMD
  paths above activate without any per-developer config. New
  `--target-cpu <value>` flag accepts `baseline` (rustc default
  = SSE2-only), `v2` / `v3` / `v4`, `native` for the local-CPU
  dev-loop, or any literal rustc target-cpu name.
- Plugin display names that contain filesystem-reserved characters
  (e.g. `Truce Dry/Wet`) are now sanitized at the path-construction
  boundary, so the on-disk bundle lands at `Truce Dry-Wet.aaxplugin`
  while DAWs still display the raw name from the plist.

## 0.44.0

- **VST3 + CLAP on macOS now link as `MH_BUNDLE` instead of
  `MH_DYLIB`.** Fixes load under hosts that take the strict
  `CFBundleLoadExecutable` path (DawDreamer's JUCE-based VST3
  host is the one we validated against). Most desktop DAWs have
  more forgiving loaders and weren't affected, but the strict
  path is the correct Mach-O shape for a bundle. Built from a
  Rust `staticlib` via `clang -bundle`. **Breaking change for
  pre-0.44.0 plugins:** the plugin crate's `[lib]` block needs
  `crate-type = ["cdylib", "staticlib", "rlib"]` (was `["cdylib",
  "rlib"]`). `cargo truce install` / `package` fails loudly with
  the exact one-line edit if the staticlib is missing.
- AU v3: Fix installs broken since 0.42.0.
- AU v2: Fix `PresentPreset` handler broken since 0.40, auval
  passes again across all bundled examples.
- `cargo truce package --formats <list>` now works on Linux, matching
  the existing macOS / Windows behavior. Internally drives the
  underlying `cargo truce build` invocation.
- CI hardening: every `cargo truce` subcommand (install, validate,
  package, uninstall, doctor, status, reset-au) now runs on macOS,
  Linux, and Windows on every PR. New scaffold-and-round-trip
  workflow exercises `cargo truce new` against single-effect,
  single-instrument, and mixed-workspace shapes.
- Doc sweep across the in-tree comments and rustdoc.

## 0.43.0

- **SysEx + UMP support (work in progress).** Initial plumbing
  for System Exclusive messages and MIDI 2.0 UMPs

## 0.42.1

- Params: `IntParam` value displays no longer pick up the
  `FloatParam` `{:.1}` / `{:.2}` formatters, so transpose's
  semitone knob now reads `0 st` instead of `0.0 st`. Internally,
  `ParamInfo` gained a `kind: ParamValueKind` field set by
  `#[derive(Params)]` from the field type.
- Example tidy: the `Mix` knobs on both fundsp reverbs and the
  `Depth` knob on tremolo now declare `unit = "%"`, so they render
  as `25%` / `0%` instead of `0.25` / `0.00`.

## 0.42.0

- **iOS support.** AU v3 plug-ins now build, install, and run on
  both the iOS Simulator (`cargo truce install --ios`) and tethered
  devices (`cargo truce install --ios-device`). Truce ships a Swift
  container app template with embedded editor, Play button, status
  label, info sheet, and a hamburger-menu landscape layout. New
  `[[plugin]]` knobs in `truce.toml`: `ios_icon_set`,
  `ios_orientations`, `ios_scale_editor_to_fit` (default `true`),
  `ios_minimum_os_version`, `ios_app_group`, `ios_url`. Touch input
  is pinned per-finger so multi-touch doesn't hijack an in-progress
  drag. `mute_preview_output` works on both standalone and the iOS
  container for analyzer-style plug-ins. Custom container apps and
  iced's iOS backend remain unsupported (latter blocked upstream).
  See the new [iOS chapter](/docs/guide/ios).
- iOS screenshot regression: `cargo truce screenshot --ios` captures
  the simulator's actual rendered output (the only path that sees
  iOS-specific compositing); `--check` gates baselines in CI.

## 0.41.0

- AAX: Fix knobs sync bug on log-ranged parameters. The C++
  shim defaulted to a linear taper for every param's normalize /
  denormalize, so AAX would round-trip a log-ranged knob through
  `RenderAudio` into a different plain value than the editor
  wrote. Wire `range_type` through `TruceAaxParamInfo` so the shim
  picks the matching `AAX_ITaperDelegate` per param. ABI bump:
  `TRUCE_AAX_ABI_VERSION` to 2.
- Standalone: Drop the "(standalone)" suffix from the window title.
- baseview: bump to the latest revision.
- Workspace: README status updated to **stable**; `repository` /
  `homepage` metadata added to every crate's Cargo.toml for
  crates.io publishing readiness.

## 0.40.2

- Move example READMEs out to truce-website (no code impact).
- Wrap VST3 / VST2 / AU / AAX state-save and state-load callbacks
  in `catch_unwind`. A panic from user `save_state` / `load_state`
  used to unwind across the `extern "C"` FFI boundary back into the
  host UB on most toolchains, abort on others. The save paths
  now pre-zero the host's out pointers so a panic mid-write leaves
  the host seeing an empty blob rather than a stale buffer.

## 0.40.1

- AU v3: Wire `macos_icon` through the bundle template. When set
  in `truce.toml`, the per-plugin `.icns` is copied into the
  `.app`'s `Contents/Resources/` and `CFBundleIconFile` is added
  to the outer Info.plist, matching the standalone-host behavior.

## 0.40.0

- CLAP: Use the macOS bundle layout (`Contents/MacOS` +
  `Info.plist`). Fixes load in Bitwig
  ([#51](https://github.com/truce-audio/truce/issues/51)).
- CLAP: Wire stubs for `get_resize_hints`, `set_transient`,
  `suggest_title`, `set_size`, `adjust_size` so the custom-editor
  button appears in Bitwig.
- fundsp: New `truce-example-fundsp-reverb-worker` showing a
  background-thread graph rebuild with a lock-free swap into the
  audio thread `process()` stays allocation-free.
- fundsp: Rename the inline-rebuild example to
  `truce-example-fundsp-reverb-simple` (pedagogical, rt-unsafe).
- Follow stable Rust toolchain (unpin from 1.90).
- Dead-code removal, stylistic fixes.

## 0.39.3

- New example integrating with fundsp; added small helpers.
- AU v2: Fix registration bug causing GUI init issues.
- LV2: Fix URI mismatch between manifest and runtime.

## 0.39.2

- Consistent naming scheme for package installers across macOS,
  Windows, and Linux.

## 0.39.1

- Standalone on macOS: Fix audio input after install, was
  missing the audio-input entitlement.

## 0.39.0

- LV2: Add packaging support.
- Enable notarization for example plugins.
- Installer: Harden against permission issues from prior installs.
- Wire `macos_icon`, `windows_icon`, `welcome_bmp`,
  `welcome_html` for example plugins.
- Installer / packaging bug fixes.
- Bump MSRV to 1.90.

## 0.38.0

- LV2: Fix MIDI effect categorization.
- Improved precision ergonomics using fundsp-style preludes.
- **Breaking:** renamed `param.smoothed_next()` to `param.read()` to support consistent float precision use. Upgrade path is a mechanical.
- Minor fixes.

## Backlog

### Known gaps

- **Retail iLok / PACE round-trip.** PACE wraptool is wired and
  exercised against a dev iLok account; we haven't yet round-tripped
  through a retail iLok + retail Pro Tools install.
- **Authenticode round-trip with a real cert.** The Azure Trusted
  Signing / SHA1 thumbprint / `.pfx` paths are wired but haven't
  been exercised with a real EV / OV cert end-to-end.
- **MIDI 2.0 over LV2.** LV2 Atom carries MIDI 1.0 byte streams,
  so plugins emitting MIDI 2.0 channel-voice, per-note, or
  ParamChange events drop those messages when loaded as LV2.
- **`truce-vizia` resize.** Vizia editors are fixed-size in
  plugin form: `ViziaEditor::set_size` is a no-op because
  `vizia::WindowHandle` exposes no resize entry point callable
  from outside its event loop. Unblocks when `vizia_baseview`
  upstream adds a window-event resize variant + handler.
- **Bring-your-own iOS container.** `cargo truce install --ios`
  always emits the bundled Swift container template (title, Play,
  status, hamburger overlay). Plug-ins that need a bespoke shell
  hand-author it outside the pipeline and load the `.appex` truce
  builds. 
- **iced on iOS.** iced's `iced_winit` calls a desktop-only `winit`
  trait inside a non-iOS-gated branch, so `truce-example-gain-iced`
  doesn't build for `aarch64-apple-ios*`. Blocked upstream.

### Future

- More example plugins (delay).
- WebView GUI backend.
- Distribution-grade dynamic shell (today's `--shell` is dev-loop
  only; making it a shipping mechanism is a phase-2 question).
