//! egui-wgpu rendering backend.
//!
//! Manages the wgpu device, surface, and egui-wgpu `Renderer`. Each frame,
//! takes egui's tessellated output and paints it onto the window surface.

/// Owns one frame's [`egui::TexturesDelta`] from `run_ui` until it's
/// rendered, clearing it on drop. epaint's `TexturesDelta::drop`
/// debug-asserts the delta was applied, and a panic between `run_ui` and
/// the end of [`EguiRenderer::render`] (in `tessellate`, or in `render`
/// after its uploads) would otherwise drop it unapplied mid-unwind - an
/// assert inside a `Drop` while panicking aborts the host instead of
/// reaching the editor's panic firewall. On the normal path `render`
/// already cleared it, so this is a no-op. The Windows `FramePacket`
/// does the same for the render thread.
pub(crate) struct OwnedTexturesDelta(pub(crate) egui::TexturesDelta);

impl Drop for OwnedTexturesDelta {
    fn drop(&mut self) {
        self.0.clear();
    }
}

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
    /// How long the last `render` blocked in the swapchain acquire
    /// (`get_current_texture`). The acquire waits until the compositor
    /// frees a frame slot, so this measures how much faster than the
    /// compositor we are painting - the editor's frame pacing reads it
    /// to avoid parking the host GUI thread in that wait.
    last_acquire_wait: std::time::Duration,
}

/// Present mode for the editor's child-window swapchain.
///
/// Windows: `on_frame` runs on the host's GUI thread, and a Fifo
/// (`AutoVsync`) present blocks that thread when the child-window
/// swapchain backs up - freezing the host (REAPER) and risking a
/// GPU-watchdog (TDR) hang. A non-blocking present (`AutoNoVsync`)
/// keeps a slow frame from stalling the host's message loop. Other
/// platforms keep vsync.
fn editor_present_mode() -> wgpu::PresentMode {
    #[cfg(target_os = "windows")]
    {
        wgpu::PresentMode::AutoNoVsync
    }
    #[cfg(not(target_os = "windows"))]
    {
        wgpu::PresentMode::AutoVsync
    }
}

impl EguiRenderer {
    /// Create from a baseview `Window` by bridging its rwh 0.5 handle
    /// to wgpu's rwh 0.6 via `SurfaceTargetUnsafe::RawHandle`.
    ///
    /// macOS / Linux only: on Windows the renderer is built and owned
    /// by the dedicated render thread (`crate::render_thread`), which
    /// keeps every blocking wgpu call - including this init - off the
    /// host's GUI thread.
    ///
    /// # Safety
    /// The window must remain valid for the lifetime of the renderer.
    #[cfg(not(any(target_os = "ios", target_os = "windows")))]
    #[must_use]
    pub unsafe fn from_window(
        window: &baseview::Window,
        width: u32,
        height: u32,
        device_lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<Self> {
        // Zero-sized configure panics inside wgpu. Some hosts (notably
        // VST3 in iZotope's RX shell) hand the editor a zero-extent
        // parent during a transient measurement step before the real
        // open. Mirror `resize`'s zero-guard so we don't crash there.
        if width == 0 || height == 0 {
            return None;
        }
        let instance = wgpu::Instance::new(truce_gui::platform::editor_instance_descriptor());
        let surface = unsafe { crate::platform::create_wgpu_surface(&instance, window) }?;
        unsafe { Self::init_with_surface(&instance, surface, width, height, device_lost) }
    }

    /// Adapter/device/swapchain setup shared by the inline (macOS,
    /// Linux) and render-thread (Windows) paths.
    ///
    /// # Safety
    /// `surface` must have been created against a window that outlives
    /// the returned renderer.
    #[cfg(not(target_os = "ios"))]
    pub(crate) unsafe fn init_with_surface(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
        device_lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<Self> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
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
        // Log the first device-level error (validation / OOM). egui-wgpu's
        // `update_buffers` panics downstream once the device is poisoned, which
        // only shows the symptom; this names the root cause.
        device.on_uncaptured_error(std::sync::Arc::new(|error| {
            log::error!("egui wgpu uncaptured error: {error}");
        }));
        // Device loss (GPU reset) bypasses `on_uncaptured_error`; wgpu reports
        // it only via this callback. Raise the shared flag so the next
        // `on_frame` rebuilds the pipeline instead of rendering against a dead
        // device.
        device.set_device_lost_callback(move |reason, msg| {
            device_lost.store(true, std::sync::atomic::Ordering::Release);
            log::warn!("egui wgpu device lost: {reason:?} - {msg}");
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
            present_mode: editor_present_mode(),
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
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
            last_acquire_wait: std::time::Duration::ZERO,
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
                    apply_limit_buckets: false,
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
                log::error!("egui wgpu uncaptured error: {error}");
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
                present_mode: editor_present_mode(),
                desired_maximum_frame_latency: 2,
                alpha_mode,
                view_formats: vec![],
                color_space: wgpu::SurfaceColorSpace::Auto,
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
                last_acquire_wait: std::time::Duration::ZERO,
            })
        }
    }

    /// Paint a frame of egui output to the surface. Returns `false` when
    /// the frame was skipped (the surface couldn't give a texture), so
    /// the caller schedules a backed-off retry (`SURFACE_RETRY_BACKOFF`)
    /// instead of leaving the editor stale or re-running the UI every
    /// tick.
    pub fn render(
        &mut self,
        textures_delta: &mut egui::TexturesDelta,
        clipped_primitives: &[egui::ClippedPrimitive],
        pixels_per_point: f32,
    ) -> bool {
        // Apply the texture-set uploads first. egui sends each delta only
        // once, so these must run even on a skipped frame (a skip flushes
        // them via `submit`) or the texture update is lost. egui 0.37
        // batches per-id deltas into a `SmallVec`.
        for (id, deltas) in &textures_delta.set {
            for delta in deltas {
                self.egui_rpass
                    .update_texture(&self.device, &self.queue, *id, delta);
            }
        }

        // Acquire the swapchain texture *before* writing the vertex /
        // index buffers. `update_buffers` and `update_texture` allocate
        // staging buffers that wgpu frees only on the next `submit`; a
        // failed acquire that returned early after `update_buffers`
        // stranded them, leaking GBs/min until the host was OOM-killed.
        let acquire_start = std::time::Instant::now();
        let acquired = self.surface.get_current_texture();
        self.last_acquire_wait = acquire_start.elapsed();

        // A `Suboptimal` surface still paints this frame but must be
        // reconfigured before the next acquire.
        let mut reconfigure = false;
        let frame = match acquired {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                reconfigure = true;
                frame
            }
            // Out-of-date / lost / invalid: reconfigure and skip. Without
            // this the surface never recovered - every later frame failed
            // the acquire, froze the window black, and leaked.
            wgpu::CurrentSurfaceTexture::Outdated
            | wgpu::CurrentSurfaceTexture::Lost
            | wgpu::CurrentSurfaceTexture::Validation => {
                self.surface.configure(&self.device, &self.surface_config);
                self.skip_frame(textures_delta);
                return false;
            }
            // Transient (occluded window / acquire timeout): skip without
            // reconfiguring.
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                self.skip_frame(textures_delta);
                return false;
            }
        };
        let frame_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

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
                            // Black margin around the centered editor content,
                            // matching the built-in editor's blit clear. Any
                            // uncovered window/host background is also black, so
                            // the two read as a single margin rather than a
                            // black + dark-grey double border.
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
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
        self.queue.present(frame);

        for id in &textures_delta.free {
            self.egui_rpass.free_texture(id);
        }
        // egui 0.37's `TexturesDelta` panics on drop with unapplied
        // deltas; everything above is applied, so clear it.
        textures_delta.clear();

        if reconfigure {
            self.surface.configure(&self.device, &self.surface_config);
        }
        true
    }

    /// Flush and discard the GPU work queued for a frame the surface
    /// couldn't take. An (even empty) `submit` frees the staging buffers
    /// wgpu allocated for the texture uploads above - the leak this
    /// method exists to prevent. The texture frees and the delta reset
    /// must still happen (egui won't resend them).
    fn skip_frame(&mut self, textures_delta: &mut egui::TexturesDelta) {
        self.queue.submit(std::iter::empty());
        for id in &textures_delta.free {
            self.egui_rpass.free_texture(id);
        }
        textures_delta.clear();
    }

    /// Resize the surface. Clamps against the adapter-reported
    /// `max_texture_dimension_2d` so a wide drag-resize on a high-
    /// DPI display can't trip `surface.configure`'s validation
    /// panic (which previously unwound through Reaper's CLAP
    /// callback as a foreign C++ exception and aborted the host).
    /// How long the last `render` blocked in the swapchain acquire
    /// waiting for the compositor to free a frame slot.
    #[must_use]
    pub fn acquire_wait(&self) -> std::time::Duration {
        self.last_acquire_wait
    }

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

#[cfg(test)]
mod owned_textures_delta_tests {
    use super::OwnedTexturesDelta;

    /// A panic between `run_ui` and the end of `render` unwinds through an
    /// unapplied delta. epaint's drop assert firing mid-unwind would abort
    /// this test binary; owned by the guard, it's cleared first and the
    /// panic is caught normally.
    #[test]
    fn panic_with_unapplied_delta_unwinds_cleanly() {
        let result = std::panic::catch_unwind(|| {
            let mut delta = egui::TexturesDelta::default();
            delta.free.insert(egui::TextureId::Managed(0));
            assert!(!delta.is_empty(), "test needs a non-empty delta");
            let _owned = OwnedTexturesDelta(delta);
            panic!("render failed after its uploads");
        });
        assert!(result.is_err());
    }
}
