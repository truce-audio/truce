//! GPU rendering backend using wgpu.
//!
//! Renders via Metal (macOS), DX12 (Windows), or Vulkan (Linux).
//! Uses immediate-mode geometry: each frame rebuilds the vertex buffer
//! from `RenderBackend` draw calls, then flushes in `present()`.

use std::collections::HashMap;
use std::ffi::c_void;

use bytemuck::{Pod, Zeroable};
use lyon_tessellation::geom::point;
use lyon_tessellation::path::Path;
use lyon_tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, StrokeOptions, StrokeTessellator,
    StrokeVertex, VertexBuffers,
};
use wgpu::util::DeviceExt;

use truce_gui::render::RenderBackend;
use truce_gui::theme::Color;

// ---------------------------------------------------------------------------
// Vertex format
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
    uv: [f32; 2],
    tex_mix: f32, // 0.0 = solid color, 1.0 = texture * color
    _pad: f32,
}

impl Vertex {
    fn solid(x: f32, y: f32, color: [f32; 4]) -> Self {
        Self {
            position: [x, y],
            color,
            uv: [0.0, 0.0],
            tex_mix: 0.0,
            _pad: 0.0,
        }
    }

    fn textured(x: f32, y: f32, color: [f32; 4], u: f32, v: f32) -> Self {
        Self {
            position: [x, y],
            color,
            uv: [u, v],
            tex_mix: 1.0,
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
    /// Cached glyph UVs keyed by (char, size_tenths).
    glyphs: HashMap<(char, u32), GlyphUV>,
    /// Pending pixel uploads: (x, y, w, h, data).
    pending: Vec<(u32, u32, u32, u32, Vec<u8>)>,
}

impl GlyphAtlas {
    fn new() -> Self {
        Self {
            shelf_y: 0,
            shelf_h: 0,
            cursor_x: 0,
            glyphs: HashMap::new(),
            pending: Vec::new(),
        }
    }

    fn clear(&mut self) {
        self.shelf_y = 0;
        self.shelf_h = 0;
        self.cursor_x = 0;
        self.glyphs.clear();
    }

    /// Ensure a glyph is in the atlas, rasterizing and packing it if needed.
    /// Returns the UV entry.
    fn ensure_glyph(&mut self, font: &fontdue::Font, ch: char, size: f32) -> &GlyphUV {
        let key = (ch, (size * 10.0) as u32);
        if !self.glyphs.contains_key(&key) {
            let (metrics, bitmap) = font.rasterize(ch, size);
            let gw = metrics.width as u32;
            let gh = metrics.height as u32;

            // Shelf-pack: does it fit on the current shelf?
            if self.cursor_x + gw > ATLAS_SIZE {
                // Start new shelf
                self.shelf_y += self.shelf_h;
                self.shelf_h = 0;
                self.cursor_x = 0;
            }
            if self.shelf_y + gh > ATLAS_SIZE {
                // Atlas full — clear and re-pack (rare)
                self.clear();
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

            self.glyphs.insert(key, GlyphUV {
                u0,
                v0,
                u1,
                v1,
                advance: metrics.advance_width,
                width: gw as f32,
                height: gh as f32,
                y_offset: metrics.ymin as f32,
            });
        }
        self.glyphs.get(&key).unwrap()
    }
}

// ---------------------------------------------------------------------------
// WGSL shader
// ---------------------------------------------------------------------------

const SHADER_SRC: &str = r#"
struct Viewport {
    transform: mat4x4<f32>,
};
@group(0) @binding(0) var<uniform> viewport: Viewport;

@group(1) @binding(0) var glyph_tex: texture_2d<f32>;
@group(1) @binding(1) var glyph_samp: sampler;

struct VsIn {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) tex_mix: f32,
};

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) tex_mix: f32,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip_pos = viewport.transform * vec4<f32>(in.position, 0.0, 1.0);
    out.color = in.color;
    out.uv = in.uv;
    out.tex_mix = in.tex_mix;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let tex_a = textureSample(glyph_tex, glyph_samp, in.uv).r;
    let alpha = mix(1.0, tex_a, in.tex_mix);
    return vec4<f32>(in.color.rgb * in.color.a * alpha, in.color.a * alpha);
}
"#;

// ---------------------------------------------------------------------------
// WgpuBackend
// ---------------------------------------------------------------------------

/// GPU-based rendering backend.
///
/// Creates a wgpu device and surface from a platform-provided Metal layer
/// (macOS) or window handle. Implements `RenderBackend` by accumulating
/// geometry per frame, then flushing it in `present()`.
pub struct WgpuBackend {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// None for headless mode (snapshot testing). When present, `present()`
    /// renders to the surface frame. When None, use `read_pixels()` instead.
    surface: Option<wgpu::Surface<'static>>,
    surface_config: Option<wgpu::SurfaceConfiguration>,
    pipeline: wgpu::RenderPipeline,
    msaa_texture: wgpu::TextureView,
    vertices: Vec<Vertex>,
    indices: Vec<u32>,
    glyph_atlas: GlyphAtlas,
    font: fontdue::Font,
    atlas_texture: wgpu::Texture,
    atlas_bind_group: wgpu::BindGroup,
    viewport_buffer: wgpu::Buffer,
    viewport_bind_group: wgpu::BindGroup,
    clear_color: wgpu::Color,
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
    pub fn from_surface(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        logical_w: u32,
        logical_h: u32,
        scale: f32,
    ) -> Option<Self> {
        let width = (logical_w as f32 * scale) as u32;
        let height = (logical_h as f32 * scale) as u32;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("truce-gpu"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .ok()?;

        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| !f.is_srgb())
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width,
            height,
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
        let viewport_buffer =
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
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
        let atlas_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let atlas_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("atlas-layout"),
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
            layout: &atlas_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&atlas_sampler),
                },
            ],
        });

        // Pipeline
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("truce-gpu-pipeline-layout"),
            bind_group_layouts: &[&viewport_bind_group_layout, &atlas_bind_group_layout],
            push_constant_ranges: &[],
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
                // tex_mix
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
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
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
            multiview: None,
            cache: None,
        });

        // Font
        let font = fontdue::Font::from_bytes(
            truce_gui::font::JETBRAINS_MONO,
            fontdue::FontSettings::default(),
        )
        .expect("failed to parse embedded font");

        Some(Self {
            device,
            queue,
            surface: Some(surface),
            surface_config: Some(surface_config),
            pipeline,
            msaa_texture,
            vertices: Vec::with_capacity(4096),
            indices: Vec::with_capacity(8192),
            glyph_atlas: GlyphAtlas::new(),
            font,
            atlas_texture,
            atlas_bind_group,
            viewport_buffer,
            viewport_bind_group,
            clear_color: wgpu::Color::BLACK,
            width,
            height,
            scale,
        })
    }

    /// Create a GPU backend from a raw `CAMetalLayer` pointer (macOS).
    ///
    /// # Safety
    /// `metal_layer` must be a valid `CAMetalLayer*` that outlives the backend.
    pub unsafe fn from_metal_layer(
        metal_layer: *mut c_void,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });

        let surface = unsafe {
            instance.create_surface_unsafe(
                wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(metal_layer),
            )
        }
        .ok()?;

        Self::from_surface(&instance, surface, width, height, 1.0)
    }

    /// Create a GPU backend from a baseview window handle.
    ///
    /// # Safety
    /// The window must remain valid for the lifetime of the backend.
    pub unsafe fn from_window(
        window: &baseview::Window,
        logical_w: u32,
        logical_h: u32,
        scale: f32,
    ) -> Option<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });

        let surface = crate::platform::create_wgpu_surface(&instance, window)?;
        Self::from_surface(&instance, surface, logical_w, logical_h, scale)
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

    /// Resize the wgpu surface, MSAA texture, and viewport projection.
    ///
    /// `logical_w` and `logical_h` are in logical points (same coordinate
    /// space as `BuiltinEditor::size()`). Returns `true` if the surface
    /// was actually reconfigured.
    pub fn resize(&mut self, logical_w: u32, logical_h: u32) -> bool {
        let new_w = (logical_w as f32 * self.scale) as u32;
        let new_h = (logical_h as f32 * self.scale) as u32;
        if new_w == self.width && new_h == self.height {
            return false;
        }
        self.width = new_w;
        self.height = new_h;

        if let Some(ref surface) = self.surface {
            if let Some(ref mut config) = self.surface_config {
                config.width = new_w;
                config.height = new_h;
                surface.configure(&self.device, config);
                self.msaa_texture = Self::create_msaa_texture(&self.device, config);
            }
        }

        // Update the orthographic projection matrix.
        let matrix = ortho_matrix(new_w as f32, new_h as f32);
        self.queue.write_buffer(
            &self.viewport_buffer,
            0,
            bytemuck::cast_slice(&matrix),
        );

        true
    }

    // --- Geometry helpers ---

    fn color_arr(c: Color) -> [f32; 4] {
        [c.r, c.g, c.b, c.a]
    }

    fn push_quad(&mut self, v0: Vertex, v1: Vertex, v2: Vertex, v3: Vertex) {
        let base = self.vertices.len() as u32;
        self.vertices.extend_from_slice(&[v0, v1, v2, v3]);
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    /// Tessellate a lyon path as a filled shape and append to vertex/index buffers.
    fn fill_path(&mut self, path: &Path, color: [f32; 4]) {
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
        let base = self.vertices.len() as u32;
        self.vertices.extend_from_slice(&buffers.vertices);
        self.indices
            .extend(buffers.indices.iter().map(|i| i + base));
    }

    /// Tessellate a lyon path as a stroked shape and append to vertex/index buffers.
    fn stroke_path(&mut self, path: &Path, color: [f32; 4], opts: &StrokeOptions) {
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
        let base = self.vertices.len() as u32;
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

/// All RenderBackend methods accept coordinates in **logical points**.
/// The backend multiplies by `self.scale` to get physical pixel positions.
/// Font glyphs are rasterized at physical resolution for sharp text.
impl RenderBackend for WgpuBackend {
    fn clear(&mut self, color: Color) {
        self.clear_color = wgpu::Color {
            r: color.r as f64,
            g: color.g as f64,
            b: color.b as f64,
            a: color.a as f64,
        };
        self.vertices.clear();
        self.indices.clear();
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
        builder.add_circle(point(cx * s, cy * s), radius * s, lyon_tessellation::path::Winding::Positive);
        let path = builder.build();
        self.fill_path(&path, c);
    }

    fn stroke_circle(&mut self, cx: f32, cy: f32, radius: f32, color: Color, width: f32) {
        let s = self.scale;
        let c = Self::color_arr(color);
        let mut builder = Path::builder();
        builder.add_circle(point(cx * s, cy * s), radius * s, lyon_tessellation::path::Winding::Positive);
        let path = builder.build();
        let opts = StrokeOptions::tolerance(0.5).with_line_width(width * s);
        self.stroke_path(&path, c, &opts);
    }

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
            builder.line_to(point(cx * s + radius * s * angle.cos(), cy * s + radius * s * angle.sin()));
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

    fn draw_text(&mut self, text: &str, x: f32, y: f32, size: f32, color: Color) {
        let s = self.scale;
        let phys_size = size * s;
        let c = Self::color_arr(color);
        let line_metrics = self.font.horizontal_line_metrics(phys_size);
        let ascent = line_metrics.map(|m| m.ascent).unwrap_or(phys_size * 0.8);

        let mut cursor_x = x * s;

        let chars: Vec<char> = text.chars().collect();
        for &ch in &chars {
            self.glyph_atlas.ensure_glyph(&self.font, ch, phys_size);
        }

        let glyph_quads: Vec<_> = chars
            .iter()
            .map(|&ch| {
                let key = (ch, (phys_size * 10.0) as u32);
                let g = &self.glyph_atlas.glyphs[&key];
                (g.u0, g.v0, g.u1, g.v1, g.width, g.height, g.y_offset, g.advance)
            })
            .collect();

        for (u0, v0, u1, v1, gw, gh, y_off, advance) in glyph_quads {
            let gx = cursor_x;
            let gy = y * s + ascent - y_off - gh;

            self.push_quad(
                Vertex::textured(gx, gy, c, u0, v0),
                Vertex::textured(gx + gw, gy, c, u1, v0),
                Vertex::textured(gx + gw, gy + gh, c, u1, v1),
                Vertex::textured(gx, gy + gh, c, u0, v1),
            );

            cursor_x += advance;
        }
    }

    fn text_width(&self, text: &str, size: f32) -> f32 {
        let phys_size = size * self.scale;
        truce_gui::font::text_width_fontdue(text, phys_size) / self.scale
    }

    fn present(&mut self) {
        // Upload any pending glyph atlas writes (before borrowing surface)
        self.flush_atlas();

        let surface = match &self.surface {
            Some(s) => s,
            None => return, // headless — no surface to present to
        };

        let frame = match surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => return,
        };
        let frame_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Upload geometry
        if self.vertices.is_empty() {
            frame.present();
            return;
        }

        self.render_pass(&frame_view);
        frame.present();
    }
}

impl WgpuBackend {
    /// Render accumulated geometry to a texture view (shared by present + headless).
    fn render_pass(&mut self, resolve_target: &wgpu::TextureView) {
        let vertex_buffer =
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("vertices"),
                    contents: bytemuck::cast_slice(&self.vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                });

        let index_buffer =
            self.device
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
                        load: wgpu::LoadOp::Clear(self.clear_color),
                        store: wgpu::StoreOp::Discard,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.viewport_bind_group, &[]);
            pass.set_bind_group(1, &self.atlas_bind_group, &[]);
            pass.set_vertex_buffer(0, vertex_buffer.slice(..));
            pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..self.indices.len() as u32, 0, 0..1);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
    }

    /// Create a headless GPU backend (no window or surface).
    /// Used for snapshot testing.
    pub fn headless(width: u32, height: u32, scale: f32) -> Option<Self> {
        let phys_w = (width as f32 * scale) as u32;
        let phys_h = (height as f32 * scale) as u32;

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("truce-gpu-headless"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .ok()?;

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
        let viewport_buffer =
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
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
        let atlas_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let atlas_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("atlas-layout"),
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
            layout: &atlas_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&atlas_sampler),
                },
            ],
        });

        // Pipeline
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("truce-gpu-pipeline-layout"),
            bind_group_layouts: &[&viewport_bind_group_layout, &atlas_bind_group_layout],
            push_constant_ranges: &[],
        });

        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x2 },
                wgpu::VertexAttribute { offset: 8, shader_location: 1, format: wgpu::VertexFormat::Float32x4 },
                wgpu::VertexAttribute { offset: 24, shader_location: 2, format: wgpu::VertexFormat::Float32x2 },
                wgpu::VertexAttribute { offset: 32, shader_location: 3, format: wgpu::VertexFormat::Float32 },
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("truce-gpu-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[vertex_layout],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: texture_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
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
            multiview: None,
            cache: None,
        });

        let font = fontdue::Font::from_bytes(
            truce_gui::font::JETBRAINS_MONO,
            fontdue::FontSettings::default(),
        )
        .expect("failed to parse embedded font");

        Some(Self {
            device,
            queue,
            surface: None,
            surface_config: None,
            pipeline,
            msaa_texture: msaa_view,
            vertices: Vec::with_capacity(4096),
            indices: Vec::with_capacity(8192),
            glyph_atlas: GlyphAtlas::new(),
            font,
            atlas_texture,
            atlas_bind_group,
            viewport_buffer,
            viewport_bind_group,
            clear_color: wgpu::Color::BLACK,
            width: phys_w,
            height: phys_h,
            scale,
        })
    }

    /// Render to an offscreen texture and read back RGBA pixels.
    /// Only works for headless backends (no surface).
    pub fn read_pixels(&mut self) -> Vec<u8> {
        self.flush_atlas();

        let w = self.width;
        let h = self.height;
        let format = wgpu::TextureFormat::Rgba8Unorm;

        // Offscreen resolve target
        let target_texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
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
            size: (bytes_per_row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
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
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit(std::iter::once(encoder.finish()));

        // Map and copy
        let buf_slice = readback_buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        buf_slice.map_async(wgpu::MapMode::Read, move |result| {
            tx.send(result).unwrap();
        });
        self.device.poll(wgpu::Maintain::Wait);
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

    #[test]
    fn ortho_matrix_maps_origin() {
        let m = ortho_matrix(800.0, 600.0);
        // (0,0) should map to (-1, 1) in clip space
        let x = m[0][0] * 0.0 + m[3][0];
        let y = m[1][1] * 0.0 + m[3][1];
        assert!((x - (-1.0)).abs() < 1e-6);
        assert!((y - 1.0).abs() < 1e-6);
    }

    #[test]
    fn ortho_matrix_maps_bottom_right() {
        let m = ortho_matrix(800.0, 600.0);
        // (800, 600) should map to (1, -1) in clip space
        let x = m[0][0] * 800.0 + m[3][0];
        let y = m[1][1] * 600.0 + m[3][1];
        assert!((x - 1.0).abs() < 1e-6);
        assert!((y - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn glyph_atlas_shelf_packing() {
        let font = fontdue::Font::from_bytes(
            truce_gui::font::JETBRAINS_MONO,
            fontdue::FontSettings::default(),
        )
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
}
