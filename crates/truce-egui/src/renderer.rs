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

/// Result of [`EguiRenderer::from_window`].
///
/// On Windows the wgpu init (adapter enumeration + device creation)
/// runs on a worker thread: a broken graphics driver can block those
/// calls in the kernel forever, and they used to run inline on the
/// host's GUI thread - freezing the entire DAW, unkillably. `Ready`
/// is the normal outcome (init finished within the bounded wait);
/// `Deferred` means the worker is still going and `on_frame` should
/// poll the receiver, adopting the renderer if it ever lands. Until
/// then the editor stays blank and the host stays responsive.
// Transient return value, destructured immediately at the single call
// site per editor open - the variant size gap never lives in a stored
// struct, so boxing the renderer would only add an allocation.
#[allow(clippy::large_enum_variant)]
#[cfg(not(target_os = "ios"))]
pub enum RendererInit {
    Ready(Option<EguiRenderer>),
    Deferred(std::sync::mpsc::Receiver<Option<EguiRenderer>>),
}

/// How long `from_window` waits for the GPU-init worker before
/// returning `Deferred`. Healthy DX12 init is well under a second
/// (device + FXC shader compile); the override exists as an escape
/// hatch and to force the deferred path in tests.
#[cfg(target_os = "windows")]
fn gpu_init_timeout() -> std::time::Duration {
    let ms = std::env::var("TRUCE_GPU_INIT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(3000);
    std::time::Duration::from_millis(ms)
}

impl EguiRenderer {
    /// Create from a baseview `Window` by bridging its rwh 0.5 handle
    /// to wgpu's rwh 0.6 via `SurfaceTargetUnsafe::RawHandle`.
    ///
    /// # Safety
    /// The window must remain valid for the lifetime of the renderer.
    /// On Windows it must also outlive a `Deferred` init (the editor
    /// keeps the child window open while the handler polls, so this
    /// holds; a destroyed HWND fails swapchain creation with a driver
    /// error, not UB).
    #[cfg(not(target_os = "ios"))]
    #[must_use]
    pub unsafe fn from_window(
        window: &baseview::Window,
        width: u32,
        height: u32,
        device_lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> RendererInit {
        // Zero-sized configure panics inside wgpu. Some hosts (notably
        // VST3 in iZotope's RX shell) hand the editor a zero-extent
        // parent during a transient measurement step before the real
        // open. Mirror `resize`'s zero-guard so we don't crash there.
        if width == 0 || height == 0 {
            return RendererInit::Ready(None);
        }
        #[cfg(target_os = "windows")]
        {
            use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
            let RawWindowHandle::Win32(handle) = window.raw_window_handle() else {
                return RendererInit::Ready(None);
            };
            let hwnd = handle.hwnd as isize;
            if hwnd == 0 {
                return RendererInit::Ready(None);
            }
            Self::init_guarded(hwnd, width, height, device_lost)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let instance = wgpu::Instance::new(truce_gui::platform::editor_instance_descriptor());
            let renderer = unsafe { crate::platform::create_wgpu_surface(&instance, window) }
                .and_then(|surface| unsafe {
                    Self::init_with_surface(&instance, surface, width, height, device_lost)
                });
            RendererInit::Ready(renderer)
        }
    }

    /// Run the whole wgpu init on a named worker thread and wait a
    /// bounded time for it. The hang this guards against is real: a
    /// mismatched/wedged AMD driver blocks adapter enumeration or
    /// `D3D12CreateDevice` in a kernel call that never returns, and
    /// the blocked thread can't even be killed. Off the GUI thread
    /// that costs one leaked worker and a blank editor; inline it
    /// froze the host.
    #[cfg(target_os = "windows")]
    fn init_guarded(
        hwnd: isize,
        width: u32,
        height: u32,
        device_lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> RendererInit {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("truce-egui-gpu-init".into())
            .spawn(move || {
                // SAFETY: `hwnd` stays valid while the editor's child
                // window is open, which spans any `Deferred` polling
                // (see `from_window`'s safety contract).
                let renderer = unsafe {
                    let instance =
                        wgpu::Instance::new(truce_gui::platform::editor_instance_descriptor());
                    truce_gui::platform::create_wgpu_surface_from_hwnd(&instance, hwnd).and_then(
                        |surface| {
                            Self::init_with_surface(&instance, surface, width, height, device_lost)
                        },
                    )
                };
                // The receiver may already be gone (editor closed, or
                // adoption abandoned after a worker panic) - ignore.
                let _ = tx.send(renderer);
            });
        if spawned.is_err() {
            log::error!("egui gpu init: failed to spawn worker thread");
            return RendererInit::Ready(None);
        }
        match rx.recv_timeout(gpu_init_timeout()) {
            Ok(renderer) => RendererInit::Ready(renderer),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                log::error!(
                    "egui gpu init did not complete within {:?}; opening blank and polling. \
                     A graphics driver blocking device creation is the usual cause",
                    gpu_init_timeout()
                );
                RendererInit::Deferred(rx)
            }
            // Worker panicked (e.g. wgpu validation); treat as failed init.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => RendererInit::Ready(None),
        }
    }

    /// Adapter/device/swapchain setup shared by the inline (macOS,
    /// Linux) and worker-thread (Windows) paths.
    ///
    /// # Safety
    /// `surface` must have been created against a window that outlives
    /// the returned renderer.
    #[cfg(not(target_os = "ios"))]
    unsafe fn init_with_surface(
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
