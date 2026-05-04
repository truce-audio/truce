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

use std::path::Path;
use std::sync::Arc;

use crate::export::PluginExport;
use crate::plugin::Plugin;

/// Drive a fresh plugin through `Editor::screenshot()` and return raw
/// RGBA pixels + physical dimensions. No PNG save.
#[must_use] 
pub fn render_pixels<P: PluginExport>() -> (Vec<u8>, u32, u32) {
    let mut plugin = P::create();
    plugin.init();
    render_pixels_for::<P>(&mut plugin)
}

/// Construct `P`, optionally apply a saved-state blob (`.pluginstate`
/// bytes), then render. Used by the `__truce_screenshot` FFI so
/// `cargo truce screenshot --state` can capture the editor under
/// arbitrary pre-saved state without needing a test harness.
#[must_use] 
pub fn render_with_state<P: PluginExport>(state: Option<&[u8]>) -> (Vec<u8>, u32, u32) {
    let mut plugin = P::create();
    plugin.init();
    if let Some(bytes) = state {
        plugin.load_state(bytes);
    }
    render_pixels_for::<P>(&mut plugin)
}

/// Render the given (already-mutated) plugin's editor.
///
/// Lets callers prepare plugin state — set params, load a state
/// blob, drive a `process()` block to populate meters — before the
/// editor renders. The `truce-test` `ScreenshotTest::setup` /
/// `state_file` paths and the `cargo truce screenshot --state` flag
/// both ride on this entry point.
///
/// # Panics
///
/// Panics if `Plugin::editor()` returns `None` or the editor's
/// `screenshot()` method returns `None`. Both panics name the
/// concrete `P` and point at the trait method to implement.
pub fn render_pixels_for<P: PluginExport>(plugin: &mut P) -> (Vec<u8>, u32, u32) {
    let mut editor = <P as Plugin>::editor(plugin).unwrap_or_else(|| {
        panic!(
            "plugin {} returned no editor: Plugin::editor() returned None. \
             Implement `fn editor(&mut self)` on your plugin (or one of \
             the built-in editor wrappers — truce-gpu / truce-egui / \
             truce-iced / truce-slint) so screenshot rendering has \
             something to draw.",
            std::any::type_name::<P>()
        )
    });
    // `PluginExport::Params` is the concrete params type the
    // `plugin!` macro wired up. Hand the editor the live params Arc
    // (so any state we pre-loaded into `plugin` flows through), erased
    // to the dyn `Params` trait the editor expects.
    let params: Arc<dyn truce_params::Params> = plugin.params_arc();
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

/// Read an RGBA PNG from disk. Panics on I/O / decode error — the
/// caller (test or CLI) is expected to surface a meaningful message
/// in that case, so a crash here is sufficient for callers that
/// already failed `Path::exists()`.
///
/// # Panics
///
/// Panics if the file cannot be opened, the PNG header / info block
/// cannot be parsed, or the frame fails to decode. All panics include
/// the underlying error string and (where available) the path.
#[must_use]
pub fn load_png(path: &Path) -> (Vec<u8>, u32, u32) {
    let file = std::fs::File::open(path)
        .unwrap_or_else(|e| panic!("Failed to open {}: {e}", path.display()));
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .unwrap_or_else(|e| panic!("Failed to read PNG info: {e}"));
    // `output_buffer_size()` is `None` for some interlaced PNGs the
    // png crate refuses to size up-front. Fall back to a generous
    // worst-case estimate (4 bytes/pixel × declared dimensions); the
    // subsequent `next_frame` will surface any real decode error.
    let info = reader.info();
    let buf_size = reader
        .output_buffer_size()
        .unwrap_or_else(|| (info.width as usize) * (info.height as usize) * 4);
    let mut buf = vec![0u8; buf_size];
    let info = reader
        .next_frame(&mut buf)
        .unwrap_or_else(|e| panic!("Failed to decode PNG frame: {e}"));
    buf.truncate(info.buffer_size());
    (buf, info.width, info.height)
}

/// Write RGBA bytes to a PNG with 144 DPI metadata so the file
/// renders at half pixel size in viewers and on GitHub.
///
/// # Panics
///
/// Panics if the file cannot be created, the PNG header cannot be
/// written, or the encoder rejects `pixels` (typically a length
/// mismatch versus `w * h * 4`).
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
