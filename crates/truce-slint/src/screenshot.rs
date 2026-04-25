//! Headless Slint screenshot rendering.
//!
//! Renders a Slint UI to an RGBA pixel buffer using the SoftwareRenderer.
//! No GPU or window needed — runs entirely in-process. Driven by
//! `SlintEditor::screenshot()` (`Editor` trait impl in `editor.rs`),
//! which is itself called from `truce_test::assert_screenshot::<Plugin>(...)`.

use slint::platform::software_renderer::PremultipliedRgbaColor;
use slint::PhysicalSize;

use crate::param_state::ParamState;
use crate::platform;

/// Headless render path shared by `SlintEditor::screenshot()` and any
/// future ad-hoc callers in this crate. Kept `pub(crate)` — external
/// callers should go through the `Editor::screenshot()` trait.
pub(crate) fn render_with_state(
    state: &ParamState,
    size: (u32, u32),
    scale: f32,
    setup: impl FnOnce(&ParamState) -> Box<dyn Fn(&ParamState)>,
) -> (Vec<u8>, u32, u32) {
    platform::ensure_platform();

    let (width, height) = size;
    let phys_w = (width as f32 * scale) as u32;
    let phys_h = (height as f32 * scale) as u32;

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

    window.draw_if_needed(|renderer| {
        renderer.render(&mut px_buf, phys_w as usize);
    });

    // Convert premultiplied RGBA to straight RGBA bytes.
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for px in &px_buf {
        if px.alpha == 0 {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else if px.alpha == 255 {
            rgba.extend_from_slice(&[px.red, px.green, px.blue, 255]);
        } else {
            // Un-premultiply.
            let a = px.alpha as u16;
            rgba.push(((px.red as u16 * 255) / a).min(255) as u8);
            rgba.push(((px.green as u16 * 255) / a).min(255) as u8);
            rgba.push(((px.blue as u16 * 255) / a).min(255) as u8);
            rgba.push(px.alpha);
        }
    }

    (rgba, phys_w, phys_h)
}
