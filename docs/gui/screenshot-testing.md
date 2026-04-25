# Screenshot Testing

Screenshot tests catch visual regressions by rendering your GUI to an
image and comparing it against a saved reference. If something changes
unexpectedly — a widget moves, a color shifts, a label disappears — the
test fails and saves a `_FAILED.png` so you can see what went wrong.

## How it works

1. The test renders your GUI headlessly (no window needed)
2. On first run, it saves the result as a reference PNG
3. On subsequent runs, it compares pixel-by-pixel against the reference
4. If pixels differ beyond the tolerance, the test fails

All reference PNGs live in `screenshots/` at the workspace root.
They're saved at 2x resolution with 144 DPI metadata so they look
correct on GitHub and in image viewers.

## Adding a screenshot test

### Built-in GUI

```rust
#[test]
fn gui_snapshot() {
    let params = Arc::new(MyParams::new());
    let plugin = MyPlugin::new(Arc::clone(&params));
    let layout = plugin.layout();
    truce_test::assert_gui_snapshot_grid::<MyParams>(
        "my_plugin_default", params, layout, 0,
    );
}
```

### egui

```rust
#[test]
fn gui_snapshot() {
    truce_egui::snapshot::assert_snapshot(
        "screenshots",
        "my_plugin_egui_default",
        WINDOW_W, WINDOW_H,  // use the same constants as your editor
        2.0,                  // scale (2.0 for Retina)
        0,                    // max pixel differences (0 = exact match)
        Some(truce_gui::font::JETBRAINS_MONO),
        |ctx, state| my_ui(ctx, state),
    );
}
```

### Iced

```rust
#[test]
fn gui_snapshot_iced() {
    let params = Arc::new(MyParams::new());
    let (pixels, w, h) = truce_iced::snapshot::render_iced_screenshot::<MyParams, MyEditor>(
        params,
        (WINDOW_W, WINDOW_H),
        2.0,
        Some(("JetBrains Mono", truce_gui::font::JETBRAINS_MONO)),
    );
    truce_test::assert_gui_snapshot_raw("my_plugin_iced_default", &pixels, w, h, 0);
}
```

### Slint

```rust
#[test]
fn gui_snapshot() {
    truce_slint::snapshot::assert_snapshot(
        "screenshots",
        "my_plugin_slint_default",
        WINDOW_W, WINDOW_H,
        2.0,
        0,
        |state| {
            let ui = MyPluginUi::new().unwrap();
            truce_slint::bind! { state, ui,
                P::Gain => gain,
            }
        },
    );
}
```

Slint uses a software renderer — no GPU needed. This makes Slint
snapshots fast and perfectly reproducible across machines.

## Keeping editor and snapshot sizes in sync

Define your window dimensions as constants and use them in both the
editor and the snapshot test. This prevents them from drifting apart:

```rust
const WINDOW_W: u32 = 176;
const WINDOW_H: u32 = 290;

// In custom_editor():
EguiEditor::new((WINDOW_W, WINDOW_H), my_ui)

// In the test:
truce_egui::snapshot::assert_snapshot(
    "screenshots", "my_plugin_default",
    WINDOW_W, WINDOW_H, 2.0, 0, None, |ctx, state| my_ui(ctx, state),
);
```

## Regenerating screenshots

When you intentionally change the UI, delete the old reference and
re-run the test:

```sh
# Regenerate one
rm screenshots/my_plugin_default.png
cargo test -p my-plugin -- gui_snapshot

# Regenerate all
rm screenshots/*.png
cargo test --workspace -- gui_snapshot
```

## Tolerance

The last argument (`max_diff`) controls how many bytes can differ. Use
`0` for exact match. If anti-aliasing differs between GPU drivers, bump
it to a small number like `100`.

When a test fails, a `_FAILED.png` is saved next to the reference so
you can open both and compare visually.

## Cross-OS behavior

The committed reference PNGs are owned by **one platform** — by
default, macOS. The rendering pipeline runs on every OS (Linux,
Windows, macOS), so screenshot tests double as smoke coverage that
the wgpu / Slint software renderer pipeline doesn't crash anywhere.
Comparison against the reference, however, is gated:

| Platform | Render | Compare | On diff |
|---|---|---|---|
| Reference (`macos` by default) | yes | yes | **fail the test**, save `*_FAILED.png` |
| Non-reference | yes | yes | log diff count, save `*_FAILED.png`, **pass** |

Why one platform owns the references: Metal, DX12, and Vulkan each
have their own anti-aliasing and text-rasterization quirks, so even
identical wgpu API calls produce slightly different bytes per
backend. Pixel-perfect cross-OS reference comparison would either
require software rendering everywhere or per-platform reference
trees. The current model keeps one canonical set of references and
treats per-platform diffs as informational.

### Choosing the reference platform

Override the default with the `TRUCE_SNAPSHOT_REFERENCE_OS`
environment variable. Valid values match `std::env::consts::OS`:
`macos`, `linux`, `windows`. For example, in a Linux-first CI:

```yaml
env:
  TRUCE_SNAPSHOT_REFERENCE_OS: linux
```

After flipping the reference, regenerate every PNG on the new
reference platform (`rm screenshots/*.png && cargo test --workspace
-- gui_snapshot`) so the saved bytes match what that platform
produces.

### Inspecting non-reference diffs

Even though non-reference platforms don't fail the test, they still
write `screenshots/*_FAILED.png` and print a line like:

```
[truce-egui] non-reference diff on linux: 1532 pixels differ vs
.../screenshots/gain_egui_default.png (informational; max allowed on
reference: 0). Saved current to .../screenshots/gain_egui_default_FAILED.png.
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
