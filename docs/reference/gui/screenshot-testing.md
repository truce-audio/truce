# Screenshot Testing

Screenshot tests catch visual regressions by rendering your GUI to an
image and comparing it against a committed reference. If something
changes unexpectedly — a widget moves, a color shifts, a label
disappears — the test fails and points at the freshly-rendered PNG so
you can compare visually.

The test API is a builder constructed by the `screenshot!` macro,
which takes the plugin type and the explicit reference-PNG path:

```rust
#[test]
fn gui_screenshot() {
    truce_test::screenshot!(Plugin, "screenshots/default.png").run();
}
```

The path is resolved relative to your crate's `Cargo.toml` directory
(or used as-is if absolute). The macro never picks a path on your
behalf — every test names its own reference, in whatever directory
layout suits the project.

## Quick start

Add `truce-test` to `[dev-dependencies]`:

```toml
[dev-dependencies]
truce-test = { workspace = true }
```

Drop the test into your `lib.rs` (or wherever your `mod tests` lives):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_screenshot() {
        truce_test::screenshot!(Plugin, "screenshots/default.png").run();
    }
}
```

If the reference doesn't exist, the test fails and points at the
CLI command that creates one:

```
No screenshot baseline at /path/to/your-plugin/screenshots/default.png.
Create one with: cargo truce screenshot --out /path/to/.../screenshots/default.png
then inspect the rendered PNG and commit it.
```

The standard flow:

```sh
cargo truce screenshot --out screenshots/default.png   # writes the PNG
git add screenshots/                                   # eyeball + commit
cargo test                                             # the gate
```

`cargo truce screenshot` is decoupled from the test surface — it
works on any crate built with `truce::plugin!` whether or not
the codebase has a `gui_screenshot` test.

## How it works

1. `truce_test::screenshot!(Plugin, "<path>")` constructs a
   `ScreenshotTest<Plugin>` with the reference path resolved
   against the calling crate's `CARGO_MANIFEST_DIR` (when
   relative) or used as-is (when absolute).
2. `.run()` calls `Plugin::create()` (plus `init()` and any
   `setup` closure you supplied), then asks the editor for
   `(pixels, width, height)` via `Editor::screenshot()`.
3. The current render is always written to
   `<workspace>/target/screenshots/<basename>.png` —
   gitignored, so it never accidentally gets committed.
4. The committed reference is loaded from the path you passed.
5. If the reference doesn't exist, the test panics with a
   `cargo truce screenshot --out <path>` hint.
6. If the reference exists and the diff exceeds tolerance, the
   test fails with both PNG paths and the exact `cp` command to
   accept the new render as the new baseline.

References save at 2× resolution with 144 DPI metadata so they
look right on GitHub and in image viewers.

## State-dependent screenshots

The default test renders against `Plugin::create()` output (a
fresh plugin at default params). For shots that need a specific
configuration — a knob at a particular value, a panel toggled
open, a meter reading a particular level — the builder's
`setup` closure runs after `init()` and before the render:

```rust
#[test]
fn gui_screenshot_max_gain() {
    truce_test::screenshot!(Plugin, "screenshots/max_gain.png")
        .set_param(MyParamId::Gain, 1.0)
        .run();
}
```

The builder applies, in order, `state_file` (if any), `set_param`
shortcuts, then the `setup` closure — same lifecycle the audio
[`PluginDriver`](../audio-testing.md) uses, so the same vocabulary
works for both audio and GUI tests.

For more than one-shot param tweaks, drop down to the `setup`
closure (`&mut Plugin`):

- Drive `p.process(…)` to populate meters or animations.
- Mutate custom (non-param) state on the plugin struct.

For state you'd rather author interactively than spell out in
code, the standalone host's `Cmd+S` / `Ctrl+S` saves a
`.pluginstate` file. Load it via `state_file`:

```rust
#[test]
fn gui_screenshot_evening() {
    truce_test::screenshot!(Plugin, "screenshots/evening.png")
        .state_file("test_states/evening.pluginstate")
        .run();
}
```

`state_file` paths are crate-manifest-relative or absolute. The
`.pluginstate` blob just gets fed to `plugin.load_state(&bytes)` —
the same path CLAP / VST3 / AU hosts use to restore session state.

## Promoting a render

The test's failure log includes the exact `cp` command. A typical
flow:

```sh
# 1. Run the test. Either it fails (regression) or panics with the
#    "no baseline" message (test added but not yet baselined).
cargo test -p my-plugin gui_screenshot

# 2. Inspect target/screenshots/default.png. If it looks correct,
#    promote it (the cp command from the failure message):
cp target/screenshots/default.png screenshots/default.png

# 3. Commit the updated reference.
git add screenshots/default.png
```

`cargo truce screenshot --out <path>` is the faster path when
you want to write directly to the baseline location:

```sh
cargo truce screenshot --out screenshots/default.png
git diff screenshots/                # eyeball the visual change
git add screenshots/ && git commit
```

For state-dependent baselines, `--state` lets the CLI feed a
`.pluginstate` blob to the renderer:

```sh
cargo truce screenshot \
    --state cool.pluginstate \
    --out screenshots/cool.png
```

(The CLI can't run setup *closures* — those live in the test
binary, not the cdylib `cargo truce` dlopens. Use `cargo test
gui_screenshot_<name>` + `cp` for closure-driven baselines.)

## Tolerance

The default tolerance is 0 (strict pixel match). Bump it via
`.tolerance(n)` if anti-aliasing or font hinting introduces
flake:

```rust
truce_test::screenshot!(Plugin, "screenshots/default.png")
    .tolerance(200)
    .run();
```

Typical bumps are 50–500 pixels for cross-machine antialiasing
slack.

## Cross-OS rendering

Per-backend wgpu rasterization differs across GPU/OS combinations
(Metal / DX12 / Vulkan each have their own anti-aliasing and text
rasterization quirks). A reference PNG rendered on macOS won't be
pixel-identical when re-rendered on Linux or Windows even if every
parameter and shader is the same.

The framework doesn't try to paper over this — strict pixel match
on every host. If you intend to gate screenshots cross-platform,
you have two options:

**Option A — single reference platform.** Pick one (typically
your CI host); only run the test there. Skip it elsewhere with a
`cfg`:

```rust
#[cfg(target_os = "macos")]
#[test]
fn gui_screenshot() {
    truce_test::screenshot!(Plugin, "screenshots/default.png").run();
}
```

**Option B — per-platform references.** One test per OS, each
gated by `cfg(target_os = …)`, each with its own committed
reference:

```rust
#[cfg(target_os = "macos")]
#[test]
fn gui_screenshot_macos() {
    truce_test::screenshot!(Plugin, "screenshots/default_macos.png").run();
}

#[cfg(target_os = "linux")]
#[test]
fn gui_screenshot_linux() {
    truce_test::screenshot!(Plugin, "screenshots/default_linux.png").run();
}

#[cfg(target_os = "windows")]
#[test]
fn gui_screenshot_windows() {
    truce_test::screenshot!(Plugin, "screenshots/default_windows.png").run();
}
```

`cargo test` on each platform compiles only its variant; cross-OS
rasterizer drift can't fail the wrong test. The in-tree examples
(`examples/truce-example-*`) use this pattern.

## API reference

### `screenshot!` macro

```rust
truce_test::screenshot!($plugin:ty, $path:expr)
```

Constructs a `ScreenshotTest<$plugin>`. `$path` is the reference
PNG path; resolved against `CARGO_MANIFEST_DIR` when relative.
Both args are required — there's no zero-arg form, no
auto-derived path, no implicit directory.

### `ScreenshotTest<P>` builder

```rust
impl<P: PluginExport> ScreenshotTest<P> {
    pub fn state_file<S: Into<PathBuf>>(self, path: S) -> Self;
    pub fn set_param(self, id: impl Into<u32>, normalized: f32) -> Self;
    pub fn setup<F: FnOnce(&mut P) + 'static>(self, f: F) -> Self;
    pub fn tolerance(self, t: usize) -> Self;
    pub fn run(self);
}
```

| Method | Effect |
|---|---|
| `state_file("path")` | Load a `.pluginstate` blob (the standalone host's `Cmd+S` save format) via `plugin.load_state(&bytes)`. Applied first. |
| `set_param(id, v)` | Set a parameter to a normalized [0, 1] value via `params().set_normalized(id, v)`. Applied after state load. Multiple calls compose. |
| `setup(\|p\| …)` | Mutate the plugin between `P::create()` and the render. Drive `process()`, mutate custom state. Applied last. |
| `tolerance(n)` | Max allowed differing-pixel count. `0` = strict. |
| `run()` | Build, render, compare. |

### `Editor::screenshot` trait method

```rust
fn screenshot(
    &mut self,
    params: Arc<dyn truce_params::Params>,
) -> Option<(Vec<u8>, u32, u32)>;
```

Built-in backends (`truce-gpu`, `truce-egui`, `truce-iced`,
`truce-slint`) all implement this. Custom editor implementations
need to override it to be testable through `screenshot!`.

### `cargo truce screenshot`

Render a plugin's GUI from the command line, no `#[test]` required.
Fully self-contained — works on any crate built with
`truce::plugin!`.

```sh
cargo truce screenshot --out shots/hero.png            # one-off render
cargo truce screenshot -p my-plugin --out shots/a.png  # workspace mode
cargo truce screenshot --state s.pluginstate --out shots/cool.png
cargo truce screenshot --check --out screenshots/default.png   # CI gate
```

| Flag | Meaning |
|---|---|
| `--out <path>` (required) | Output path. CWD-relative or absolute. The CLI never picks a path for you. |
| `-p <crate>` | Plugin crate. Required when the project has multiple plugins (each gets its own `--out`). |
| `--state <path>` | Load a `.pluginstate` blob (the standalone host's `Cmd+S` save format) before rendering. CWD-relative or absolute. |
| `--check` | Diff against the existing baseline at `--out`; exit non-zero on regression. Strict pixel match. |
| `--debug` | Cargo dev profile (faster compile). Default is release. |

The CLI dlopens the plugin's cdylib and calls a hidden
`__truce_screenshot` symbol that `truce::plugin!` exports. No
per-plugin scaffolding required.

---

## Texture format gotchas

Each backend's `Editor::screenshot()` impl is responsible for
returning RGBA8 bytes; the comparator just does a pixel-byte
diff. The built-in backends already convert from their native
formats — the table below is for debugging color mismatches
when you're hand-rolling a renderer.

| Backend | Live format | Screenshot bytes returned |
|---------|------------|----------------------------|
| Built-in (`truce-gpu`) | Non-sRGB surface default | RGBA8 |
| egui (`truce-egui`) | `Rgba8UnormSrgb` | RGBA8 (sRGB) |
| Iced (`truce-iced`) | `Bgra8UnormSrgb` | RGBA8 (sRGB, swizzled) |
| Slint (`truce-slint`) | CPU pixels (premultiplied) | RGBA8 (un-premultiplied) |

Mismatches usually look like a uniform tint shift (everything
darker / lighter / wrong red-blue) — that's a sign the renderer
returns bytes in a format the comparator can't compare against
the reference.
