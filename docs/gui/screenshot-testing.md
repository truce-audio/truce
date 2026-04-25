# Screenshot Testing

Screenshot tests catch visual regressions by rendering your GUI to an
image and comparing it against a committed reference. If something
changes unexpectedly — a widget moves, a color shifts, a label
disappears — the test fails on the reference platform and points at
the freshly-rendered PNG so you can compare visually.

The shape is the same regardless of which GUI backend you use. Each
backend exposes a `render_to_pixels(...) -> (Vec<u8>, u32, u32)`
helper that returns RGBA bytes and physical dimensions. A single
backend-agnostic comparator — `truce_test::assert_screenshot` — does
the diffing, file I/O, and platform gating. Two lines per test:

```rust
let (pixels, w, h) = truce_<backend>::screenshot::render_to_pixels(...);
truce_test::assert_screenshot(name, &pixels, w, h, max_diff, "snapshots");
```

## How it works

1. The backend's `render_to_pixels` runs your GUI headlessly (no
   window needed) and returns RGBA bytes plus physical dimensions.
2. `assert_screenshot` writes the current render to
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

## Adding a screenshot test

Pick the backend matching your editor and use its `render_to_pixels`.
The `truce_test::assert_screenshot` call is identical in every case.

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

## Tolerance

`assert_screenshot`'s `max_diff_pixels` argument is the number of
RGBA bytes that may differ before the test fails. `0` is exact-match
(used by every truce example). Bump it to a small number like
`100`–`500` if anti-aliasing or font hinting introduces flake on
your reference platform.

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

## Texture format gotchas

`assert_screenshot` always reads RGBA8 bytes; each backend's
`render_to_pixels` is responsible for converting from its native
format into that shape. Each backend already does this — the table
below is for debugging color mismatches if you're hand-rolling a
renderer.

| Backend | Live format | Screenshot bytes returned |
|---------|------------|----------------------------|
| Built-in (`truce-gpu`) | Non-sRGB surface default | RGBA8 |
| egui (`truce-egui`) | `Rgba8UnormSrgb` | RGBA8 (sRGB) |
| Iced (`truce-iced`) | `Bgra8UnormSrgb` | RGBA8 (sRGB, swizzled) |
| Slint (`truce-slint`) | CPU pixels (premultiplied) | RGBA8 (un-premultiplied) |

Mismatches usually look like a uniform tint shift (everything
darker / lighter / wrong red-blue) — that's a sign the renderer is
returning bytes in a format `assert_screenshot` can't compare against
the reference.
