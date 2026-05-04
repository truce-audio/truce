//! Headless Slint screenshot rendering.
//!
//! Renders a Slint UI to an RGBA pixel buffer using the `SoftwareRenderer`.
//! No GPU or window needed — runs entirely in-process. Driven by
//! `SlintEditor::screenshot()` (`Editor` trait impl in `editor.rs`),
//! which is itself called from `truce_test::assert_screenshot::<Plugin>(...)`.

use slint::PhysicalSize;
use slint::platform::software_renderer::PremultipliedRgbaColor;

use truce_core::editor::PluginContext;
use truce_params::Params;

use crate::editor::SyncFn;
use crate::platform;

/// Headless render path shared by `SlintEditor::screenshot()` and any
/// future ad-hoc callers in this crate. Kept `pub(crate)` — external
/// callers should go through the `Editor::screenshot()` trait.
///
/// Returns `None` when the Slint window's `draw_if_needed` reports no
/// draw happened — without that signal the buffer would contain
/// zero-alpha pixels and the screenshot diff would surface as a
/// confusing "blank vs reference" rather than the underlying "renderer
/// didn't run." Matches the peer pattern in `truce-egui` and `truce-iced`
/// where the underlying renderer is also `Option`-returning.
pub(crate) fn render_with_state<P: Params + ?Sized>(
    state: &PluginContext<P>,
    size: (u32, u32),
    scale: f32,
    setup: impl FnOnce(&PluginContext<P>) -> SyncFn<P>,
) -> Option<(Vec<u8>, u32, u32)> {
    platform::ensure_platform();

    let (width, height) = size;
    let phys_w = truce_gui::to_physical_px(width, f64::from(scale));
    let phys_h = truce_gui::to_physical_px(height, f64::from(scale));

    let window = platform::create_slint_window();
    window.set_size(slint::WindowSize::Physical(PhysicalSize::new(
        phys_w, phys_h,
    )));
    window.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
        scale_factor: scale,
    });

    let sync_fn = setup(state);

    // Sync params so the UI shows default values.
    sync_fn(state);

    // Render to pixel buffer.
    let pixel_count = (phys_w * phys_h) as usize;
    let mut px_buf = vec![PremultipliedRgbaColor::default(); pixel_count];

    let drew = window.draw_if_needed(|renderer| {
        renderer.render(&mut px_buf, phys_w as usize);
    });
    if !drew {
        return None;
    }

    // Convert premultiplied RGBA to straight RGBA bytes.
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for px in &px_buf {
        if px.alpha == 0 {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else if px.alpha == 255 {
            rgba.extend_from_slice(&[px.red, px.green, px.blue, 255]);
        } else {
            // Un-premultiply with round-to-nearest. The previous
            // truncating integer division (floor) made screenshots
            // 1-bit darker than `truce-gpu::WgpuBackend::read_pixels`,
            // which rounds — producing reference-PNG drift between
            // the two render paths.
            let a = u16::from(px.alpha);
            let half = a / 2;
            rgba.push(((u16::from(px.red) * 255 + half) / a).min(255) as u8);
            rgba.push(((u16::from(px.green) * 255 + half) / a).min(255) as u8);
            rgba.push(((u16::from(px.blue) * 255 + half) / a).min(255) as u8);
            rgba.push(px.alpha);
        }
    }

    Some((rgba, phys_w, phys_h))
}
