//! Offscreen iced rendering for screenshot tests.
//!
//! Creates a headless wgpu device, drives one iced frame against an
//! offscreen target via `iced_wgpu::Renderer`, and reads back RGBA
//! pixel data. Used by screenshot tests to generate and compare
//! reference PNGs.

use std::sync::Arc;

use iced::{Color, Size};
use iced_wgpu::wgpu;
use truce_params::Params;

use crate::editor::{IcedPlugin, IcedProgram};
use crate::param_cache::ParamCache;
use crate::param_message::Message;
use truce_core::editor::for_test_params;

/// Render an iced plugin UI offscreen and return RGBA pixel data.
///
/// Creates a headless wgpu device (no window/surface needed), builds
/// the iced program, drives one frame via `UserInterface`, then asks
/// `iced_wgpu::Renderer::screenshot` to render and read back the
/// pixels.
///
/// Internal entry point for the headless screenshot render. Plugin
/// tests reach this via `truce_test::assert_screenshot`.
///
/// Returns `None` when no wgpu adapter is available (CI runners
/// without a GPU, headless VMs). Lets the caller fall back to a CPU
/// path or skip the screenshot assertion rather than panicking.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn render_to_pixels<P, M>(
    params: Arc<P>,
    plugin: M,
    size: (u32, u32),
    scale: f64,
    font: Option<&'static [u8]>,
) -> Option<(Vec<u8>, u32, u32)>
where
    P: Params + 'static,
    M: IcedPlugin<P>,
{
    let w = truce_gui::to_physical_px(size.0, scale);
    let h = truce_gui::to_physical_px(size.1, scale);

    // Headless wgpu setup, matching the live editor's backend per platform
    // (DX12 on Windows, Metal on macOS, Vulkan on Linux); per-backend
    // rasterization differences are handled by the reference-platform gate in
    // callers. `compatible_surface: None` is unavoidable headless - on
    // multi-GPU hosts wgpu may select a different physical adapter than the
    // editor's live path, so bake baselines on the host you gate from.
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: crate::editor::editor_backends(),
        ..Default::default()
    });

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .ok()?;

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("truce-iced-screenshot"),
        required_features: wgpu::Features::empty(),
        required_limits: adapter.limits(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::Off,
    }))
    .ok()?;

    // `Renderer::screenshot` converts the offscreen target to RGBA8
    // unconditionally before reading back, so the engine's format
    // only matters for intermediate rendering. Match the canonical
    // sRGB Metal surface so the offscreen tone mapping mirrors the
    // live editor's path.
    let format = wgpu::TextureFormat::Bgra8UnormSrgb;

    let engine = iced_wgpu::Engine::new(
        &adapter,
        device,
        queue,
        format,
        Some(iced_graphics::Antialiasing::MSAAx4),
        iced_graphics::Shell::headless(),
    );

    let default_font = if let Some(data) = font {
        crate::font::apply_font(data)
    } else {
        iced::Font::DEFAULT
    };
    let mut renderer = iced_wgpu::Renderer::new(engine, default_font, iced::Pixels(14.0));

    // Build the iced program. Seeded via [`for_test_params`] so
    // transport-aware widgets render a populated readout instead of
    // a `(no host transport)` placeholder, and so the synthetic context
    // matches the dyn-erased shape live editors receive.
    let mut param_cache = ParamCache::new(params.clone());
    param_cache.set_font(default_font);
    let context = for_test_params(params.clone() as Arc<dyn Params>).with_params(params.clone());

    let mut program = IcedProgram {
        plugin,
        param_cache,
        context,
        meter_ids: Vec::new(),
    };

    #[allow(clippy::cast_possible_truncation)] // display DPI; bounded
    let scale_f32 = scale as f32;
    let viewport = iced_graphics::Viewport::with_physical_size(Size::new(w, h), scale_f32);
    let theme = program.plugin.theme();

    // One UserInterface pass: pull a fresh param snapshot via Tick,
    // build the view, draw into the renderer. No events are queued -
    // this is a static one-shot render.
    program.dispatch(Message::Tick);

    let style = iced_runtime::core::renderer::Style {
        text_color: Color::from_rgb(0.90, 0.90, 0.92),
    };
    let cursor = iced::mouse::Cursor::Available(iced::Point::new(-1.0, -1.0));

    let mut messages: Vec<Message<M::Message>> = Vec::new();
    let view_element = program.view();
    let mut user_interface = iced_runtime::UserInterface::build(
        view_element,
        viewport.logical_size(),
        iced_runtime::user_interface::Cache::new(),
        &mut renderer,
    );
    let _ = user_interface.update(
        &[],
        cursor,
        &mut renderer,
        &mut iced_runtime::core::clipboard::Null,
        &mut messages,
    );
    user_interface.draw(&mut renderer, &theme, &style, cursor);

    let bg = crate::theme::truce_dark_theme().palette().background;
    let rgba = renderer.screenshot(&viewport, bg);

    Some((rgba, w, h))
}
