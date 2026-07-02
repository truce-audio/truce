//! GPU rendering backend using wgpu.
//!
//! Renders via Metal (macOS), DX12 (Windows), or Vulkan (Linux).
//! Uses immediate-mode geometry: each frame rebuilds the vertex buffer
//! from `RenderBackend` draw calls, then flushes in `present()`.

use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::ffi::c_void;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use lyon_tessellation::geom::point;
use lyon_tessellation::path::Path;
use lyon_tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, StrokeOptions, StrokeTessellator,
    StrokeVertex, VertexBuffers,
};
use wgpu::util::DeviceExt;

use truce_core::cast::len_u32;
use truce_gui_types::render::{ImageId, RenderBackend};
use truce_gui_types::theme::Color;

// ---------------------------------------------------------------------------
// Vertex format
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
    uv: [f32; 2],
    /// 0.0 = solid color; 1.0 = glyph atlas (R8, .r is alpha);
    /// 2.0 = RGBA image (tex * color, both premultiplied).
    tex_mode: f32,
    _pad: f32,
}

impl Vertex {
    fn solid(x: f32, y: f32, color: [f32; 4]) -> Self {
        Self {
            position: [x, y],
            color,
            uv: [0.0, 0.0],
            tex_mode: 0.0,
            _pad: 0.0,
        }
    }

    fn glyph(x: f32, y: f32, color: [f32; 4], u: f32, v: f32) -> Self {
        Self {
            position: [x, y],
            color,
            uv: [u, v],
            tex_mode: 1.0,
            _pad: 0.0,
        }
    }

    fn image(x: f32, y: f32, color: [f32; 4], u: f32, v: f32) -> Self {
        Self {
            position: [x, y],
            color,
            uv: [u, v],
            tex_mode: 2.0,
            _pad: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Glyph atlas
// ---------------------------------------------------------------------------

const ATLAS_SIZE: u32 = 512;

struct GlyphUV {
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
    advance: f32,
    width: f32,
    height: f32,
    y_offset: f32,
}

struct GlyphAtlas {
    /// Shelf-packing state.
    shelf_y: u32,
    shelf_h: u32,
    cursor_x: u32,
    /// Cached glyph UVs keyed by (char, `size_tenths`).
    glyphs: HashMap<(char, u32), GlyphUV>,
    /// Pending pixel uploads: (x, y, w, h, data).
    pending: Vec<(u32, u32, u32, u32, Vec<u8>)>,
    /// Set when `ensure_glyph` couldn't fit a new glyph. The next call to
    /// `WgpuBackend::clear` evicts the cache so subsequent frames can
    /// re-rasterize from scratch - never mid-frame, which would invalidate
    /// UVs the current frame's vertex buffer already references.
    overflow_pending: bool,
}

impl GlyphAtlas {
    fn new() -> Self {
        Self {
            shelf_y: 0,
            shelf_h: 0,
            cursor_x: 0,
            glyphs: HashMap::new(),
            pending: Vec::new(),
            overflow_pending: false,
        }
    }

    fn clear(&mut self) {
        self.shelf_y = 0;
        self.shelf_h = 0;
        self.cursor_x = 0;
        self.glyphs.clear();
        self.overflow_pending = false;
    }

    /// Try to place a glyph in the atlas. On overflow, sets
    /// `overflow_pending` and returns without inserting; caller must
    /// tolerate a missing entry for the rest of this frame. Subsequent
    /// frames clear the atlas at frame start and re-rasterize.
    // Quantized cache key - `(size * 10.0) as u32` deliberately
    // truncates to one decimal place for HashMap stability. Atlas
    // dimensions and glyph metrics fit comfortably in f32.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn ensure_glyph(&mut self, font: &fontdue::Font, ch: char, size: f32) {
        let key = (ch, (size * 10.0) as u32);
        if self.glyphs.contains_key(&key) {
            return;
        }
        let (metrics, bitmap) = font.rasterize(ch, size);
        let gw = len_u32(metrics.width);
        let gh = len_u32(metrics.height);

        // Shelf-pack: does it fit on the current shelf?
        if self.cursor_x + gw > ATLAS_SIZE {
            self.shelf_y += self.shelf_h;
            self.shelf_h = 0;
            self.cursor_x = 0;
        }
        if self.shelf_y + gh > ATLAS_SIZE {
            // Atlas full. Calling self.clear() here would wipe entries
            // the current frame's vertex buffer still references and
            // evict glyphs that earlier draw_text iterations expect to
            // look up - at best wrong UVs, at worst a HashMap lookup
            // panic. Defer the clear to the next frame boundary.
            self.overflow_pending = true;
            return;
        }

        let x = self.cursor_x;
        let y = self.shelf_y;
        self.cursor_x += gw;
        self.shelf_h = self.shelf_h.max(gh);

        let u0 = x as f32 / ATLAS_SIZE as f32;
        let v0 = y as f32 / ATLAS_SIZE as f32;
        let u1 = (x + gw) as f32 / ATLAS_SIZE as f32;
        let v1 = (y + gh) as f32 / ATLAS_SIZE as f32;

        self.pending.push((x, y, gw, gh, bitmap));

        self.glyphs.insert(
            key,
            GlyphUV {
                u0,
                v0,
                u1,
                v1,
                advance: metrics.advance_width,
                width: gw as f32,
                height: gh as f32,
                y_offset: metrics.ymin as f32,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// WGSL shader
// ---------------------------------------------------------------------------

const SHADER_SRC: &str = r"
struct Viewport {
    transform: mat4x4<f32>,
};
@group(0) @binding(0) var<uniform> viewport: Viewport;

// At group 1 slot 0 we bind either the R8 glyph atlas (tex_mode == 1.0)
// or an RGBA image (tex_mode == 2.0). For solid draws (tex_mode == 0.0)
// the texture is not sampled; any compatible binding works.
@group(1) @binding(0) var main_tex: texture_2d<f32>;
@group(1) @binding(1) var main_samp: sampler;

struct VsIn {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) tex_mode: f32,
};

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) tex_mode: f32,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip_pos = viewport.transform * vec4<f32>(in.position, 0.0, 1.0);
    out.color = in.color;
    out.uv = in.uv;
    out.tex_mode = in.tex_mode;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let tex = textureSample(main_tex, main_samp, in.uv);
    if (in.tex_mode > 1.5) {
        // Image: RGBA texture tinted by vertex color. Both sides are
        // treated as premultiplied; output is premultiplied.
        return tex * in.color;
    }
    // Glyph (tex_mode == 1) uses .r as coverage; solid (tex_mode == 0)
    // bypasses the sample. mix(1.0, tex.r, tex_mode) handles both.
    let alpha = mix(1.0, tex.r, in.tex_mode);
    return vec4<f32>(in.color.rgb * in.color.a * alpha, in.color.a * alpha);
}
";

// ---------------------------------------------------------------------------
// WgpuBackend
// ---------------------------------------------------------------------------

/// One image registered via `register_image`. Owns its wgpu texture
/// (kept alive so the bind group's view stays valid) and the bind group.
struct ImageEntry {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// A contiguous run of indices that share a single bind group.
///
/// `image` is `None` for primitives and glyphs (which use the atlas bind
/// group) and `Some(id)` for RGBA image draws. Batches are closed and a
/// new one started whenever the target bind group changes.
#[derive(Clone, Copy)]
struct DrawBatch {
    index_start: u32,
    image: Option<ImageId>,
}

/// GPU-based rendering backend.
///
/// Creates a wgpu device and surface from a platform-provided Metal layer
/// (macOS) or window handle. Implements `RenderBackend` by accumulating
/// geometry per frame, then flushing it in `present()`.
pub struct WgpuBackend {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    /// None for headless mode (snapshot testing) or when using the
    /// standalone `new()` constructor (caller owns the surface). When
    /// present, `present()` renders to the surface frame.
    surface: Option<wgpu::Surface<'static>>,
    surface_config: Option<wgpu::SurfaceConfiguration>,
    pipeline: wgpu::RenderPipeline,
    /// Format of the eventual color target. Used to (re)build the MSAA
    /// texture on resize / `begin_frame` size changes.
    target_format: wgpu::TextureFormat,
    msaa_texture: wgpu::TextureView,
    /// Current physical dimensions of the MSAA texture. `begin_frame`
    /// rebuilds the texture if these no longer match the target view.
    msaa_width: u32,
    msaa_height: u32,
    /// How long the last `present` blocked in the swapchain acquire
    /// (`get_current_texture`). The acquire waits for the compositor
    /// to free a frame slot; editor handlers read this to pace paints
    /// so the wait doesn't park the host's GUI thread.
    last_acquire_wait: std::time::Duration,
    vertices: Vec<Vertex>,
    indices: Vec<u32>,
    /// Ordered list of bind-group switches within the current frame. Always
    /// starts with one batch referencing the atlas; additional entries are
    /// appended when `draw_image` needs to switch to an image bind group.
    batches: Vec<DrawBatch>,
    glyph_atlas: GlyphAtlas,
    font: fontdue::Font,
    atlas_texture: wgpu::Texture,
    atlas_bind_group: wgpu::BindGroup,
    /// Layout shared between the atlas bind group and every per-image
    /// bind group (same `texture2d<f32>` + sampler layout).
    tex_bind_group_layout: wgpu::BindGroupLayout,
    /// Shared linear sampler used for both the glyph atlas and images.
    sampler: wgpu::Sampler,
    /// Registered images indexed by `ImageId.0`. `None` = free slot.
    images: Vec<Option<ImageEntry>>,
    viewport_buffer: wgpu::Buffer,
    viewport_bind_group: wgpu::BindGroup,
    /// Pending clear request for the next render pass. `Some(c)` means
    /// the next `finish()` clears the target to `c`; `None` means it
    /// loads existing contents (the common case when widgets overlay a
    /// custom render). Set by [`RenderBackend::clear`] and consumed by
    /// `finish()` / `present()`.
    clear_color: Option<wgpu::Color>,
    /// Fallback clear color for the present path (which can't `Load` -
    /// the swap-chain texture would surface stale prior-frame content).
    /// Used when `clear_color` is `None`. The Metal layer path defaults
    /// to `TRANSPARENT` so the host's compositor sees through; other
    /// backends default to `BLACK`.
    present_clear_default: wgpu::Color,
    width: u32,
    height: u32,
    /// Scale factor: logical points × scale = physical pixels.
    scale: f32,
}

fn ortho_matrix(w: f32, h: f32) -> [[f32; 4]; 4] {
    [
        [2.0 / w, 0.0, 0.0, 0.0],
        [0.0, -2.0 / h, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [-1.0, 1.0, 0.0, 1.0],
    ]
}

impl WgpuBackend {
    /// Create a GPU backend from a pre-created wgpu surface.
    ///
    /// `logical_w` and `logical_h` are in logical points. `scale` is the
    /// display scale factor (2.0 on Retina). The surface is configured at
    /// `logical × scale` physical pixels.
    ///
    /// # Panics
    ///
    /// Panics if the embedded font fails to parse (a bug in the
    /// bundled font asset, never user input).
    // Surface dimensions in pixels stay below 2^23, well within f32.
    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    pub fn from_surface(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        logical_w: u32,
        logical_h: u32,
        scale: f32,
    ) -> Option<Self> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .ok()?;

        // Request what the adapter actually supports rather than the
        // fixed `downlevel_defaults` cap (2048 max texture dim). On a
        // desktop GPU that's typically 8192-16384, which is needed for
        // any editor whose Retina-physical canvas exceeds 2048px on
        // either axis (the GUI Zoo's tall layouts). The defensive
        // clamp below still guards against requesting a surface
        // larger than whatever the device ended up granting.
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("truce-gpu"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .ok()?;
        let device = Arc::new(device);
        let queue = Arc::new(queue);

        let max_dim = device.limits().max_texture_dimension_2d.max(1);
        let width = truce_gui_types::to_physical_px(logical_w, f64::from(scale)).clamp(1, max_dim);
        let height = truce_gui_types::to_physical_px(logical_h, f64::from(scale)).clamp(1, max_dim);

        // Prefer `Rgba8Unorm` so the surface format matches
        // `read_pixels` and the headless screenshot path; fall back to
        // any non-sRGB format the surface advertises, then to whatever
        // the surface lists first. Keeping the format aligned across
        // windowed and headless paths means the same shader-side
        // gamma/blend assumptions hold.
        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| **f == wgpu::TextureFormat::Rgba8Unorm)
            .or_else(|| surface_caps.formats.iter().find(|f| !f.is_srgb()))
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width,
            height,
            // Windows: `on_frame` runs on the host's GUI thread; a Fifo
            // (AutoVsync) present blocks that thread when the child-window
            // swapchain backs up, freezing the host (REAPER) and risking a
            // GPU-watchdog (TDR) hang. Non-blocking present elsewhere keeps
            // vsync.
            #[cfg(target_os = "windows")]
            present_mode: wgpu::PresentMode::AutoNoVsync,
            #[cfg(not(target_os = "windows"))]
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        };
        surface.configure(&device, &surface_config);

        // MSAA texture
        let msaa_texture = Self::create_msaa_texture(&device, &surface_config);

        // Shader
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("truce-gpu-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        // Viewport uniform
        let matrix = ortho_matrix(width as f32, height as f32);
        let viewport_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("viewport"),
            contents: bytemuck::cast_slice(&matrix),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let viewport_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("viewport-layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let viewport_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewport-bg"),
            layout: &viewport_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: viewport_buffer.as_entire_binding(),
            }],
        });

        // Atlas texture (R8Unorm, 512x512)
        let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph-atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let tex_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("tex-layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas-bg"),
            layout: &tex_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        // Pipeline
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("truce-gpu-pipeline-layout"),
            bind_group_layouts: &[
                Some(&viewport_bind_group_layout),
                Some(&tex_bind_group_layout),
            ],
            immediate_size: 0,
        });

        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                // position
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // color
                wgpu::VertexAttribute {
                    offset: 8,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x4,
                },
                // uv
                wgpu::VertexAttribute {
                    offset: 24,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // tex_mode
                wgpu::VertexAttribute {
                    offset: 32,
                    shader_location: 3,
                    format: wgpu::VertexFormat::Float32,
                },
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("truce-gpu-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[vertex_layout],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 4,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });

        // Font
        let font =
            fontdue::Font::from_bytes(truce_font::JETBRAINS_MONO, fontdue::FontSettings::default())
                .expect("failed to parse embedded font");

        Some(Self {
            device,
            queue,
            surface: Some(surface),
            surface_config: Some(surface_config),
            pipeline,
            target_format: surface_format,
            msaa_texture,
            msaa_width: width,
            msaa_height: height,
            last_acquire_wait: std::time::Duration::ZERO,
            vertices: Vec::with_capacity(4096),
            indices: Vec::with_capacity(8192),
            batches: Vec::new(),
            glyph_atlas: GlyphAtlas::new(),
            font,
            atlas_texture,
            atlas_bind_group,
            tex_bind_group_layout,
            sampler,
            images: Vec::new(),
            viewport_buffer,
            viewport_bind_group,
            clear_color: None,
            present_clear_default: wgpu::Color::BLACK,
            width,
            height,
            scale,
        })
    }

    /// Create a GPU backend from a raw `CAMetalLayer` pointer (macOS).
    ///
    /// `logical_w` / `logical_h` are in logical points; `scale` is the
    /// layer's `contentsScale` (2.0 on Retina). The surface is
    /// configured at `logical × scale` physical pixels, matching the
    /// contract of [`Self::from_surface`] / [`Self::from_window`].
    ///
    /// # Safety
    /// `metal_layer` must be a valid `CAMetalLayer*` that outlives the backend.
    #[cfg(target_os = "macos")]
    pub unsafe fn from_metal_layer(
        metal_layer: *mut c_void,
        logical_w: u32,
        logical_h: u32,
        scale: f32,
    ) -> Option<Self> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::METAL;
        let instance = wgpu::Instance::new(desc);

        let surface = unsafe {
            instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(metal_layer))
        }
        .ok()?;

        Self::from_surface(&instance, surface, logical_w, logical_h, scale)
    }

    /// Create a GPU backend from a baseview window handle. baseview
    /// is the macOS / Windows / Linux windowing layer - iOS does not
    /// compile this constructor (the iOS editor builds its surface
    /// directly from a `CAMetalLayer` attached to a `UIView`).
    ///
    /// # Safety
    /// The window must remain valid for the lifetime of the backend.
    #[cfg(not(target_os = "ios"))]
    #[must_use]
    pub unsafe fn from_window(
        window: &baseview::Window,
        logical_w: u32,
        logical_h: u32,
        scale: f32,
    ) -> Option<Self> {
        unsafe {
            let instance = wgpu::Instance::new(crate::platform::editor_instance_descriptor());

            let surface = crate::platform::create_wgpu_surface(&instance, window)?;
            Self::from_surface(&instance, surface, logical_w, logical_h, scale)
        }
    }

    /// Build a standalone `WgpuBackend` that records into encoders
    /// supplied per-frame by the caller.
    ///
    /// Unlike [`Self::from_surface`] / `from_metal_layer` / [`Self::from_window`],
    /// this constructor does **not** own a `wgpu::Surface` or manage
    /// frame acquisition. The caller is expected to have its own render
    /// loop, allocate command encoders, and present - this backend only
    /// supplies the 2D widget pipeline, glyph atlas, and lyon-tessellated
    /// primitive recording.
    ///
    /// Usage:
    ///
    /// ```ignore
    /// let mut backend = WgpuBackend::new(
    ///     device.clone(), queue.clone(),
    ///     target_format, max_w, max_h,
    /// ).expect("backend init");
    ///
    /// // per-frame, after the caller has drawn its own content into `view`:
    /// backend.begin_frame(w, h);
    /// truce_gui::widgets::draw(&mut backend, &layout, &theme, &snap, &mut state);
    /// backend.finish(&mut encoder, &view);
    /// // caller submits encoder + presents.
    /// ```
    ///
    /// `max_logical_w` / `max_logical_h` are in logical points; `scale`
    /// is the display scale factor (2.0 on Retina, 1.0 otherwise). The
    /// MSAA texture is seeded at `logical × scale` physical pixels; if
    /// a subsequent `begin_frame(logical_w, logical_h)` exceeds the
    /// seed, the MSAA texture is reallocated transparently.
    ///
    /// Matches the coordinate contract of [`Self::from_surface`] /
    /// [`Self::from_window`]: draw calls and event coordinates are logical
    /// points; the backend multiplies by `scale` internally when
    /// rasterizing.
    ///
    /// # Panics
    ///
    /// Panics if the embedded font fails to parse (bundled-asset
    /// bug, never user input).
    // Surface dimensions in pixels stay below 2^23, well within f32.
    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    #[must_use]
    pub fn new(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        target_format: wgpu::TextureFormat,
        max_logical_w: u32,
        max_logical_h: u32,
        scale: f32,
    ) -> Option<Self> {
        let scale = scale.max(0.0);
        let width = truce_gui_types::to_physical_px(max_logical_w, f64::from(scale));
        let height = truce_gui_types::to_physical_px(max_logical_h, f64::from(scale));

        // Shader
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("truce-gpu-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        // Viewport uniform
        let matrix = ortho_matrix(width as f32, height as f32);
        let viewport_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("viewport"),
            contents: bytemuck::cast_slice(&matrix),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let viewport_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("viewport-layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let viewport_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewport-bg"),
            layout: &viewport_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: viewport_buffer.as_entire_binding(),
            }],
        });

        // Glyph atlas
        let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph-atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let tex_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("tex-layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas-bg"),
            layout: &tex_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        // Pipeline
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("truce-gpu-pipeline-layout"),
            bind_group_layouts: &[
                Some(&viewport_bind_group_layout),
                Some(&tex_bind_group_layout),
            ],
            immediate_size: 0,
        });

        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: 8,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x4,
                },
                wgpu::VertexAttribute {
                    offset: 24,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: 32,
                    shader_location: 3,
                    format: wgpu::VertexFormat::Float32,
                },
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("truce-gpu-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[vertex_layout],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 4,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });

        // MSAA
        let msaa_texture = Self::create_msaa_view(&device, target_format, width, height);

        let font =
            fontdue::Font::from_bytes(truce_font::JETBRAINS_MONO, fontdue::FontSettings::default())
                .expect("failed to parse embedded font");

        Some(Self {
            device,
            queue,
            surface: None,
            surface_config: None,
            pipeline,
            target_format,
            msaa_texture,
            msaa_width: width,
            msaa_height: height,
            last_acquire_wait: std::time::Duration::ZERO,
            vertices: Vec::with_capacity(4096),
            indices: Vec::with_capacity(8192),
            batches: Vec::new(),
            glyph_atlas: GlyphAtlas::new(),
            font,
            atlas_texture,
            atlas_bind_group,
            tex_bind_group_layout,
            sampler,
            images: Vec::new(),
            viewport_buffer,
            viewport_bind_group,
            clear_color: None,
            present_clear_default: wgpu::Color::TRANSPARENT,
            width,
            height,
            scale,
        })
    }

    /// Prepare for recording a frame of `logical_w × logical_h` logical
    /// points. The MSAA target and ortho matrix are sized at
    /// `logical × self.scale()` physical pixels; widget draw calls use
    /// logical coordinates.
    ///
    /// Resets accumulated geometry and the clear flag. Rebuilds the MSAA
    /// texture if the physical size differs from the previous frame.
    ///
    /// Only meaningful when the backend was built via [`Self::new`]; the
    /// surface-owning constructors drive their own frame lifecycle.
    #[allow(clippy::cast_precision_loss)]
    pub fn begin_frame(&mut self, logical_w: u32, logical_h: u32) {
        let phys_w = truce_gui_types::to_physical_px(logical_w, f64::from(self.scale));
        let phys_h = truce_gui_types::to_physical_px(logical_h, f64::from(self.scale));
        self.vertices.clear();
        self.indices.clear();
        self.batches.clear();
        self.clear_color = None;

        if phys_w != self.width || phys_h != self.height {
            self.width = phys_w;
            self.height = phys_h;
            let matrix = ortho_matrix(phys_w as f32, phys_h as f32);
            self.queue
                .write_buffer(&self.viewport_buffer, 0, bytemuck::cast_slice(&matrix));
        }

        if phys_w != self.msaa_width || phys_h != self.msaa_height {
            self.msaa_texture =
                Self::create_msaa_view(&self.device, self.target_format, phys_w, phys_h);
            self.msaa_width = phys_w;
            self.msaa_height = phys_h;
        }
    }

    /// Display scale factor: `logical × scale = physical`. Callers
    /// sizing sibling GPU resources (e.g. an intermediate texture that
    /// the backend will resolve into) should use this to stay
    /// consistent with the backend's raster dimensions.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Update the display scale factor. The next [`Self::resize`] (or
    /// [`Self::begin_frame`] in headless mode) recomputes physical
    /// dimensions and reconfigures the surface / MSAA target. Callers
    /// driving a windowed surface should follow with a `resize` so the
    /// `surface_config` picks up the new size on the same frame; the
    /// short-circuit in `resize` doesn't trigger because the scale
    /// change makes the new physical dims differ from the old.
    pub fn set_scale(&mut self, scale: f32) {
        if scale.is_finite() && scale > 0.0 {
            self.scale = scale;
        }
    }

    /// Flush accumulated geometry into a single render pass on `view`,
    /// recorded into `encoder`. The caller retains ownership of both -
    /// this method neither submits the encoder nor calls `present()`.
    ///
    /// If `clear()` was called since the last `begin_frame`, the pass
    /// uses `LoadOp::Clear(clear_color)`; otherwise `LoadOp::Load` so
    /// any prior content in `view` is preserved (the common case when
    /// widgets overlay a custom render).
    pub fn finish(&mut self, encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView) {
        self.flush_atlas();

        if self.indices.is_empty() {
            self.clear_color = None;
            return;
        }

        let vertex_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("vertices"),
                contents: bytemuck::cast_slice(&self.vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });

        let index_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("indices"),
                contents: bytemuck::cast_slice(&self.indices),
                usage: wgpu::BufferUsages::INDEX,
            });

        // MSAA load/store must agree across frames: if we plan to `Load`
        // next pass, this pass must `Store` (otherwise the next pass loads
        // undefined contents). With a `resolve_target` set, `Discard` is
        // standard for MSAA - but it's only well-defined when we also
        // `Clear` on entry, since a fresh load after a discard is UB.
        let (load, store) = match self.clear_color {
            Some(c) => (wgpu::LoadOp::Clear(c), wgpu::StoreOp::Discard),
            None => (wgpu::LoadOp::Load, wgpu::StoreOp::Store),
        };

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("truce-gpu-frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.msaa_texture,
                    resolve_target: Some(view),
                    ops: wgpu::Operations { load, store },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.viewport_bind_group, &[]);
            pass.set_vertex_buffer(0, vertex_buffer.slice(..));
            pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);

            let total_indices = len_u32(self.indices.len());
            if self.batches.is_empty() {
                pass.set_bind_group(1, &self.atlas_bind_group, &[]);
                pass.draw_indexed(0..total_indices, 0, 0..1);
            } else {
                for i in 0..self.batches.len() {
                    let b = self.batches[i];
                    let end = self
                        .batches
                        .get(i + 1)
                        .map_or(total_indices, |n| n.index_start);
                    if end <= b.index_start {
                        continue;
                    }
                    let bg = match b.image {
                        None => &self.atlas_bind_group,
                        Some(img_id) => {
                            match self.images.get(img_id.0 as usize).and_then(|s| s.as_ref()) {
                                Some(entry) => &entry.bind_group,
                                None => continue,
                            }
                        }
                    };
                    pass.set_bind_group(1, bg, &[]);
                    pass.draw_indexed(b.index_start..end, 0, 0..1);
                }
            }
        }

        self.clear_color = None;
    }

    fn create_msaa_view(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> wgpu::TextureView {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("msaa"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 4,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        tex.create_view(&wgpu::TextureViewDescriptor::default())
    }

    fn create_msaa_texture(
        device: &wgpu::Device,
        config: &wgpu::SurfaceConfiguration,
    ) -> wgpu::TextureView {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("msaa"),
            size: wgpu::Extent3d {
                width: config.width,
                height: config.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 4,
            dimension: wgpu::TextureDimension::D2,
            format: config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        tex.create_view(&wgpu::TextureViewDescriptor::default())
    }

    /// How long the last `present` blocked in the swapchain acquire.
    /// See [`Self::last_acquire_wait`].
    #[must_use]
    pub fn acquire_wait(&self) -> std::time::Duration {
        self.last_acquire_wait
    }

    /// Resize the wgpu surface, MSAA texture, and viewport projection.
    ///
    /// `logical_w` and `logical_h` are in logical points (same coordinate
    /// space as `BuiltinEditor::size()`). Returns `true` if the surface
    /// was actually reconfigured.
    #[allow(clippy::cast_precision_loss)]
    pub fn resize(&mut self, logical_w: u32, logical_h: u32) -> bool {
        // Belt-and-braces: cap each axis at whatever the adapter
        // granted us in `from_surface`. The required_limits passed in
        // are adapter-native, but a host that ignores `gui_set_size`
        // could still hand us a logical*scale request past the cap.
        let max_dim = self.device.limits().max_texture_dimension_2d.max(1);
        let new_w =
            truce_gui_types::to_physical_px(logical_w, f64::from(self.scale)).clamp(1, max_dim);
        let new_h =
            truce_gui_types::to_physical_px(logical_h, f64::from(self.scale)).clamp(1, max_dim);
        if new_w == self.width && new_h == self.height {
            return false;
        }
        self.width = new_w;
        self.height = new_h;

        if let Some(ref surface) = self.surface
            && let Some(ref mut config) = self.surface_config
        {
            config.width = new_w;
            config.height = new_h;
            surface.configure(&self.device, config);
            self.msaa_texture = Self::create_msaa_texture(&self.device, config);
        }

        // Update the orthographic projection matrix.
        let matrix = ortho_matrix(new_w as f32, new_h as f32);
        self.queue
            .write_buffer(&self.viewport_buffer, 0, bytemuck::cast_slice(&matrix));

        true
    }

    // --- Geometry helpers ---

    fn color_arr(c: Color) -> [f32; 4] {
        [c.r, c.g, c.b, c.a]
    }

    /// Ensure the current (last) batch targets `image`. If not, close the
    /// current batch and open a new one. Call before pushing indices.
    fn ensure_batch(&mut self, image: Option<ImageId>) {
        let needs_new = self.batches.last().is_none_or(|last| last.image != image);
        if needs_new {
            self.batches.push(DrawBatch {
                index_start: len_u32(self.indices.len()),
                image,
            });
        }
    }

    fn push_quad(&mut self, v0: Vertex, v1: Vertex, v2: Vertex, v3: Vertex) {
        self.ensure_batch(None);
        let base = len_u32(self.vertices.len());
        self.vertices.extend_from_slice(&[v0, v1, v2, v3]);
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    /// Tessellate a lyon path as a filled shape and append to vertex/index buffers.
    fn fill_path(&mut self, path: &Path, color: [f32; 4]) {
        self.ensure_batch(None);
        let mut buffers: VertexBuffers<Vertex, u32> = VertexBuffers::new();
        let mut tessellator = FillTessellator::new();
        let _ = tessellator.tessellate_path(
            path,
            &FillOptions::tolerance(0.5),
            &mut BuffersBuilder::new(&mut buffers, |vertex: FillVertex| {
                let p = vertex.position();
                Vertex::solid(p.x, p.y, color)
            }),
        );
        let base = len_u32(self.vertices.len());
        self.vertices.extend_from_slice(&buffers.vertices);
        self.indices
            .extend(buffers.indices.iter().map(|i| i + base));
    }

    /// Tessellate a lyon path as a stroked shape and append to vertex/index buffers.
    fn stroke_path(&mut self, path: &Path, color: [f32; 4], opts: &StrokeOptions) {
        self.ensure_batch(None);
        let mut buffers: VertexBuffers<Vertex, u32> = VertexBuffers::new();
        let mut tessellator = StrokeTessellator::new();
        let _ = tessellator.tessellate_path(
            path,
            opts,
            &mut BuffersBuilder::new(&mut buffers, |vertex: StrokeVertex| {
                let p = vertex.position();
                Vertex::solid(p.x, p.y, color)
            }),
        );
        let base = len_u32(self.vertices.len());
        self.vertices.extend_from_slice(&buffers.vertices);
        self.indices
            .extend(buffers.indices.iter().map(|i| i + base));
    }

    /// Upload pending glyph atlas writes to the GPU.
    fn flush_atlas(&mut self) {
        for (x, y, w, h, data) in self.glyph_atlas.pending.drain(..) {
            if w == 0 || h == 0 {
                continue;
            }
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.atlas_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------
// RenderBackend implementation
// ---------------------------------------------------------------------------

/// All `RenderBackend` methods accept coordinates in **logical points**.
/// The backend multiplies by `self.scale` to get physical pixel positions.
/// Font glyphs are rasterized at physical resolution for sharp text.
// Rasterizer math uses standard short names (`x`, `y`, `w`, `h`,
// `r`, `g`, `b`, `s` = scale, etc.).
#[allow(clippy::many_single_char_names)]
impl RenderBackend for WgpuBackend {
    fn clear(&mut self, color: Color) {
        self.clear_color = Some(wgpu::Color {
            r: f64::from(color.r),
            g: f64::from(color.g),
            b: f64::from(color.b),
            a: f64::from(color.a),
        });
        self.vertices.clear();
        self.indices.clear();
        self.batches.clear();
        // If a previous frame hit atlas overflow, do the eviction at the
        // frame boundary now - past frames' vertex buffers are gone, so
        // dropping the UV cache is safe. Glyphs re-rasterize lazily as
        // draw_text walks them this frame.
        if self.glyph_atlas.overflow_pending {
            self.glyph_atlas.clear();
        }
    }

    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        let s = self.scale;
        let c = Self::color_arr(color);
        self.push_quad(
            Vertex::solid(x * s, y * s, c),
            Vertex::solid((x + w) * s, y * s, c),
            Vertex::solid((x + w) * s, (y + h) * s, c),
            Vertex::solid(x * s, (y + h) * s, c),
        );
    }

    fn fill_circle(&mut self, cx: f32, cy: f32, radius: f32, color: Color) {
        let s = self.scale;
        let c = Self::color_arr(color);
        let mut builder = Path::builder();
        builder.add_circle(
            point(cx * s, cy * s),
            radius * s,
            lyon_tessellation::path::Winding::Positive,
        );
        let path = builder.build();
        self.fill_path(&path, c);
    }

    fn stroke_circle(&mut self, cx: f32, cy: f32, radius: f32, color: Color, width: f32) {
        let s = self.scale;
        let c = Self::color_arr(color);
        let mut builder = Path::builder();
        builder.add_circle(
            point(cx * s, cy * s),
            radius * s,
            lyon_tessellation::path::Winding::Positive,
        );
        let path = builder.build();
        let opts = StrokeOptions::tolerance(0.5).with_line_width(width * s);
        self.stroke_path(&path, c, &opts);
    }

    #[allow(clippy::cast_precision_loss)]
    fn stroke_arc(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        start_angle: f32,
        end_angle: f32,
        color: Color,
        width: f32,
    ) {
        let s = self.scale;
        let c = Self::color_arr(color);
        let segments = 64u32;
        let sweep = end_angle - start_angle;
        let step = sweep / segments as f32;

        let mut builder = Path::builder();
        builder.begin(point(
            cx * s + radius * s * start_angle.cos(),
            cy * s + radius * s * start_angle.sin(),
        ));
        for i in 1..=segments {
            let angle = start_angle + step * i as f32;
            builder.line_to(point(
                cx * s + radius * s * angle.cos(),
                cy * s + radius * s * angle.sin(),
            ));
        }
        builder.end(false);
        let path = builder.build();

        let opts = StrokeOptions::tolerance(0.5)
            .with_line_width(width * s)
            .with_line_cap(lyon_tessellation::LineCap::Round);
        self.stroke_path(&path, c, &opts);
    }

    fn draw_line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, color: Color, width: f32) {
        let s = self.scale;
        let c = Self::color_arr(color);
        let mut builder = Path::builder();
        builder.begin(point(x1 * s, y1 * s));
        builder.line_to(point(x2 * s, y2 * s));
        builder.end(false);
        let path = builder.build();

        let opts = StrokeOptions::tolerance(0.5)
            .with_line_width(width * s)
            .with_line_cap(lyon_tessellation::LineCap::Round);
        self.stroke_path(&path, c, &opts);
    }

    // Glyph cache key uses `(phys_size * 10.0) as u32` quantization,
    // matching `ensure_glyph`. Window-bounded coordinates fit in u32.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn draw_text(&mut self, text: &str, x: f32, y: f32, size: f32, color: Color) {
        let s = self.scale;
        let phys_size = size * s;
        let c = Self::color_arr(color);
        let line_metrics = self.font.horizontal_line_metrics(phys_size);
        let ascent = line_metrics.map_or(phys_size * 0.8, |m| m.ascent);

        let mut cursor_x = x * s;

        let chars: Vec<char> = text.chars().collect();
        for &ch in &chars {
            self.glyph_atlas.ensure_glyph(&self.font, ch, phys_size);
        }

        // `.get` rather than `[..]` - when the atlas overflows mid-frame,
        // `ensure_glyph` skips the insert (see GlyphAtlas::ensure_glyph)
        // and we want to drop the missing glyph silently rather than
        // panic on lookup. The next frame clears the atlas and these
        // glyphs come back.
        for &ch in &chars {
            let key = (ch, (phys_size * 10.0) as u32);
            let Some(g) = self.glyph_atlas.glyphs.get(&key) else {
                continue;
            };
            let (u0, v0, u1, v1, gw, gh, y_off, advance) = (
                g.u0, g.v0, g.u1, g.v1, g.width, g.height, g.y_offset, g.advance,
            );
            // Snap the glyph quad to integer pixel positions on emit.
            // Atlas texels and screen pixels are 1:1 (glyphs are
            // rasterized at `phys_size`); the shared sampler is
            // `FilterMode::Linear`, so a fractional `gx`/`gy` would
            // produce per-output-pixel UV interpolation that mixes
            // neighbouring atlas texels and smears the glyph. Snapping
            // on emit (while `cursor_x` keeps full f32 advance) yields
            // crisp glyphs with the kerning error capped at ±0.5px,
            // invisible at UI text sizes.
            let gx = cursor_x.round();
            let gy = (y * s + ascent - y_off - gh).round();

            self.push_quad(
                Vertex::glyph(gx, gy, c, u0, v0),
                Vertex::glyph(gx + gw, gy, c, u1, v0),
                Vertex::glyph(gx + gw, gy + gh, c, u1, v1),
                Vertex::glyph(gx, gy + gh, c, u0, v1),
            );

            cursor_x += advance;
        }
    }

    fn text_width(&self, text: &str, size: f32) -> f32 {
        let phys_size = size * self.scale;
        // Sum advance widths via the local fontdue instance. This used
        // to delegate to `truce_gui::font::text_width_fontdue` (a
        // glyph-cached version); doing the math inline keeps
        // truce-gpu independent of truce-gui.
        let phys: f32 = text
            .chars()
            .map(|ch| self.font.metrics(ch, phys_size).advance_width)
            .sum();
        phys / self.scale
    }

    fn register_image(&mut self, rgba: &[u8], width: u32, height: u32) -> ImageId {
        let expected = (width as usize) * (height as usize) * 4;
        if width == 0 || height == 0 || rgba.len() < expected {
            return ImageId::INVALID;
        }

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &rgba[..expected],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image-bg"),
            layout: &self.tex_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        let entry = ImageEntry {
            _texture: texture,
            bind_group,
        };

        if let Some((idx, slot)) = self
            .images
            .iter_mut()
            .enumerate()
            .find(|(_, s)| s.is_none())
        {
            *slot = Some(entry);
            return ImageId(len_u32(idx));
        }
        let id = len_u32(self.images.len());
        self.images.push(Some(entry));
        ImageId(id)
    }

    fn unregister_image(&mut self, id: ImageId) {
        if let Some(slot) = self.images.get_mut(id.0 as usize) {
            *slot = None;
        }
    }

    fn draw_image(&mut self, id: ImageId, x: f32, y: f32, w: f32, h: f32) {
        if self
            .images
            .get(id.0 as usize)
            .and_then(|s| s.as_ref())
            .is_none()
        {
            return;
        }
        self.ensure_batch(Some(id));

        let s = self.scale;
        let c = [1.0, 1.0, 1.0, 1.0];
        let base = len_u32(self.vertices.len());
        self.vertices.extend_from_slice(&[
            Vertex::image(x * s, y * s, c, 0.0, 0.0),
            Vertex::image((x + w) * s, y * s, c, 1.0, 0.0),
            Vertex::image((x + w) * s, (y + h) * s, c, 1.0, 1.0),
            Vertex::image(x * s, (y + h) * s, c, 0.0, 1.0),
        ]);
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    fn present(&mut self) {
        // Upload any pending glyph atlas writes (before borrowing surface)
        self.flush_atlas();

        let Some(surface) = &self.surface else {
            return; // headless - no surface to present to
        };

        // Acquire a swapchain frame, recovering from a stale surface.
        // After a window resize on X11/Vulkan the surface reports
        // `Outdated` and stays that way until it is reconfigured - even
        // reconfiguring to the same size clears the flag, so a plain
        // skip-the-frame would freeze the editor on its pre-resize image
        // with the desktop showing through the newly exposed area. On
        // `Outdated` / `Lost` / `Validation` we reconfigure and retry;
        // `Timeout` / `Occluded` are transient, so we skip this frame.
        let acquire_start = std::time::Instant::now();
        let mut acquired = None;
        let mut transient_skip = false;
        for _ in 0..2 {
            match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(frame)
                | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                    acquired = Some(frame);
                    break;
                }
                wgpu::CurrentSurfaceTexture::Outdated
                | wgpu::CurrentSurfaceTexture::Lost
                | wgpu::CurrentSurfaceTexture::Validation => {
                    if let Some(config) = &self.surface_config {
                        surface.configure(&self.device, config);
                    }
                }
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    transient_skip = true;
                    break;
                }
            }
        }
        self.last_acquire_wait = acquire_start.elapsed();
        if transient_skip {
            return;
        }
        let Some(frame) = acquired else {
            return;
        };
        let frame_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        if self.vertices.is_empty() {
            // No draws this frame, but the surface is double/triple-buffered
            // - without a render pass the swap chain shows whatever was
            // there before (often the second-prior frame, producing a
            // visible flicker on idle). Issue a clear-only pass so the
            // surface ends up at `clear_color`.
            self.clear_only_pass(&frame_view);
            frame.present();
            return;
        }

        self.render_pass(&frame_view);
        frame.present();
    }
}

impl WgpuBackend {
    /// Issue a render pass that only clears the surface. Used by
    /// `present()` when there is no geometry - without it the swap chain
    /// would show stale buffer contents. Always clears (`Load` would
    /// surface prior-frame garbage), falling back to
    /// `present_clear_default` when no `clear()` was requested.
    fn clear_only_pass(&mut self, resolve_target: &wgpu::TextureView) {
        let clear_color = self.clear_color.unwrap_or(self.present_clear_default);
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("clear-only"),
            });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear-only"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.msaa_texture,
                    resolve_target: Some(resolve_target),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Discard,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.queue.submit(std::iter::once(encoder.finish()));
    }

    /// Render accumulated geometry to a texture view (shared by present + headless).
    fn render_pass(&mut self, resolve_target: &wgpu::TextureView) {
        let clear_color = self.clear_color.unwrap_or(self.present_clear_default);
        let vertex_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("vertices"),
                contents: bytemuck::cast_slice(&self.vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });

        let index_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("indices"),
                contents: bytemuck::cast_slice(&self.indices),
                usage: wgpu::BufferUsages::INDEX,
            });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.msaa_texture,
                    resolve_target: Some(resolve_target),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Discard,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.viewport_bind_group, &[]);
            pass.set_vertex_buffer(0, vertex_buffer.slice(..));
            pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);

            let total_indices = len_u32(self.indices.len());
            if self.batches.is_empty() {
                // Backwards-compatible path: no batching recorded (e.g. a
                // caller that bypassed clear()). Draw everything with the
                // atlas bind group.
                pass.set_bind_group(1, &self.atlas_bind_group, &[]);
                pass.draw_indexed(0..total_indices, 0, 0..1);
            } else {
                for i in 0..self.batches.len() {
                    let b = self.batches[i];
                    let end = self
                        .batches
                        .get(i + 1)
                        .map_or(total_indices, |n| n.index_start);
                    if end <= b.index_start {
                        continue;
                    }
                    let bg = match b.image {
                        None => &self.atlas_bind_group,
                        Some(img_id) => {
                            match self.images.get(img_id.0 as usize).and_then(|s| s.as_ref()) {
                                Some(entry) => &entry.bind_group,
                                // Image was unregistered mid-frame; skip draw.
                                None => continue,
                            }
                        }
                    };
                    pass.set_bind_group(1, bg, &[]);
                    pass.draw_indexed(b.index_start..end, 0, 0..1);
                }
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
    }

    /// Create a headless GPU backend (no window or surface).
    /// Used for snapshot testing.
    ///
    /// # Panics
    ///
    /// Panics if the embedded font fails to parse (bundled-asset
    /// bug, never user input).
    // Surface dimensions in pixels stay below 2^23, well within f32.
    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    #[must_use]
    pub fn headless(width: u32, height: u32, scale: f32) -> Option<Self> {
        let phys_w = truce_gui_types::to_physical_px(width, f64::from(scale));
        let phys_h = truce_gui_types::to_physical_px(height, f64::from(scale));

        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(desc);

        // Headless: there is no `wgpu::Surface` to constrain the adapter
        // pick against, so `compatible_surface` is `None`. On a multi-GPU
        // host (e.g. discrete + integrated, or NVIDIA Optimus) wgpu may
        // pick a different physical adapter than the live render path's
        // `compatible_surface: Some(&surface)`, which can produce subtle
        // rasterization differences (driver-specific shader compile, MSAA
        // resolve, sRGB rounding). Bake screenshot baselines on the host
        // you gate from - this is the same constraint already documented
        // in `cargo truce screenshot --check`.
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok()?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("truce-gpu-headless"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .ok()?;
        let device = Arc::new(device);
        let queue = Arc::new(queue);

        // Use non-sRGB to match the windowed path (which picks !is_srgb())
        let texture_format = wgpu::TextureFormat::Rgba8Unorm;

        // MSAA texture
        let msaa_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("msaa"),
            size: wgpu::Extent3d {
                width: phys_w,
                height: phys_h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 4,
            dimension: wgpu::TextureDimension::D2,
            format: texture_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let msaa_view = msaa_texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Shader
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("truce-gpu-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        // Viewport
        let matrix = ortho_matrix(phys_w as f32, phys_h as f32);
        let viewport_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("viewport"),
            contents: bytemuck::cast_slice(&matrix),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let viewport_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("viewport-layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let viewport_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewport-bg"),
            layout: &viewport_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: viewport_buffer.as_entire_binding(),
            }],
        });

        // Atlas
        let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph-atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let tex_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("tex-layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas-bg"),
            layout: &tex_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        // Pipeline
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("truce-gpu-pipeline-layout"),
            bind_group_layouts: &[
                Some(&viewport_bind_group_layout),
                Some(&tex_bind_group_layout),
            ],
            immediate_size: 0,
        });

        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: 8,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x4,
                },
                wgpu::VertexAttribute {
                    offset: 24,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: 32,
                    shader_location: 3,
                    format: wgpu::VertexFormat::Float32,
                },
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("truce-gpu-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[vertex_layout],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: texture_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 4,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });

        let font =
            fontdue::Font::from_bytes(truce_font::JETBRAINS_MONO, fontdue::FontSettings::default())
                .expect("failed to parse embedded font");

        Some(Self {
            device,
            queue,
            surface: None,
            surface_config: None,
            pipeline,
            target_format: texture_format,
            msaa_texture: msaa_view,
            msaa_width: phys_w,
            msaa_height: phys_h,
            vertices: Vec::with_capacity(4096),
            indices: Vec::with_capacity(8192),
            batches: Vec::new(),
            glyph_atlas: GlyphAtlas::new(),
            font,
            atlas_texture,
            atlas_bind_group,
            tex_bind_group_layout,
            sampler,
            images: Vec::new(),
            viewport_buffer,
            viewport_bind_group,
            clear_color: None,
            present_clear_default: wgpu::Color::BLACK,
            width: phys_w,
            height: phys_h,
            scale,
            last_acquire_wait: std::time::Duration::ZERO,
        })
    }

    /// Render to an offscreen texture and read back RGBA pixels.
    /// Only works for headless backends (no surface).
    ///
    /// # Panics
    ///
    /// Panics if `wgpu::Buffer::map_async` reports failure when reading
    /// back the GPU readback buffer - that indicates an adapter / driver
    /// fault rather than a recoverable runtime condition, so the
    /// snapshot path bubbles it up rather than papering over it.
    pub fn read_pixels(&mut self) -> Vec<u8> {
        self.flush_atlas();

        let w = self.width;
        let h = self.height;
        let format = wgpu::TextureFormat::Rgba8Unorm;

        // Offscreen resolve target
        let target_texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
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
        let target_view = target_texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Render
        if !self.vertices.is_empty() {
            self.render_pass(&target_view);
        }

        // Readback
        let bytes_per_row = (w * 4 + 255) & !255; // 256-byte aligned
        let readback_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(bytes_per_row * h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback_buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(std::iter::once(encoder.finish()));

        // Map and copy
        let buf_slice = readback_buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        buf_slice.map_async(wgpu::MapMode::Read, move |result| {
            tx.send(result).unwrap();
        });
        let _ = self.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        });
        rx.recv().unwrap().expect("buffer map failed");

        let mapped = buf_slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for row in 0..h {
            let start = (row * bytes_per_row) as usize;
            let end = start + (w * 4) as usize;
            pixels.extend_from_slice(&mapped[start..end]);
        }
        drop(mapped);
        readback_buf.unmap();

        // Shader writes premultiplied alpha (BlendState::PREMULTIPLIED_ALPHA_BLENDING),
        // but downstream consumers - `truce-test`'s reference-PNG comparison
        // and `cargo truce screenshot` output - assume straight RGBA, the
        // same convention `truce-slint::screenshot::render_with_state` uses.
        // Un-premultiply here so the GPU readback matches the headless
        // contract instead of leaking the GPU's internal format.
        for px in pixels.chunks_exact_mut(4) {
            let a = px[3];
            if a == 0 || a == 255 {
                continue;
            }
            let a16 = u16::from(a);
            // Round to nearest: (c * 255 + a/2) / a.
            px[0] = ((u16::from(px[0]) * 255 + a16 / 2) / a16).min(255) as u8;
            px[1] = ((u16::from(px[1]) * 255 + a16 / 2) / a16).min(255) as u8;
            px[2] = ((u16::from(px[2]) * 255 + a16 / 2) / a16).min(255) as u8;
        }

        pixels
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_size() {
        // 2 (pos) + 4 (color) + 2 (uv) + 1 (tex_mix) + 1 (pad) = 10 floats = 40 bytes
        let size = std::mem::size_of::<Vertex>();
        assert!(size > 0, "Vertex should have non-zero size: {size}");
    }

    // Both ortho-matrix tests check that the helper maps top-left
    // screen-space origin (0, 0) to wgpu's top-left clip-space corner
    // (-1, +1) and bottom-right screen (w, h) to clip (+1, -1). The Y
    // flip is the `-2.0 / h` term in `ortho_matrix` - without it,
    // increasing screen-y would move the vertex *up* in clip space.
    #[test]
    fn ortho_matrix_maps_origin() {
        let m = ortho_matrix(800.0, 600.0);
        let x = m[0][0] * 0.0 + m[3][0];
        let y = m[1][1] * 0.0 + m[3][1];
        assert!((x - (-1.0)).abs() < 1e-6);
        assert!((y - 1.0).abs() < 1e-6);
    }

    #[test]
    fn ortho_matrix_maps_bottom_right() {
        let m = ortho_matrix(800.0, 600.0);
        let x = m[0][0] * 800.0 + m[3][0];
        let y = m[1][1] * 600.0 + m[3][1];
        assert!((x - 1.0).abs() < 1e-6);
        assert!((y - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn glyph_atlas_shelf_packing() {
        let font =
            fontdue::Font::from_bytes(truce_font::JETBRAINS_MONO, fontdue::FontSettings::default())
                .unwrap();
        let mut atlas = GlyphAtlas::new();

        // Pack a few glyphs
        atlas.ensure_glyph(&font, 'A', 14.0);
        atlas.ensure_glyph(&font, 'B', 14.0);
        atlas.ensure_glyph(&font, 'C', 14.0);

        assert_eq!(atlas.glyphs.len(), 3);
        assert!(!atlas.pending.is_empty());

        // Same glyph at same size should not create a new entry
        atlas.ensure_glyph(&font, 'A', 14.0);
        assert_eq!(atlas.glyphs.len(), 3);
    }

    #[test]
    fn lyon_fill_circle_produces_triangles() {
        let mut builder = Path::builder();
        builder.add_circle(
            point(50.0, 50.0),
            10.0,
            lyon_tessellation::path::Winding::Positive,
        );
        let path = builder.build();
        let mut buffers: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
        let mut tess = FillTessellator::new();
        tess.tessellate_path(
            &path,
            &FillOptions::tolerance(0.5),
            &mut BuffersBuilder::new(&mut buffers, |v: FillVertex| {
                let p = v.position();
                [p.x, p.y]
            }),
        )
        .unwrap();
        assert!(buffers.vertices.len() >= 3);
        assert!(buffers.indices.len() >= 3);
    }

    /// End-to-end smoke test for the standalone `new` / `begin_frame` /
    /// `finish` path: build a backend against a caller-owned device,
    /// record some primitives + text, render into an offscreen texture,
    /// and verify we wrote non-background pixels.
    #[test]
    #[allow(clippy::too_many_lines, clippy::many_single_char_names)]
    fn standalone_pipeline_renders() {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(desc);
        let Ok(adapter) =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            }))
        else {
            return; // no GPU in this environment
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("standalone-test"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .expect("request_device");
        let device = Arc::new(device);
        let queue = Arc::new(queue);

        let w = 64u32;
        let h = 48u32;
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut backend =
            WgpuBackend::new(Arc::clone(&device), Arc::clone(&queue), format, w, h, 1.0)
                .expect("backend new");

        // Pre-fill the offscreen target with red so we can tell apart
        // "finish drew something" from "finish cleared to background".
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("standalone-target"),
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
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());

        backend.begin_frame(w, h);
        backend.clear(Color::rgb(0.0, 0.0, 0.0));
        backend.fill_rect(8.0, 8.0, 16.0, 16.0, Color::rgb(0.0, 1.0, 0.0));
        backend.draw_text("x", 20.0, 20.0, 14.0, Color::rgb(1.0, 1.0, 1.0));

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("standalone-enc"),
        });
        backend.finish(&mut encoder, &view);

        // Copy target to a readback buffer and inspect.
        let bytes_per_row = (w * 4 + 255) & !255;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(bytes_per_row * h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(std::iter::once(encoder.finish()));

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            tx.send(r).unwrap();
        });
        let _ = device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        });
        rx.recv().unwrap().unwrap();
        let mapped = slice.get_mapped_range();

        // Probe the green rect center (16, 16) - should be ~green.
        let row_off = 16usize * bytes_per_row as usize;
        let px_off = row_off + 16 * 4;
        let r = mapped[px_off];
        let g = mapped[px_off + 1];
        let b = mapped[px_off + 2];
        assert!(g > 200, "green rect not rendered: got rgb=({r},{g},{b})");
        assert!(
            r < 50 && b < 50,
            "green rect leaked other channels: rgb=({r},{g},{b})"
        );
    }
}
