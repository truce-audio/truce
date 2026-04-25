//! Headless egui screenshot rendering for tests.
//!
//! Renders an egui UI to an offscreen wgpu texture and returns RGBA pixels.
//! Driven by `EguiEditor::screenshot()` (`Editor` trait impl in
//! `editor.rs`), which is itself called from
//! `truce_test::assert_screenshot::<Plugin>(...)`.

use crate::ParamState;

/// Headless render path shared by `EguiEditor::screenshot()` and any
/// future ad-hoc callers in this crate. Kept `pub(crate)` — external
/// callers should go through the `Editor::screenshot()` trait.
pub(crate) fn render_with_state(
    state: &ParamState,
    size: (u32, u32),
    pixels_per_point: f32,
    font: Option<&'static [u8]>,
    visuals: Option<egui::Visuals>,
    ui_fn: impl Fn(&egui::Context, &ParamState),
) -> (Vec<u8>, u32, u32) {
    let (width, height) = size;
    let ctx = egui::Context::default();
    ctx.set_visuals(visuals.unwrap_or_else(crate::theme::dark));

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
        ui_fn(ctx, state);
    });

    let clipped_primitives = ctx.tessellate(output.shapes, output.pixels_per_point);

    // Headless wgpu rendering. `PRIMARY` picks the platform-default
    // backend (Metal on macOS, DX12 on Windows, Vulkan on Linux) so
    // the screenshot pipeline runs everywhere. Per-backend rasterization
    // differences mean the rendered pixels won't byte-match across
    // platforms — `assert_screenshot` only enforces the comparison on
    // the reference platform (see `is_reference_platform`).
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .expect("no wgpu adapter for screenshot rendering");

    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("truce-egui-screenshot"),
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
        label: Some("screenshot-target"),
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
        label: Some("screenshot-frame"),
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
                label: Some("screenshot-egui"),
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
        label: Some("screenshot-readback"),
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

    (pixels, phys_w, phys_h)
}
