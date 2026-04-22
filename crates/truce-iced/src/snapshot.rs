//! Offscreen iced rendering for snapshot tests.
//!
//! Creates a headless wgpu device, renders the iced UI to an offscreen
//! texture, and reads back RGBA pixel data. Used by snapshot tests to
//! generate and compare reference PNGs.

use std::sync::Arc;

use iced::{Color, Size};
use iced_graphics;
use iced_wgpu::wgpu;
use truce_params::Params;

use crate::editor::{IcedPlugin, IcedProgram};
use crate::param_message::Message;
use crate::param_state::ParamState;
use crate::editor_handle::EditorHandle;
use truce_core::editor::EditorContext;

/// Render an iced plugin UI offscreen and return RGBA pixel data.
///
/// Creates a headless wgpu device (no window/surface needed), builds the
/// iced program state, renders one frame to an offscreen texture, and
/// reads back the pixels.
pub fn render_iced_screenshot<P, M>(
    params: Arc<P>,
    size: (u32, u32),
    scale: f64,
    font: Option<(&'static str, &'static [u8])>,
) -> (Vec<u8>, u32, u32)
where
    P: Params + 'static,
    M: IcedPlugin<P>,
{
    let w = (size.0 as f64 * scale) as u32;
    let h = (size.1 as f64 * scale) as u32;

    // Create headless wgpu device
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..Default::default()
    });

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        compatible_surface: None, // headless
        force_fallback_adapter: false,
    }))
    .expect("No GPU adapter available for snapshot rendering");

    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("truce-iced-snapshot"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
        },
        None,
    ))
    .expect("Failed to create wgpu device for snapshot");

    // Use sRGB to match the windowed Metal surface (Bgra8UnormSrgb).
    let format = wgpu::TextureFormat::Bgra8UnormSrgb;

    // Create iced engine + renderer (MSAA 4x for smooth edges)
    let engine = iced_wgpu::Engine::new(
        &adapter, &device, &queue, format,
        Some(iced_graphics::Antialiasing::MSAAx4),
    );

    let default_font = if let Some((family, data)) = font {
        crate::font::apply_font(family, data)
    } else {
        iced::Font::DEFAULT
    };
    let mut renderer = iced_wgpu::Renderer::new(
        &device,
        &engine,
        default_font,
        iced::Pixels(14.0),
    );

    // Build the iced program
    let mut param_state = ParamState::new(params.clone());
    param_state.set_font(default_font);
    let noop_ctx = EditorContext {
        begin_edit: Arc::new(|_| {}),
        set_param: Arc::new(|_, _| {}),
        end_edit: Arc::new(|_| {}),
        request_resize: Arc::new(|_, _| false),
        get_param: {
            let p = params.clone();
            Arc::new(move |id| p.get_normalized(id).unwrap_or(0.0))
        },
        get_param_plain: {
            let p = params.clone();
            Arc::new(move |id| p.get_plain(id).unwrap_or(0.0))
        },
        format_param: {
            let p = params.clone();
            Arc::new(move |id| {
                let v = p.get_plain(id).unwrap_or(0.0);
                p.format_value(id, v).unwrap_or_default()
            })
        },
        get_meter: Arc::new(|_| 0.0),
        get_state: Arc::new(|| Vec::new()),
        set_state: Arc::new(|_| {}),
        transport: Arc::new(|| None),
    };
    let editor_handle = EditorHandle::new(noop_ctx);

    let plugin = M::new(params);
    let program = IcedProgram {
        plugin,
        param_state,
        editor_handle,
        meter_ids: Vec::new(),
    };

    let viewport = iced_graphics::Viewport::with_physical_size(Size::new(w, h), scale);
    let mut debug = iced_runtime::Debug::new();
    let theme = program.plugin.theme();

    let mut state = iced_runtime::program::State::new(
        program,
        viewport.logical_size(),
        &mut renderer,
        &mut debug,
    );

    // Run one update cycle to build the UI
    state.queue_message(Message::Tick);
    let style = iced_runtime::core::renderer::Style {
        text_color: Color::from_rgb(0.90, 0.90, 0.92),
    };
    let cursor = iced::mouse::Cursor::Available(iced::Point::new(-1.0, -1.0));
    let _ = state.update(
        viewport.logical_size(),
        cursor,
        &mut renderer,
        &theme,
        &style,
        &mut iced_runtime::core::clipboard::Null,
        &mut debug,
    );

    // Create offscreen texture
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("truce-iced-snapshot-tex"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let tex_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let bg = crate::theme::truce_dark_theme().palette().background;

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("truce-iced-snapshot-enc"),
    });

    let mut engine = engine;
    renderer.present(
        &mut engine,
        &device,
        &queue,
        &mut encoder,
        Some(bg),
        format,
        &tex_view,
        &viewport,
        &[] as &[String],
    );

    // Copy texture to readable buffer
    let bytes_per_row = w * 4;
    // wgpu requires rows aligned to 256 bytes
    let padded_bytes_per_row = (bytes_per_row + 255) & !255;

    let readback_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("truce-iced-snapshot-buf"),
        size: (padded_bytes_per_row * h) as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    encoder.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::ImageCopyBuffer {
            buffer: &readback_buffer,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );

    engine.submit(&queue, encoder);

    // Map and read pixels
    let slice = readback_buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        tx.send(result).ok();
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv()
        .expect("GPU readback channel closed")
        .expect("GPU readback failed");

    let mapped = slice.get_mapped_range();

    // Convert BGRA → RGBA and remove row padding
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for row in 0..h {
        let start = (row * padded_bytes_per_row) as usize;
        let end = start + (w * 4) as usize;
        for pixel in mapped[start..end].chunks_exact(4) {
            // BGRA → RGBA
            rgba.push(pixel[2]); // R
            rgba.push(pixel[1]); // G
            rgba.push(pixel[0]); // B
            rgba.push(pixel[3]); // A
        }
    }

    (rgba, w, h)
}
