//! Headless GUI rendering for plugins.
//!
//! Drives a fresh plugin instance through `Editor::screenshot()` and
//! saves the resulting RGBA bytes as a PNG. No comparison, no
//! platform gating - that machinery lives in `truce-test`.
//!
//! Useful both as the render half of `truce_test::assert_screenshot`
//! and as a standalone capture path for `cargo truce screenshot` and
//! similar tooling that needs to produce a PNG without running through
//! the test harness.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::export::PluginExport;

/// Default scale factor used by all screenshot rendering when no
/// explicit override is supplied. Pinned to a `HiDPI` value so a
/// reference PNG baked on one host (CI runner, dev machine, headless
/// container) renders at the same physical dimensions on any other.
/// Without a pin, `truce_gui::backing_scale()` would return whatever
/// the host's main-screen DPI happens to be - 1.0 on a virtualized
/// CI runner, 2.0 on a Retina `MacBook` - and reference PNGs would
/// mismatch across machines.
pub const DEFAULT_SCREENSHOT_SCALE: f64 = 2.0;

/// Process-wide screenshot scale override (f64 bits). Zero means "no
/// override active"; any non-zero value is consulted by
/// `truce_gui::backing_scale()` ahead of the platform query so the
/// editor backends construct their `EditorScale` at the override.
static SCREENSHOT_SCALE_BITS: AtomicU64 = AtomicU64::new(0);

/// Read the active screenshot-scale override, if any. `truce-gui`
/// consults this from `backing_scale()` before falling back to the
/// per-OS main-screen DPI query.
#[must_use]
pub fn override_scale() -> Option<f64> {
    let bits = SCREENSHOT_SCALE_BITS.load(Ordering::Relaxed);
    if bits == 0 {
        return None;
    }
    let v = f64::from_bits(bits);
    (v.is_finite() && v > 0.0).then_some(v)
}

/// Scoped screenshot-scale override. Drop clears the override so live
/// (non-screenshot) editor construction afterwards goes back to the
/// host's main-screen DPI. Panic-safe: the override is cleared even
/// if rendering panics partway through.
struct ScreenshotScaleGuard;

impl ScreenshotScaleGuard {
    fn set(scale: f64) -> Self {
        let bits = if scale.is_finite() && scale > 0.0 {
            scale.to_bits()
        } else {
            0
        };
        SCREENSHOT_SCALE_BITS.store(bits, Ordering::Relaxed);
        Self
    }
}

impl Drop for ScreenshotScaleGuard {
    fn drop(&mut self) {
        SCREENSHOT_SCALE_BITS.store(0, Ordering::Relaxed);
    }
}

/// Drive a fresh plugin through `Editor::screenshot()` at the
/// default screenshot scale. See [`render_pixels_for_at_scale`] for
/// an explicit-scale entry point.
#[must_use]
pub fn render_pixels<P: PluginExport>() -> (Vec<u8>, u32, u32) {
    let mut plugin = P::create();
    plugin.init();
    render_pixels_for::<P>(&mut plugin)
}

/// Construct `P`, optionally apply a saved-state blob (`.pluginstate`
/// bytes), then render at the default screenshot scale. Used by the
/// `__truce_screenshot` FFI so `cargo truce screenshot --state` can
/// capture the editor under arbitrary pre-saved state without needing
/// a test harness.
#[must_use]
pub fn render_with_state<P: PluginExport>(state: Option<&[u8]>) -> (Vec<u8>, u32, u32) {
    render_with_state_at_scale::<P>(state, DEFAULT_SCREENSHOT_SCALE)
}

/// `render_with_state` variant that pins the render to an explicit
/// scale. Plumbed through the `__truce_screenshot` FFI so the CLI's
/// `--scale` flag and `ScreenshotTest::scale` can both reach the
/// editor's construction-time `EditorScale`.
#[must_use]
pub fn render_with_state_at_scale<P: PluginExport>(
    state: Option<&[u8]>,
    scale: f64,
) -> (Vec<u8>, u32, u32) {
    let mut plugin = P::create();
    plugin.init();
    if let Some(bytes) = state
        && let Err(e) = plugin.load_state(bytes)
    {
        eprintln!("truce: screenshot load_state failed: {e}");
    }
    render_pixels_for_at_scale::<P>(&mut plugin, scale)
}

/// Render the given (already-mutated) plugin's editor at the
/// default screenshot scale. See [`render_pixels_for_at_scale`] for
/// an explicit-scale entry point.
///
/// # Panics
///
/// Panics if `PluginExport::editor()` returns `None` or the editor's
/// `screenshot()` method returns `None`. Both panics name the
/// concrete `P` and point at the trait method to implement.
pub fn render_pixels_for<P: PluginExport>(plugin: &mut P) -> (Vec<u8>, u32, u32) {
    render_pixels_for_at_scale::<P>(plugin, DEFAULT_SCREENSHOT_SCALE)
}

/// Render the given (already-mutated) plugin's editor at an explicit
/// scale. The scale is published via [`override_scale`] for the
/// duration of the call so the editor's `EditorScale` (initialized
/// from `truce_gui::backing_scale()` during `PluginExport::editor()`)
/// picks up the screenshot value rather than the host's main-screen
/// DPI. The override is cleared on return - including on panic - so
/// any subsequent live-editor construction sees the regular
/// platform scale.
///
/// Lets callers prepare plugin state - set params, load a state
/// blob, drive a `process()` block to populate meters - before the
/// editor renders. The `truce-test` `ScreenshotTest::setup` /
/// `state_file` paths and the `cargo truce screenshot --state` flag
/// both ride on this entry point.
///
/// # Panics
///
/// Panics if `PluginExport::editor()` returns `None` or the editor's
/// `screenshot()` method returns `None`. Both panics name the
/// concrete `P` and point at the trait method to implement.
pub fn render_pixels_for_at_scale<P: PluginExport>(
    plugin: &mut P,
    scale: f64,
) -> (Vec<u8>, u32, u32) {
    let _scale_guard = ScreenshotScaleGuard::set(scale);
    let mut editor = plugin.editor_builder()(plugin.params_arc()).unwrap_or_else(|| {
        panic!(
            "plugin {} returned no editor: PluginExport::editor_builder() returned None. \
             Implement `fn editor(params)` on your plugin (or one of \
             the built-in editor wrappers - truce-gpu / truce-egui / \
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

/// Read an RGBA PNG from disk. Panics on I/O / decode error - the
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
