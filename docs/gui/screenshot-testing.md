# Screenshot Testing

Screenshot tests catch visual regressions by rendering your GUI to an
image and comparing it against a committed reference. If something
changes unexpectedly — a widget moves, a color shifts, a label
disappears — the test fails on the reference platform and points at
the freshly-rendered PNG so you can compare visually.

## How it works

1. The test renders your GUI headlessly (no window needed).
2. The current render is **always** written to
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

References are saved at 2x resolution with 144 DPI metadata so they
look correct on GitHub and in image viewers.

## Adding a screenshot test

Every test follows the same shape: a backend-specific
`render_to_pixels` returns `(pixels, width, height)`, and
`truce_test::assert_screenshot` compares against the committed
reference. Same comparator across all four backends; only the
renderer differs.

### Built-in GUI

```rust
#[test]
fn gui_screenshot() {
    let params = Arc::new(MyParams::new());
    let plugin = MyPlugin::new(Arc::clone(&params));
    let layout = plugin.layout();
    let (pixels, w, h) = truce_gpu::screenshot::render_to_pixels(params, layout);
    truce_test::assert_screenshot(
        "my_plugin_default", &pixels, w, h,
        0,             // max pixel differences (0 = exact match)
        "snapshots",   // workspace-relative dir for committed PNGs
    );
}
```

### egui

```rust
#[test]
fn gui_screenshot() {
    let (pixels, w, h) = truce_egui::screenshot::render_to_pixels::<MyParams>(
        WINDOW_W, WINDOW_H,   // use the same constants as your editor
        2.0,                   // scale (2.0 for Retina)
        Some(truce_font::JETBRAINS_MONO),
        |ctx, state| my_ui(ctx, state),
    );
    truce_test::assert_screenshot(
        "my_plugin_egui_default", &pixels, w, h, 0, "snapshots",
    );
}
```

### Iced

```rust
#[test]
fn gui_screenshot_iced() {
    let params = Arc::new(MyParams::new());
    let (pixels, w, h) = truce_iced::screenshot::render_to_pixels::<MyParams, MyEditor>(
        params,
        (WINDOW_W, WINDOW_H),
        2.0,
        Some(("JetBrains Mono", truce_font::JETBRAINS_MONO)),
    );
    truce_test::assert_screenshot(
        "my_plugin_iced_default", &pixels, w, h, 0, "snapshots",
    );
}
```

### Slint

```rust
#[test]
fn gui_screenshot() {
    let (pixels, w, h) = truce_slint::screenshot::render_to_pixels::<MyParams>(
        WINDOW_W, WINDOW_H,
        2.0,
        |state| {
            let ui = MyPluginUi::new().unwrap();
            truce_slint::bind! { state, ui,
                P::Gain => gain,
            }
        },
    );
    truce_test::assert_screenshot(
        "my_plugin_slint_default", &pixels, w, h, 0, "snapshots",
    );
}
```

Slint uses a software renderer — no GPU needed. This makes Slint
screenshots fast and reproducible across machines (font hinting still
varies per-OS, see [Cross-OS behavior](#cross-os-behavior)).

## Keeping editor and screenshot sizes in sync

Define your window dimensions as constants and use them in both the
editor and the screenshot test. This prevents them from drifting
apart:

```rust
const WINDOW_W: u32 = 176;
const WINDOW_H: u32 = 290;

// In custom_editor():
EguiEditor::new((WINDOW_W, WINDOW_H), my_ui)

// In the test:
let (pixels, w, h) = truce_egui::screenshot::render_to_pixels::<MyParams>(
    WINDOW_W, WINDOW_H, 2.0, None, |ctx, state| my_ui(ctx, state),
);
truce_test::assert_screenshot(
    "my_plugin_default", &pixels, w, h, 0, "snapshots",
);
```

## Promoting a new or changed render

The test's panic / "no reference" message includes the exact `cp`
command. A typical flow:

```sh
# 1. Run the test. It either fails (regression) or logs a promote hint
#    (new test or accepted change).
cargo test -p my-plugin -- gui_snapshot

# 2. Inspect target/screenshots/my_plugin_default.png. If it looks
#    correct, promote it:
cp target/screenshots/my_plugin_default.png snapshots/my_plugin_default.png

# 3. Commit the updated reference.
git add snapshots/my_plugin_default.png
```

To regenerate every reference at once after an intentional UI change:

```sh
rm -f snapshots/*.png
cargo test --workspace -- gui_snapshot
# Tests pass with promote hints. Inspect target/screenshots/*.png,
# then bulk-promote:
cp target/screenshots/*.png snapshots/
git add snapshots/
```

## Tolerance

The last argument (`max_diff`) controls how many bytes can differ. Use
`0` for exact match. If anti-aliasing differs between GPU drivers, bump
it to a small number like `100`.

When a test fails, the current render is at
`target/screenshots/<name>.png` and the reference is at
`<reference_dir>/<name>.png`. Open both and compare visually; if the
new render is correct, promote it via the `cp` command in the
panic message.

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
--workspace -- gui_snapshot`, then `cp target/screenshots/*.png
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

## Texture format gotchas

Screenshots must use the same texture format as the live editor, or
colors will look wrong (typically darker or lighter). The backends
handle this automatically, but if you're debugging color mismatches:

| Backend | Live format | Screenshot format |
|---------|------------|-------------------|
| Built-in | Non-sRGB surface default | `Rgba8Unorm` |
| egui | `Rgba8UnormSrgb` | `Rgba8UnormSrgb` |
| Iced | `Bgra8UnormSrgb` | `Bgra8UnormSrgb` |
| Slint | CPU pixels | RGBA8 |
