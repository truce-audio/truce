//! Headless egui snapshot rendering for tests.
//!
//! Renders an egui UI to an offscreen wgpu texture and returns RGBA pixels,
//! or compares them against a reference PNG.

use crate::ParamState;
use std::sync::Arc;

/// Render an egui UI function to RGBA pixels using headless wgpu.
///
/// `width` and `height` are in logical points. The output pixel buffer
/// is `width * ppp × height * ppp` physical pixels.
///
/// Uses `P::default_for_gui()` to provide accurate parameter values
/// and formatting in the snapshot.
pub fn render_to_pixels<P: truce_params::Params + 'static>(
    width: u32,
    height: u32,
    pixels_per_point: f32,
    font: Option<&'static [u8]>,
    ui_fn: impl Fn(&egui::Context, &ParamState),
) -> Vec<u8> {
    let params = Arc::new(P::default_for_gui());
    let state = ParamState::from_params(params);
    let ctx = egui::Context::default();
    ctx.set_visuals(crate::theme::dark());

    if let Some(font_data) = font {
        crate::font::apply_font(&ctx, font_data);
    }

    // Run the egui frame
    let mut raw_input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(width as f32, height as f32),
        )),
        time: Some(0.0),
        focused: true,
        ..Default::default()
    };
    raw_input
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .native_pixels_per_point = Some(pixels_per_point);

    let output = ctx.run(raw_input, |ctx| {
        ui_fn(ctx, &state);
    });

    let clipped_primitives = ctx.tessellate(output.shapes, output.pixels_per_point);

    // Headless wgpu rendering
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..Default::default()
    });

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .expect("no wgpu adapter for snapshot rendering");

    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("truce-egui-snapshot"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            memory_hints: wgpu::MemoryHints::Performance,
        },
        None,
    ))
    .expect("failed to create wgpu device for snapshot");

    let format = wgpu::TextureFormat::Rgba8UnormSrgb;

    // Physical pixel dimensions
    let phys_w = (width as f32 * pixels_per_point) as u32;
    let phys_h = (height as f32 * pixels_per_point) as u32;

    // Render target at physical resolution
    let target_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("snapshot-target"),
        size: wgpu::Extent3d {
            width: phys_w,
            height: phys_h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target_tex.create_view(&wgpu::TextureViewDescriptor::default());

    // egui-wgpu renderer
    let mut egui_renderer = egui_wgpu::Renderer::new(&device, format, None, 1, false);

    // Update textures
    for (id, delta) in &output.textures_delta.set {
        egui_renderer.update_texture(&device, &queue, *id, delta);
    }

    let screen_desc = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [phys_w, phys_h],
        pixels_per_point: output.pixels_per_point,
    };

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("snapshot-frame"),
    });

    egui_renderer.update_buffers(
        &device,
        &queue,
        &mut encoder,
        &clipped_primitives,
        &screen_desc,
    );

    {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("snapshot-egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.12,
                            g: 0.12,
                            b: 0.14,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            })
            .forget_lifetime();

        egui_renderer.render(&mut pass, &clipped_primitives, &screen_desc);
    }

    // Copy texture to buffer for readback
    let bytes_per_row = (phys_w * 4 + 255) & !255; // align to 256
    let readback_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("snapshot-readback"),
        size: (bytes_per_row * phys_h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target_tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback_buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(phys_h),
            },
        },
        wgpu::Extent3d {
            width: phys_w,
            height: phys_h,
            depth_or_array_layers: 1,
        },
    );

    queue.submit(std::iter::once(encoder.finish()));

    // Map and read pixels
    let buf_slice = readback_buf.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    buf_slice.map_async(wgpu::MapMode::Read, move |result| {
        tx.send(result).unwrap();
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv().unwrap().expect("buffer map failed");

    let mapped = buf_slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((phys_w * phys_h * 4) as usize);
    for row in 0..phys_h {
        let start = (row * bytes_per_row) as usize;
        let end = start + (phys_w * 4) as usize;
        pixels.extend_from_slice(&mapped[start..end]);
    }
    drop(mapped);
    readback_buf.unmap();

    // Free textures
    for id in &output.textures_delta.free {
        egui_renderer.free_texture(id);
    }

    pixels
}

/// Render an egui UI and compare against a reference PNG snapshot.
///
/// On first run (no reference exists), saves the reference and returns.
/// On subsequent runs, compares pixel-by-pixel and panics if the diff
/// exceeds `max_diff_pixels`.
///
/// The snapshot directory is resolved relative to the workspace root.
#[allow(clippy::too_many_arguments)]
pub fn assert_snapshot<P: truce_params::Params + 'static>(
    snapshot_dir: &str,
    name: &str,
    width: u32,
    height: u32,
    pixels_per_point: f32,
    max_diff_pixels: usize,
    font: Option<&'static [u8]>,
    ui_fn: impl Fn(&egui::Context, &ParamState),
) {
    let pixels = render_to_pixels::<P>(width, height, pixels_per_point, font, ui_fn);
    let phys_w = (width as f32 * pixels_per_point) as u32;
    let phys_h = (height as f32 * pixels_per_point) as u32;

    // Resolve snapshot directory from workspace root
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // Walk up to workspace root (truce-egui → crates → root)
    let root = manifest_dir.parent().unwrap().parent().unwrap();
    let dir = root.join(snapshot_dir);
    std::fs::create_dir_all(&dir).ok();

    let ref_path = dir.join(format!("{name}.png"));

    if !ref_path.exists() {
        save_png(&ref_path, &pixels, phys_w, phys_h);
        eprintln!(
            "[truce-egui] Snapshot reference created: {}",
            ref_path.display()
        );
        return;
    }

    let (ref_pixels, ref_w, ref_h) = load_png(&ref_path);
    assert_eq!(
        (phys_w, phys_h),
        (ref_w, ref_h),
        "GUI size changed: current {phys_w}x{phys_h}, reference {ref_w}x{ref_h}. \
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
        save_png(&fail_path, &pixels, phys_w, phys_h);
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

fn save_png(path: &std::path::Path, pixels: &[u8], w: u32, h: u32) {
    let file = std::fs::File::create(path)
        .unwrap_or_else(|e| panic!("Failed to create {}: {e}", path.display()));
    let mut encoder = png::Encoder::new(file, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .unwrap_or_else(|e| panic!("Failed to write PNG header: {e}"));
    writer
        .write_image_data(pixels)
        .unwrap_or_else(|e| panic!("Failed to write PNG data: {e}"));
}

fn load_png(path: &std::path::Path) -> (Vec<u8>, u32, u32) {
    let file = std::fs::File::open(path)
        .unwrap_or_else(|e| panic!("Failed to open {}: {e}", path.display()));
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .unwrap_or_else(|e| panic!("Failed to read PNG info: {e}"));
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
    let info = reader
        .next_frame(&mut buf)
        .unwrap_or_else(|e| panic!("Failed to decode PNG frame: {e}"));
    buf.truncate(info.buffer_size());
    (buf, info.width, info.height)
}
