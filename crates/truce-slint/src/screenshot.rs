//! Headless Slint screenshot rendering.
//!
//! Renders a Slint UI to an RGBA pixel buffer using the SoftwareRenderer.
//! No GPU or window needed — runs entirely in-process.

use slint::platform::software_renderer::PremultipliedRgbaColor;
use slint::PhysicalSize;

use crate::param_state::ParamState;
use crate::platform;
use std::sync::Arc;

/// Render a Slint UI to RGBA pixels.
///
/// `setup` is the same closure passed to `SlintEditor::new` — it creates
/// the component and returns a sync function. The sync function is called
/// once before rendering so the UI reflects default param values.
///
/// `scale` is the DPI scale factor (2.0 for Retina). The returned pixel
/// buffer is `(width * scale) × (height * scale)` physical pixels.
pub fn render_to_pixels<P: truce_params::Params + 'static>(
    width: u32,
    height: u32,
    scale: f32,
    setup: impl FnOnce(ParamState) -> Box<dyn Fn(&ParamState)>,
) -> (Vec<u8>, u32, u32) {
    platform::ensure_platform();

    let phys_w = (width as f32 * scale) as u32;
    let phys_h = (height as f32 * scale) as u32;

    let window = platform::create_slint_window();
    window.set_size(slint::WindowSize::Physical(PhysicalSize::new(
        phys_w, phys_h,
    )));
    window.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
        scale_factor: scale,
    });

    let params = Arc::new(P::default_for_gui());
    let state = ParamState::from_params(params);
    let sync_fn = setup(state.clone());

    // Sync params so the UI shows default values.
    sync_fn(&state);

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
