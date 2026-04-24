//! Headless Slint snapshot rendering.
//!
//! Renders a Slint UI to an RGBA pixel buffer using the SoftwareRenderer.
//! No GPU or window needed — runs entirely in-process.

use std::fs;
use std::io::BufReader;
use std::path::Path;

use slint::platform::software_renderer::PremultipliedRgbaColor;
use slint::PhysicalSize;

use crate::param_state::ParamState;
use crate::platform;

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

    let params = std::sync::Arc::new(P::default_for_gui());
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

/// Render a Slint UI and compare against a reference PNG snapshot.
///
/// On first run (no reference exists), saves the reference and returns.
/// On subsequent runs, compares pixel-by-pixel and panics if the diff
/// exceeds `max_diff_pixels`.
pub fn assert_snapshot<P: truce_params::Params + 'static>(
    snapshot_dir: &str,
    name: &str,
    width: u32,
    height: u32,
    scale: f32,
    max_diff_pixels: usize,
    setup: impl FnOnce(ParamState) -> Box<dyn Fn(&ParamState)>,
) {
    let (pixels, width, height) = render_to_pixels::<P>(width, height, scale, setup);

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    // Walk up to workspace root (truce-slint → crates → root)
    let root = manifest_dir.parent().unwrap().parent().unwrap();
    let dir = root.join(snapshot_dir);
    fs::create_dir_all(&dir).ok();

    let ref_path = dir.join(format!("{name}.png"));

    if !ref_path.exists() {
        save_png(&ref_path, &pixels, width, height);
        eprintln!(
            "[truce-slint] Snapshot reference created: {}",
            ref_path.display()
        );
        return;
    }

    let (ref_pixels, ref_w, ref_h) = load_png(&ref_path);
    assert_eq!(
        (width, height),
        (ref_w, ref_h),
        "GUI size changed: current {width}x{height}, reference {ref_w}x{ref_h}. \
         Delete {} to regenerate.",
        ref_path.display()
    );

    let mut diff_count = 0usize;
    for (&current, &reference) in pixels.iter().zip(ref_pixels.iter()) {
        if current != reference {
            diff_count += 1;
        }
    }

    if diff_count > max_diff_pixels {
        let fail_path = dir.join(format!("{name}_FAILED.png"));
        save_png(&fail_path, &pixels, width, height);
        panic!(
            "GUI snapshot mismatch: {diff_count} pixels differ (max allowed: {max_diff_pixels}).\n\
             Reference: {}\n\
             Current:   {}\n\
             Delete the reference to regenerate.",
            ref_path.display(),
            fail_path.display(),
        );
    }
}

fn save_png(path: &Path, pixels: &[u8], w: u32, h: u32) {
    let file = fs::File::create(path)
        .unwrap_or_else(|e| panic!("Failed to create {}: {e}", path.display()));
    let mut encoder = png::Encoder::new(file, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(pixels).unwrap();
}

fn load_png(path: &Path) -> (Vec<u8>, u32, u32) {
    let file =
        fs::File::open(path).unwrap_or_else(|e| panic!("Failed to open {}: {e}", path.display()));
    let decoder = png::Decoder::new(BufReader::new(file));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    (buf, info.width, info.height)
}
