# Screenshot Testing

Screenshot tests catch visual regressions by rendering your GUI to an
image and comparing it against a committed reference. If something
changes unexpectedly — a widget moves, a color shifts, a label
disappears — the test fails and points at the freshly-rendered PNG so
you can compare visually.

The API is a builder you construct via the `screenshot!` macro:

```rust
#[test]
fn gui_screenshot() {
    truce_test::screenshot!(Plugin).run();
}
```

That's it. The macro reads `CARGO_PKG_NAME` + `CARGO_MANIFEST_DIR`
from the calling crate at compile time, so the test renders your
plugin's editor and compares against `screenshots/<crate>.png`
(relative to your `Cargo.toml`). No paths to coordinate, no
filename to invent.

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
        truce_test::screenshot!(Plugin).run();
    }
}
```

First run: there's no reference yet, so the test logs a `cp`-based
"promote" hint and **passes** (it doesn't fail — committing the
first reference is meant to be a deliberate step):

```
[truce-test] No reference at /path/to/your-plugin/screenshots/your-plugin.png.
Current render saved to /path/to/your-plugin/target/screenshots/your-plugin.png.
To promote: cp '/path/to/.../target/screenshots/your-plugin.png' '/path/to/.../screenshots/your-plugin.png'
```

Run that `cp`, commit the PNG, and from then on the test gates
regressions silently.

Faster path that doesn't go through `cargo test`:

```sh
cargo truce screenshot          # writes <crate>/screenshots/<crate>.png directly
```

`cargo truce screenshot` is fully decoupled from the test surface —
you can use it on any crate built with `truce::plugin!`, with or
without a `gui_screenshot` test in the codebase.

## How it works

1. `truce_test::screenshot!(Plugin)` constructs a
   `ScreenshotTest<Plugin>` builder anchored to the calling
   crate's manifest dir.
2. `.run()` calls `Plugin::create()` (plus `init()` and any
   `setup` closure you supplied), then asks the editor for
   `(pixels, width, height)` via `Editor::screenshot()`.
3. The current render is always written to
   `<workspace>/target/screenshots/<crate>.png` — gitignored, so
   it never accidentally gets committed.
4. The committed reference is loaded from
   `<crate>/screenshots/<crate>.png` (or whatever path you set
   via `.name()` / `.path()`).
5. If the reference doesn't exist yet, the test logs the `cp`
   hint and passes.
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
    truce_test::screenshot!(Plugin)
        .name("max_gain")
        .setup(|p| p.params().gain.set_normalized(1.0))
        .run();
}
```

`setup` gets `&mut Plugin`, so you can:

- Set parameter values directly (`p.params().<param>.set_normalized(…)`).
- Load saved state (`p.load_state(&bytes)`).
- Tick `p.process(…)` to populate meters or animations.

For state you'd rather author interactively than spell out in
code, the standalone host's `Cmd+S` / `Ctrl+S` saves a
`.pluginstate` file. Load it via `state_file`:

```rust
#[test]
fn gui_screenshot_evening() {
    truce_test::screenshot!(Plugin)
        .name("evening")
        .state_file("test_states/evening.pluginstate")
        .run();
}
```

`state_file` paths are crate-manifest-relative or absolute. The
`.pluginstate` blob just gets fed to `plugin.load_state(&bytes)` —
the same path CLAP / VST3 / AU hosts use to restore session state.

## Promoting a render

The test's failure / "no reference" log line includes the exact
`cp` command. A typical flow:

```sh
# 1. Run the test. Either it fails (regression) or logs a promote
#    hint (new test or accepted change).
cargo test -p my-plugin gui_screenshot

# 2. Inspect target/screenshots/my-plugin.png. If it looks correct,
#    promote it (the cp command from the log line):
cp target/screenshots/my-plugin.png screenshots/my-plugin.png

# 3. Commit the updated reference.
git add screenshots/my-plugin.png
```

`cargo truce screenshot` is the faster path when you just want
fresh PNGs. It writes directly to the baseline path, skipping the
`cp`:

```sh
cargo truce screenshot                # → <crate>/screenshots/<crate>.png
git diff screenshots/                 # eyeball the visual change
git add screenshots/ && git commit
```

For state-dependent baselines, `--state` lets the CLI feed a
`.pluginstate` blob to the renderer:

```sh
cargo truce screenshot --state cool.pluginstate --out screenshots/cool.png
```

(The CLI can't run setup *closures* — those live in the test
binary, not the cdylib `cargo truce` dlopens. Use `cargo test
gui_screenshot_<name>` + `cp` for closure-driven baselines.)

## Tolerance

The default tolerance is 0 (strict pixel match). Bump it via
`.tolerance(n)` if anti-aliasing or font hinting introduces
flake:

```rust
truce_test::screenshot!(Plugin).tolerance(200).run();
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
    truce_test::screenshot!(Plugin).run();
}
```

**Option B — per-platform references.** One test per OS, each
gated by `cfg(target_os = …)`, each with its own committed
reference:

```rust
#[cfg(target_os = "macos")]
#[test]
fn gui_screenshot_macos() {
    truce_test::screenshot!(Plugin).name("default_macos").run();
}

#[cfg(target_os = "linux")]
#[test]
fn gui_screenshot_linux() {
    truce_test::screenshot!(Plugin).name("default_linux").run();
}

#[cfg(target_os = "windows")]
#[test]
fn gui_screenshot_windows() {
    truce_test::screenshot!(Plugin).name("default_windows").run();
}
```

Each commits to `screenshots/default_<os>.png`. `cargo test` on
each platform compiles only its variant; cross-OS rasterizer
drift can't fail the wrong test. The in-tree examples
(`examples/truce-example-*`) use this pattern.

## API reference

### `screenshot!` macro

```rust
truce_test::screenshot!(Plugin)
```

Constructs a `ScreenshotTest<Plugin>` anchored to the calling
crate's manifest dir + `CARGO_PKG_NAME`. The plugin type is the
one `truce::plugin!` emits (`crate::Plugin`); pass an alternate
type if you need to.

### `ScreenshotTest<P>` builder

```rust
impl<P: PluginExport> ScreenshotTest<P> {
    pub fn setup<F: FnOnce(&mut P) + 'static>(self, f: F) -> Self;
    pub fn state_file<S: Into<PathBuf>>(self, path: S) -> Self;
    pub fn name<S: Into<String>>(self, name: S) -> Self;
    pub fn path<S: Into<PathBuf>>(self, path: S) -> Self;
    pub fn tolerance(self, t: usize) -> Self;
    pub fn run(self);
}
```

| Method | Effect |
|---|---|
| `setup(\|p\| …)` | Mutate the plugin between `P::create()` and the render. Set params, drive `process()`, load arbitrary state. |
| `state_file("path")` | Sugar for `setup(\|p\| p.load_state(&fs::read(path)?))`. Loads a `.pluginstate` blob written by the standalone host's `Cmd+S` / `Ctrl+S`. |
| `name("foo")` | Use the conventional `screenshots/` dir but a different filename: `<crate>/screenshots/foo.png`. |
| `path("dir/foo.png")` | Explicit path (crate-manifest-relative or absolute). |
| `tolerance(n)` | Max allowed differing-pixel count. `0` = strict. |
| `run()` | Build, render, compare. |

Path resolution:

| Form | Resolves to |
|---|---|
| (default) | `<crate>/screenshots/<CARGO_PKG_NAME>.png` |
| `.name("foo")` | `<crate>/screenshots/foo.png` |
| `.path("dir/foo.png")` (relative) | `<crate>/dir/foo.png` |
| `.path("/abs/path.png")` (absolute) | `/abs/path.png` |

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
cargo truce screenshot                                # default-state, default path
cargo truce screenshot -p my-plugin                   # workspace mode: pick a plugin
cargo truce screenshot --out shots/hero.png           # explicit output (CWD-relative)
cargo truce screenshot --name dark                    # → <crate>/screenshots/dark.png
cargo truce screenshot --state s.pluginstate          # load state before rendering
cargo truce screenshot --state s.pluginstate --out shots/cool.png
cargo truce screenshot --check                        # CI gate (no cargo test needed)
```

Output path:

| Flag | Resolves to |
|---|---|
| (default) | `<crate>/screenshots/<crate>.png` |
| `--name foo` | `<crate>/screenshots/foo.png` |
| `--out <path>` | `<path>` (CWD-relative or absolute) |

Other flags:

- `-p <crate>` — pick one plugin in workspace mode.
- `--state <path>` — load a `.pluginstate` blob (the file the
  standalone host's `Cmd+S` saves) before rendering.
  CWD-relative or absolute.
- `--check` — diff against the existing baseline; exit non-zero
  on regression. Strict pixel match.
- `--debug` — cargo dev profile (faster compile).

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
