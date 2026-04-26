//! Headless GUI rendering for plugins.
//!
//! Drives a fresh plugin instance through `Editor::screenshot()` and
//! saves the resulting RGBA bytes as a PNG. No comparison, no
//! platform gating — that machinery lives in `truce-test`.
//!
//! Useful both as the render half of `truce_test::assert_screenshot`
//! and as a standalone capture path for `cargo truce screenshot` and
//! similar tooling that needs to produce a PNG without running through
//! the test harness.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::export::PluginExport;
use crate::plugin::Plugin;

/// Default screenshot directory: `target/screenshots/` under the
/// workspace root. Gitignored (covered by `/target`).
pub const DEFAULT_SCREENSHOT_DIR: &str = "target/screenshots";

/// Render a plugin's editor headlessly and save the PNG.
///
/// Constructs a fresh `P` via `P::create()`, asks it for its editor,
/// drives `Editor::screenshot()` to get RGBA pixels, and writes
/// `<workspace>/target/screenshots/<name>.png`. Returns the path.
///
/// # Example
/// ```ignore
/// let path = truce_core::screenshot::render::<Plugin>("gain_dark");
/// println!("rendered to {}", path.display());
/// ```
pub fn render<P: PluginExport>(name: &str) -> PathBuf {
    let (pixels, w, h) = render_pixels::<P>();
    let dir = workspace_screenshot_dir(DEFAULT_SCREENSHOT_DIR);
    let path = dir.join(format!("{name}.png"));
    save_png(&path, &pixels, w, h);
    path
}

/// Drive a fresh plugin through `Editor::screenshot()` and return raw
/// RGBA pixels + physical dimensions. No PNG save.
///
/// Used by [`render`] (which goes on to save a file) and by
/// `truce_test::assert_screenshot` (which goes on to compare against
/// a reference).
pub fn render_pixels<P: PluginExport>() -> (Vec<u8>, u32, u32) {
    let mut plugin = P::create();
    plugin.init();
    let mut editor = <P as Plugin>::editor(&mut plugin)
        .expect("plugin returned no editor: PluginLogic::custom_editor() returned None and layout() was empty");
    // `PluginExport::Params` is the concrete params type the
    // `plugin!` macro wired up. We hand the editor a fresh dyn-erased
    // instance so each backend can build its own ParamState without
    // needing to know `P` at construction.
    let params: Arc<dyn truce_params::Params> =
        Arc::new(<P::Params as truce_params::Params>::new());
    editor.screenshot(params).unwrap_or_else(|| {
        panic!(
            "editor for {} returned None from Editor::screenshot(). \
             If this is a custom editor implementation, override \
             Editor::screenshot() to return RGBA pixels. Built-in \
             backends (truce-gpu / truce-egui / truce-iced / \
             truce-slint) all implement it.",
            std::any::type_name::<P>()
        )
    })
}

/// Resolve `<project_root>/<rel>/`, creating the directory if
/// needed. Walks up from the current working directory looking for a
/// `Cargo.toml`; prefers a manifest containing `[workspace]` (so
/// workspace members write to the shared root), and falls back to
/// the topmost package manifest for single-crate projects.
///
/// Uses runtime `cwd` rather than the compile-time
/// `CARGO_MANIFEST_DIR` of this crate — the latter would resolve to
/// the cargo git checkout when truce is consumed as a git dep, and
/// screenshots would land in `~/.cargo/git/checkouts/...` instead of
/// the user's project.
///
/// Pass [`DEFAULT_SCREENSHOT_DIR`] for the default location, or any
/// other path (e.g. `"examples/screenshots"` for an in-tree reference
/// directory).
pub fn workspace_screenshot_dir(rel: &str) -> PathBuf {
    let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dir = start.clone();
    let mut topmost_package: Option<PathBuf> = None;
    loop {
        let toml = dir.join("Cargo.toml");
        if toml.exists() {
            if let Ok(s) = std::fs::read_to_string(&toml) {
                if s.contains("[workspace]") {
                    // Workspace root — workspace members all share this dir.
                    let snap = dir.join(rel);
                    std::fs::create_dir_all(&snap).ok();
                    return snap;
                }
                // Track the highest-up package manifest as a fallback for
                // single-crate projects (no `[workspace]` anywhere up-tree).
                topmost_package = Some(dir.clone());
            }
        }
        if !dir.pop() {
            let chosen = topmost_package.unwrap_or(start);
            let snap = chosen.join(rel);
            std::fs::create_dir_all(&snap).ok();
            return snap;
        }
    }
}

/// Write RGBA bytes to a PNG with 144 DPI metadata so the file
/// renders at half pixel size in viewers and on GitHub.
pub fn save_png(path: &Path, pixels: &[u8], w: u32, h: u32) {
    let file = std::fs::File::create(path)
        .unwrap_or_else(|e| panic!("Failed to create {}: {e}", path.display()));
    let mut encoder = png::Encoder::new(file, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_pixel_dims(Some(png::PixelDimensions {
        xppu: 5669, // 144 DPI in pixels per meter
        yppu: 5669,
        unit: png::Unit::Meter,
    }));
    let mut writer = encoder
        .write_header()
        .unwrap_or_else(|e| panic!("Failed to write PNG header: {e}"));
    writer
        .write_image_data(pixels)
        .unwrap_or_else(|e| panic!("Failed to write PNG data: {e}"));
}
