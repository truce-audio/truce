# Screenshot Testing

Screenshot tests catch visual regressions by rendering your GUI to an
image and comparing it against a committed reference. If something
changes unexpectedly — a widget moves, a color shifts, a label
disappears — the test fails on the reference platform and points at
the freshly-rendered PNG so you can compare visually.

The unified API is a single line:

```rust
#[test]
fn gui_screenshot() {
    truce_test::assert_screenshot::<Plugin>("my_plugin_default", "snapshots", 0);
}
```

`truce_test::assert_screenshot` instantiates your plugin via the same path the
host uses, asks the editor for a headless render, and hands the bytes
to the backend-agnostic comparator. Works for the built-in GUI and all
custom backends (egui, iced, slint).

## How it works

1. `truce_test::assert_screenshot::<Plugin>` creates the plugin, calls
   `Editor::screenshot()`, and gets back `(pixels, width, height)`.
2. The current render is always written to
   `<workspace>/target/screenshots/<name>.png` — gitignored, so it
   never accidentally gets committed.
3. The committed reference is loaded from
   `<workspace>/<reference_dir>/<name>.png`. You choose where
   `reference_dir` lives (typically `snapshots/` for a plugin project,
   or `examples/screenshots/` in a workspace).
4. If the reference doesn't exist yet, the test logs a `cp`-based
   "promote" hint with the exact command and **passes**. New tests
   land non-disruptively.
5. If the reference exists and the diff exceeds tolerance, the test
   fails on the reference platform and reports the diff
   informationally on others.

References are saved at 2× resolution with 144 DPI metadata so they
look correct on GitHub and in image viewers.

## Promoting a new or changed render

The test's panic / "no reference" message includes the exact `cp`
command. A typical flow:

```sh
# 1. Run the test. It either fails (regression) or logs a promote hint
#    (new test or accepted change).
cargo test -p my-plugin -- gui_screenshot

# 2. Inspect target/screenshots/my_plugin_default.png. If it looks
#    correct, promote it:
cp target/screenshots/my_plugin_default.png snapshots/my_plugin_default.png

# 3. Commit the updated reference.
git add snapshots/my_plugin_default.png
```

To regenerate every reference at once after an intentional UI change:

```sh
rm -f snapshots/*.png
cargo test --workspace -- gui_screenshot
# Tests pass with promote hints. Inspect target/screenshots/*.png,
# then bulk-promote:
cp target/screenshots/*.png snapshots/
git add snapshots/
```

`cargo truce screenshot` is a faster path when you just want fresh
PNGs (no test harness, no diffing): it builds each plugin's cdylib
once and invokes the `__truce_screenshot` symbol directly, writing
straight to `target/screenshots/`.

## Tolerance

The third argument to `assert_screenshot` is `max_diff_pixels` — how
many RGBA bytes are allowed to differ before the test fails on the
reference platform. `0` means exact-match (the typical case). Raise it
if anti-aliasing or font hinting introduces flake:

```rust
truce_test::assert_screenshot::<Plugin>("my_plugin_default", "snapshots", 200);
```

For more elaborate scenarios (rendering pixels through a non-`Editor`
path), drop down to [`assert_screenshot_pixels`](#pixel-comparator).

When a test fails, the current render is at
`target/screenshots/<name>.png` and the reference is at
`<reference_dir>/<name>.png`. Open both and compare visually; if the
new render is correct, promote it via the `cp` command in the panic
message.

## Cross-OS behavior

The committed reference PNGs are owned by **one platform** — by
default, macOS. The rendering pipeline runs on every OS (Linux,
Windows, macOS), so screenshot tests double as smoke coverage that
the wgpu / Slint software renderer pipeline doesn't crash anywhere.
Comparison against the reference, however, is gated:

| Platform | Render | Compare | On diff |
|---|---|---|---|
| Reference (`macos` by default) | yes (→ `target/screenshots/`) | yes | **fail the test**, panic message names both PNGs |
| Non-reference | yes (→ `target/screenshots/`) | yes | log diff count, **pass** |

Why one platform owns the references: Metal, DX12, and Vulkan each
have their own anti-aliasing and text-rasterization quirks, so even
identical wgpu API calls produce slightly different bytes per
backend. Pixel-perfect cross-OS reference comparison would either
require software rendering everywhere or per-platform reference
trees. The current model keeps one canonical set of references and
treats per-platform diffs as informational.

### Choosing the reference platform

Override the default with the `TRUCE_SCREENSHOT_REFERENCE_OS`
environment variable. Valid values match `std::env::consts::OS`:
`macos`, `linux`, `windows`. For example, in a Linux-first CI:

```yaml
env:
  TRUCE_SCREENSHOT_REFERENCE_OS: linux
```

After flipping the reference, regenerate every PNG on the new
reference platform (`rm <reference_dir>/*.png && cargo test
--workspace -- gui_screenshot`, then `cp target/screenshots/*.png
<reference_dir>/`) so the saved bytes match what that platform
produces.

### Inspecting non-reference diffs

Non-reference platforms still render to `target/screenshots/` and
print a line like:

```
[truce-egui] non-reference diff on linux: 1532 pixels differ vs
.../snapshots/gain_egui_default.png (informational; max allowed on
reference: 0). Current render at .../target/screenshots/gain_egui_default.png.
```

That gives you a way to spot real cross-platform regressions
(e.g. a Linux-only rendering bug) without having the test be
permanently red on those platforms.

## API reference

### Render and assert

```rust
pub fn truce_test::assert_screenshot<P: PluginExport>(
    name: &str,             // file-stem; ends up at <reference_dir>/<name>.png
    reference_dir: &str,    // workspace-relative dir for committed PNGs
    max_diff_pixels: usize, // RGBA bytes allowed to differ; 0 = exact
)
```

Instantiates the plugin, asks its editor for a headless render via
`Editor::screenshot(Arc<dyn Params>)`, then delegates to
[`assert_screenshot_pixels`](#pixel-comparator). The synthetic
`Arc<dyn Params>` is built from `<P::Params as Params>::new()` (defaults).

### Render only (no comparison)

```rust
pub fn truce_core::screenshot::render<P: PluginExport>(
    name: &str,
) -> std::path::PathBuf
```

Same render as [`assert_screenshot`], but skips the comparison and
returns the path to the freshly-saved PNG
(`target/screenshots/<name>.png`). Use it to regenerate README artwork
or capture a debug snapshot without involving a reference.

```rust
let path = truce_core::screenshot::render::<Plugin>("gain_dark");
println!("rendered to {}", path.display());
```

Lives in `truce-core` (not `truce-test`) so non-test contexts can call
it without pulling in dev-dependencies — including the `cargo truce
screenshot` CLI (see below). Re-exported as
`truce_test::render_screenshot` for symmetry with the assert helpers.

### `cargo truce screenshot`

Render a plugin's GUI from the command line, no `#[test]` required:

```sh
cargo truce screenshot                              # every plugin in truce.toml
cargo truce screenshot -p my-plugin                 # one plugin
cargo truce screenshot -p my-plugin --name dark     # → target/screenshots/dark.png
```

Default filename is `<bundle_id>_screenshot.png`. Use this to
regenerate README artwork or capture debug snapshots without writing
test code.

How it works: `truce::plugin!` exports a hidden `extern "C" fn
__truce_screenshot(...)` symbol into the plugin's cdylib (the same
build artifact CLAP/VST3 use). The CLI builds the cdylib with
`--no-default-features --lib` (skipping format-wrapper compilation
for speed), `dlopen`s it, and calls the symbol. No per-plugin
scaffolding required — the macro provides everything.

### Pixel comparator

```rust
pub fn truce_test::assert_screenshot_pixels(
    name: &str,            // file-stem; ends up at <reference_dir>/<name>.png
    pixels: &[u8],         // RGBA8, row-major, width*height*4 bytes
    width: u32,            // physical width (already scale-multiplied)
    height: u32,           // physical height
    max_diff_pixels: usize, // RGBA bytes allowed to differ; 0 = exact
    reference_dir: &str,   // workspace-relative dir for committed PNGs
)
```

Use this directly if you need a non-zero tolerance: capture pixels
from `editor.screenshot(params)` yourself, then call
`assert_screenshot_pixels` with your chosen `max_diff_pixels`. Always
writes the current render to `<workspace>/target/screenshots/<name>.png`.
Loads the committed reference from
`<workspace>/<reference_dir>/<name>.png`. Missing reference logs a `cp`
promote hint and passes; present reference fails on diff >
`max_diff_pixels` (reference platform only).

### Editor trait method

```rust
fn screenshot(
    &mut self,
    params: Arc<dyn truce_params::Params>,
) -> Option<(Vec<u8>, u32, u32)>;
```

Built-in backends (`truce-gpu`, `truce-egui`, `truce-iced`,
`truce-slint`) all implement this. Custom editor implementations only
need to override it if they want to be testable through the unified
helper.

---

## Texture format gotchas

`assert_screenshot_pixels` always reads RGBA8 bytes; each backend's
`Editor::screenshot()` impl is responsible for converting from its
native format into that shape. Each backend already does this — the
table below is for debugging color mismatches if you're hand-rolling a
renderer.

| Backend | Live format | Screenshot bytes returned |
|---------|------------|----------------------------|
| Built-in (`truce-gpu`) | Non-sRGB surface default | RGBA8 |
| egui (`truce-egui`) | `Rgba8UnormSrgb` | RGBA8 (sRGB) |
| Iced (`truce-iced`) | `Bgra8UnormSrgb` | RGBA8 (sRGB, swizzled) |
| Slint (`truce-slint`) | CPU pixels (premultiplied) | RGBA8 (un-premultiplied) |

Mismatches usually look like a uniform tint shift (everything
darker / lighter / wrong red-blue) — that's a sign the renderer is
returning bytes in a format `assert_screenshot_pixels` can't compare
against the reference.
