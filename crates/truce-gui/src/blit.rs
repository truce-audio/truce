//! Pixel buffer → wgpu surface blit pipeline.
//!
//! Uploads an RGBA pixel buffer to a GPU texture, then draws it to the
//! surface at its **native** pixel size, centred on **integer**
//! (whole-pixel) margins, with the rest of the surface left black. When
//! the surface matches the texture (the usual case, and every resizable
//! editor once `set_size` catches up) that quad is the whole surface,
//! identical to a plain fullscreen blit. When the surface is *larger*
//! than the texture - a fixed-size editor whose host (REAPER's LV2 X11
//! embedding) grew the window past the editor - the texture renders 1:1
//! instead of being stretched to fill (the blurry GUI), with black
//! letterboxing the gap. Copied from truce-slint, then extended with
//! the native-size quad.
//!
//! The centring is pixel-snapped on purpose: a symmetric ±scale quad
//! centres with a half-pixel offset whenever the letterbox margin is
//! odd, which makes a native-size (nearest-sampled) blit shimmer as the
//! surface wobbles ±1px under a fixed texture - REAPER cycling an embed
//! window between e.g. 277 and 278 px wide. Flooring the margin to a
//! whole pixel keeps every texel on exactly one output pixel.

const BLIT_SHADER: &str = r"
// `rect` = the texture quad's NDC bounds (left, top, right, bottom),
// computed CPU-side from *integer* letterbox margins so the texture
// always lands on exact output pixels. Passing pixel-snapped bounds
// (rather than a symmetric ±scale that centres with a half-pixel offset
// on odd margins) keeps a native-size blit from shimmering when the
// surface size wobbles by a pixel under it.
struct Params {
    rect: vec4<f32>,
};
@group(0) @binding(2) var<uniform> params: Params;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VertexOutput {
    // Unit quad as a triangle strip: (0,0) (1,0) (0,1) (1,1).
    var unit = array<vec2<f32>, 4>(
        vec2(0.0, 0.0),
        vec2(1.0, 0.0),
        vec2(0.0, 1.0),
        vec2(1.0, 1.0),
    );
    let u = unit[idx];
    var out: VertexOutput;
    // Map the unit quad onto the pixel-snapped NDC rect: x from left
    // (rect.x) to right (rect.z), y from top (rect.y) to bottom (rect.w).
    out.position = vec4(
        mix(params.rect.x, params.rect.z, u.x),
        mix(params.rect.y, params.rect.w, u.y),
        0.0,
        1.0,
    );
    out.uv = u;
    return out;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
";

/// Simple wgpu pipeline that blits an RGBA pixel buffer to the screen.
pub struct BlitPipeline {
    pipeline: wgpu::RenderPipeline,
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Holds the `scale` (texture/surface fraction) the vertex shader
    /// reads; rewritten each `render` from the live surface size.
    uniform_buf: wgpu::Buffer,
    width: u32,
    height: u32,
}

impl BlitPipeline {
    #[must_use]
    pub fn new(
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit-shader"),
            source: wgpu::ShaderSource::Wgsl(BLIT_SHADER.into()),
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blit-layout"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blit-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // 16 bytes: `scale` (vec2) + padding to satisfy the uniform's
        // 16-byte alignment. Initialised to (1, 1) - a full-surface
        // blit - and rewritten each `render`.
        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("blit-scale-uniform"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let texture = Self::create_texture(device, width, height);
        let bind_group =
            Self::create_bind_group(device, &bind_group_layout, &texture, &sampler, &uniform_buf);

        Self {
            pipeline,
            texture,
            bind_group,
            bind_group_layout,
            sampler,
            uniform_buf,
            width,
            height,
        }
    }

    /// Upload new pixel data (RGBA, 4 bytes per pixel).
    pub fn update(&self, queue: &wgpu::Queue, rgba_pixels: &[u8]) {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba_pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.width * 4),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Draw the texture to a render target sized `(surf_w, surf_h)`
    /// physical pixels. The texture is drawn at its native size,
    /// centred on whole-pixel margins; any surface area beyond it is
    /// cleared to black (letterbox). When the surface matches the
    /// texture this is a plain fullscreen blit.
    // Window dimensions are a few thousand px at most - far below
    // `f32`'s 2^24 exact-integer ceiling, so the ratio is exact.
    #[allow(clippy::cast_precision_loss)]
    pub fn render(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        surf_w: u32,
        surf_h: u32,
    ) {
        // Integer-snapped letterbox: place the native-size texture at a
        // whole-pixel margin, so every texel maps to exactly one output
        // pixel (no fractional offset -> no shimmer when the surface
        // wobbles ±1px under a fixed texture). When the texture is
        // larger than the surface the margin floors to 0 and the quad
        // extends past the edge, clipping - matches the prior "host
        // shrank below natural" behavior.
        let sw = surf_w.max(1);
        let sh = surf_h.max(1);
        let left = sw.saturating_sub(self.width) / 2;
        let top = sh.saturating_sub(self.height) / 2;
        // Pixel bounds -> NDC. y is flipped (row 0 = top = +1).
        let l = (left as f32 / sw as f32) * 2.0 - 1.0;
        let r = ((left + self.width) as f32 / sw as f32) * 2.0 - 1.0;
        let t = 1.0 - (top as f32 / sh as f32) * 2.0;
        let b = 1.0 - ((top + self.height) as f32 / sh as f32) * 2.0;
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&l.to_ne_bytes());
        bytes[4..8].copy_from_slice(&t.to_ne_bytes());
        bytes[8..12].copy_from_slice(&r.to_ne_bytes());
        bytes[12..16].copy_from_slice(&b.to_ne_bytes());
        queue.write_buffer(&self.uniform_buf, 0, &bytes);

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("blit-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            ..Default::default()
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        // 4-vertex triangle strip = one quad.
        pass.draw(0..4, 0..1);
    }

    /// Resize the blit texture. Call when the window size changes.
    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        self.texture = Self::create_texture(device, width, height);
        self.bind_group = Self::create_bind_group(
            device,
            &self.bind_group_layout,
            &self.texture,
            &self.sampler,
            &self.uniform_buf,
        );
    }

    fn create_texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("blit-texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    fn create_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        texture: &wgpu::Texture,
        sampler: &wgpu::Sampler,
        uniform_buf: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blit-bg"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform_buf.as_entire_binding(),
                },
            ],
        })
    }
}
