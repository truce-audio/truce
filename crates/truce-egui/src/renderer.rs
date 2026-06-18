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
    /// Adapter-reported `max_texture_dimension_2d`. `resize`
    /// clamps requested physical width / height against this so a
    /// 4K-display drag-resize doesn't trip `surface.configure`'s
    /// validation panic (which on macOS unwinds into the host's
    /// callback and aborts the DAW).
    max_texture_dim: u32,
}

impl EguiRenderer {
    /// Create from a baseview `Window` by bridging its rwh 0.5 handle
    /// to wgpu's rwh 0.6 via `SurfaceTargetUnsafe::RawHandle`.
    ///
    /// # Safety
    /// The window must remain valid for the lifetime of the renderer.
    #[cfg(not(target_os = "ios"))]
    #[must_use]
    pub unsafe fn from_window(window: &baseview::Window, width: u32, height: u32) -> Option<Self> {
        // Zero-sized configure panics inside wgpu. Some hosts (notably
        // VST3 in iZotope's RX shell) hand the editor a zero-extent
        // parent during a transient measurement step before the real
        // open. Mirror `resize`'s zero-guard so we don't crash there.
        if width == 0 || height == 0 {
            return None;
        }
        let instance = wgpu::Instance::new(truce_gui::platform::editor_instance_descriptor());

        let surface = unsafe { crate::platform::create_wgpu_surface(&instance, window)? };

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .ok()?;

        // `downlevel_defaults` caps `max_texture_dimension_2d` at
        // 2048, which on a Retina (2x) display means the editor
        // can't physically exceed 1024 logical points per axis
        // before `surface.configure` panics with a validation
        // error. Take the adapter's actual reported limits
        // instead - Apple Silicon Metal reports 8192 / 16384, x86
        // discrete GPUs typically 8192+, even iGPUs are usually
        // 4096+. This is the same shape JUCE / nih-plug use:
        // trust what the adapter says, then clamp resize requests
        // against it in `resize()` below as belt-and-braces.
        let adapter_limits = adapter.limits();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("truce-egui"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter_limits.clone(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .ok()?;
        // Capture the *first* device-level error (validation / OOM / lost)
        // to the resize log. egui-wgpu's `update_buffers` panics downstream
        // ("Failed to create staging buffer") once the device is poisoned,
        // which only shows the symptom; this names the root cause.
        device.on_uncaptured_error(std::sync::Arc::new(|error| {
            crate::editor::resize_log(&format!("wgpu uncaptured error: {error}"));
        }));
        // Device-lost (GPU TDR / driver reset) does NOT route through
        // `on_uncaptured_error` - wgpu surfaces it only via this callback.
        // The downstream "Failed to create staging buffer" panic is a
        // symptom of the device already being lost; this names the reason.
        device.set_device_lost_callback(|reason, msg| {
            crate::editor::resize_log(&format!("wgpu DEVICE LOST: {reason:?} - {msg}"));
        });
        let max_texture_dim = adapter_limits.max_texture_dimension_2d;

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

        let egui_rpass = egui_wgpu::Renderer::new(
            &device,
            surface_format,
            egui_wgpu::RendererOptions::default(),
        );

        Some(Self {
            device,
            queue,
            surface,
            surface_config,
            egui_rpass,
            width,
            height,
            max_texture_dim,
        })
    }

    /// Create from a raw `CAMetalLayer` pointer (AAX native view path
    /// on macOS, `AUv3` `UIView` path on iOS).
    ///
    /// # Safety
    /// `metal_layer` must be a valid `CAMetalLayer*` that remains alive for
    /// the lifetime of the renderer.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub unsafe fn from_metal_layer(
        metal_layer: *mut std::ffi::c_void,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        // See `from_window` - same zero-guard applies; AAX hands a
        // CAMetalLayer that may not yet have a real bounds size.
        if width == 0 || height == 0 {
            return None;
        }
        unsafe {
            let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
            desc.backends = wgpu::Backends::METAL;
            let instance = wgpu::Instance::new(desc);

            let surface = instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(metal_layer))
                .ok()?;

            let adapter =
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: Some(&surface),
                    force_fallback_adapter: false,
                }))
                .ok()?;

            // `downlevel_defaults` caps `max_texture_dimension_2d`
            // at 2048 - that targets WebGL2-era hardware. iOS / iOS-
            // simulator Metal supports far more (16384+ on modern
            // devices), so an 800x400 logical editor scaled 3× to a
            // 2400x1200 drawable would otherwise trip a validation
            // panic in `Surface::configure`. Request the adapter's
            // reported limits so we never artificially cap below
            // what the device can do.
            let required_limits = adapter.limits();
            let max_texture_dim = required_limits.max_texture_dimension_2d;
            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("truce-egui-aax"),
                    required_features: wgpu::Features::empty(),
                    required_limits,
                    experimental_features: wgpu::ExperimentalFeatures::default(),
                    memory_hints: wgpu::MemoryHints::Performance,
                    trace: wgpu::Trace::Off,
                }))
                .ok()?;
            device.on_uncaptured_error(std::sync::Arc::new(|error| {
                crate::editor::resize_log(&format!("wgpu uncaptured error: {error}"));
            }));

            let surface_caps = surface.get_capabilities(&adapter);
            let surface_format = surface_caps
                .formats
                .iter()
                .find(|f| !f.is_srgb())
                .copied()
                .unwrap_or(surface_caps.formats[0]);
            // iOS-sim Metal's `surface_caps.alpha_modes` may not
            // contain `CompositeAlphaMode::Auto`; configure() then
            // panics. Pick the first reported mode explicitly so
            // we always land on one the surface actually supports.
            let alpha_mode = surface_caps
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto);

            let surface_config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: surface_format,
                width,
                height,
                present_mode: wgpu::PresentMode::AutoVsync,
                desired_maximum_frame_latency: 2,
                alpha_mode,
                view_formats: vec![],
            };
            surface.configure(&device, &surface_config);

            let egui_rpass = egui_wgpu::Renderer::new(
                &device,
                surface_format,
                egui_wgpu::RendererOptions::default(),
            );

            Some(Self {
                device,
                queue,
                surface,
                surface_config,
                egui_rpass,
                width,
                height,
                max_texture_dim,
            })
        }
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

        let (wgpu::CurrentSurfaceTexture::Success(frame)
        | wgpu::CurrentSurfaceTexture::Suboptimal(frame)) = self.surface.get_current_texture()
        else {
            return;
        };
        let frame_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        {
            // `egui_wgpu::Renderer::render` (egui 0.31) takes
            // `&mut RenderPass<'static>`, but `begin_render_pass` returns a
            // pass borrowing `encoder` and `frame_view` lifetimes. wgpu 24
            // exposes `forget_lifetime()` specifically to bridge this -
            // discharging the borrow checker's view without changing the
            // GPU contract (the inner scope still ends before `encoder`
            // is consumed by `submit`). The egui-wgpu API is the
            // constraint here; nothing on our side to fix until egui
            // drops the `'static` requirement.
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
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
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

    /// Resize the surface. Clamps against the adapter-reported
    /// `max_texture_dimension_2d` so a wide drag-resize on a high-
    /// DPI display can't trip `surface.configure`'s validation
    /// panic (which previously unwound through Reaper's CLAP
    /// callback as a foreign C++ exception and aborted the host).
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let width = width.min(self.max_texture_dim);
        let height = height.min(self.max_texture_dim);
        self.width = width;
        self.height = height;
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
    }
}
