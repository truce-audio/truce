# Screenshot Testing

## How It Works

Screenshot tests render a plugin's GUI to an offscreen GPU texture,
save the result as a PNG, and compare pixel-by-pixel against a
reference image. On first run, the reference is created. On subsequent
runs, any difference beyond a tolerance threshold fails the test.

All screenshots live in `screenshots/` at the workspace root.
PNGs are saved at 2x (Retina) resolution with 144 DPI metadata so
they display at logical size in viewers and on GitHub.

## Backends

| Backend | Screenshot support | How |
|---------|-------------------|-----|
| Built-in (`truce-gui` + `truce-gpu`) | Yes | `WgpuBackend::headless()` → offscreen wgpu texture → pixel readback |
| egui (`truce-egui`) | Yes | Headless wgpu device → `egui_wgpu::Renderer` → pixel readback |
| Iced (`truce-iced`) | Yes | Headless wgpu device → `iced_wgpu::Renderer` → pixel readback |
| Slint (`truce-slint`) | Yes | `SoftwareRenderer` → pixel buffer (no GPU needed) |

## Writing a Screenshot Test

### Built-in GUI

```rust
#[test]
fn gui_screenshot() {
    let params = std::sync::Arc::new(MyParams::new());
    let plugin = MyPlugin::new(std::sync::Arc::clone(&params));
    let layout = plugin.layout();
    truce_test::assert_gui_snapshot_grid::<MyParams>(
        "my_plugin_default", params, layout, 0,
    );
}
```

`assert_gui_snapshot_grid` renders via `truce_gpu::snapshot::render_to_pixels`
(headless wgpu), then compares against `screenshots/my_plugin_default.png`.

### egui

```rust
#[test]
fn gui_screenshot() {
    truce_egui::snapshot::assert_snapshot(
        "screenshots",
        "my_plugin_egui_default",
        640, 480,
        2.0,  // pixels_per_point
        0,    // max_diff (0 = exact match)
        |ctx, state| my_ui(ctx, state),
    );
}
```

### Iced

Iced screenshots use `truce_iced::snapshot::render_iced_screenshot`
internally. See `crates/truce-iced/examples/gain-iced/src/lib.rs`
for a working example.

### Slint

```rust
#[test]
fn gui_screenshot() {
    truce_slint::snapshot::assert_snapshot(
        "screenshots",
        "my_plugin_slint_default",
        320, 150,
        2.0,  // scale (2.0 for Retina)
        0,    // max_diff
        |state| {
            let ui = MyPluginUi::new().unwrap();
            truce_slint::bind! { state, ui,
                P::Gain => gain,
            }
        },
    );
}
```

Slint snapshots use the `SoftwareRenderer` — no GPU or window required.
This makes them fast and deterministic across machines.

## Regenerating Screenshots

Delete a reference PNG and re-run the test:

```sh
rm screenshots/gain_default.png
cargo test -p truce-example-gain -- gui_screenshot
```

Or regenerate all:

```sh
rm screenshots/*.png
cargo test --workspace
```

## Texture Format Matching

Screenshots must use the same texture format as the windowed rendering
path to produce accurate colors:

| Backend | Windowed format | Screenshot format |
|---------|----------------|-------------------|
| Built-in (wgpu) | Non-sRGB (surface default) | `Rgba8Unorm` |
| egui | `Rgba8UnormSrgb` | `Rgba8UnormSrgb` |
| Iced | `Bgra8UnormSrgb` (Metal default) | `Bgra8UnormSrgb` |
| Slint | `PremultipliedRgbaColor` (CPU) | RGBA8 (un-premultiplied) |

Mismatched formats cause screenshots to appear darker or lighter
than what the DAW shows. If screenshots look wrong, check the format
in the snapshot code matches the editor's surface format.

## Tolerance

The `max_diff` parameter controls how many bytes can differ before
the test fails. Use `0` for exact match. Anti-aliasing differences
between GPU drivers may require a small tolerance (e.g., `100`).

When a test fails, a `_FAILED.png` is saved next to the reference
for visual comparison.
