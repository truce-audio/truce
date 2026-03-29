//! egui-wgpu rendering backend.
//!
//! Manages the wgpu device, surface, and egui-wgpu `Renderer`. Each frame,
//! takes egui's tessellated output and paints it onto the window surface.

/// Wraps wgpu + egui-wgpu for rendering egui output to a window surface.
pub struct EguiRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    egui_rpass: egui_wgpu::Renderer,
    width: u32,
    height: u32,
}

impl EguiRenderer {
    /// Create from a baseview `Window` by bridging its rwh 0.5 handle
    /// to wgpu's rwh 0.6 via `SurfaceTargetUnsafe::RawHandle`.
    ///
    /// # Safety
    /// The window must remain valid for the lifetime of the renderer.
    pub unsafe fn from_window(
        window: &baseview::Window,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });

        let surface = unsafe { crate::platform::create_wgpu_surface(&instance, window)? };

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("truce-egui"),
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

        let egui_rpass = egui_wgpu::Renderer::new(&device, surface_format, None, 1, false);

        Some(Self {
            device,
            queue,
            surface,
            surface_config,
            egui_rpass,
            width,
            height,
        })
    }

    /// Create from a raw CAMetalLayer pointer (AAX native view path).
    ///
    /// # Safety
    /// `metal_layer` must be a valid `CAMetalLayer*` that remains alive for
    /// the lifetime of the renderer.
    #[cfg(target_os = "macos")]
    pub unsafe fn from_metal_layer(
        metal_layer: *mut std::ffi::c_void,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });

        let surface = instance
            .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
                metal_layer,
            ))
            .ok()?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("truce-egui-aax"),
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

        let egui_rpass = egui_wgpu::Renderer::new(&device, surface_format, None, 1, false);

        Some(Self {
            device,
            queue,
            surface,
            surface_config,
            egui_rpass,
            width,
            height,
        })
    }

    /// Paint a frame of egui output to the surface.
    pub fn render(
        &mut self,
        textures_delta: &egui::TexturesDelta,
        clipped_primitives: &[egui::ClippedPrimitive],
        pixels_per_point: f32,
    ) {
        for (id, delta) in &textures_delta.set {
            self.egui_rpass
                .update_texture(&self.device, &self.queue, *id, delta);
        }

        let screen_desc = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.width, self.height],
            pixels_per_point,
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("egui-frame"),
            });

        self.egui_rpass.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            clipped_primitives,
            &screen_desc,
        );

        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => return,
        };
        let frame_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &frame_view,
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

            self.egui_rpass
                .render(&mut pass, clipped_primitives, &screen_desc);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();

        for id in &textures_delta.free {
            self.egui_rpass.free_texture(id);
        }
    }

    /// Resize the surface.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.width = width;
        self.height = height;
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
    }
}
