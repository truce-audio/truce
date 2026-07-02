//! `SlintEditor`: implements `truce_core::Editor` using Slint + baseview + wgpu.
//!
//! On `open()`, creates a baseview child window with a wgpu surface.
//! Each frame, renders the Slint UI to a pixel buffer via `SoftwareRenderer`,
//! uploads it to a wgpu texture, and blits to the surface.
//!
//! Runs the same code path on every macOS host, AAX included.

use std::iter;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions, WindowScalePolicy};
use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor};
use slint::platform::{PointerEventButton, WindowAdapter, WindowEvent};
use slint::{LogicalPosition, PhysicalSize};

use truce_core::editor::{Editor, PluginContext, RawWindowHandle, ResizeCorrector};
use truce_gui::EditorScale;
use truce_params::Params;

use crate::blit::BlitPipeline;
use crate::platform::{self, ParentWindow};

/// Per-frame sync closure: takes the current `PluginContext` and updates the
/// Slint component's properties. Returned by the editor's `setup` callback.
///
/// Deliberately not `Send`-bounded - Slint's generated UI types contain
/// `Rc<...>` and are `!Send`, so they can only be captured here (the
/// closure stays on whichever thread `setup` was called on, namely
/// baseview's window thread or the screenshot caller's thread).
pub type SyncFn<P> = Box<dyn Fn(&PluginContext<P>)>;

/// Editor `setup` callback: called every time the host re-opens the editor,
/// creates the Slint component, and returns a `SyncFn` that the editor calls
/// each frame to push live param values into the component.
///
/// # Contract
///
/// The `Send + Sync` bound is on the *outer* closure only - required so
/// that `Arc<dyn Fn(...) + Send + Sync>` is itself `Send`, which is in
/// turn required because `SlintEditor: Send` (the `Editor` trait
/// demands it). It does **not** propagate to the `SyncFn` the closure
/// returns: the inner `Box<dyn Fn(&PluginContext<P>)>` is unbounded
/// and is where Slint's `!Send` UI types are meant to live.
///
/// In practice this means the setup closure must:
/// - Construct the Slint component **inside** the closure body
///   (`let ui = MyUi::new()?;`), never capture it from the surrounding
///   environment - that would force the outer closure to be `!Send`
///   and violate this bound.
/// - Capture only `Send + Sync` data in its environment (e.g. plain
///   handles, `Arc<...>`, etc.).
/// - Move the freshly-built Slint component into the returned
///   `SyncFn`, where `!Send` types are fine.
///
/// Both the setup-time outer call and the per-frame returned call run
/// on the same window thread, so no thread crossing actually happens
/// for the Slint values themselves.
pub type SetupFn<P> = Arc<dyn Fn(PluginContext<P>) -> SyncFn<P> + Send + Sync>;

/// Slint-based editor implementing truce's `Editor` trait.
///
/// The developer provides a setup closure that:
/// 1. Creates the Slint component
/// 2. Wires Slint callbacks to `PluginContext` for UI→host parameter changes
/// 3. Returns a per-frame sync closure for host→UI parameter updates
///
/// # Example
///
/// ```ignore
/// SlintEditor::new(params, (400, 300), |state: PluginContext<MyParams>| {
///     let ui = MyPluginUi::new().unwrap();
///     let s = state.clone();
///     ui.on_gain_changed(move |v| s.automate(0u32, v as f64));
///     Box::new(move |state: &PluginContext<MyParams>| {
///         ui.set_gain(state.get_param(0u32) as f32);
///     })
/// })
/// ```
// Several independent one-shot flags (scale mode + host-scale-seen, plus
// the resize/size flags below). They're genuinely distinct booleans, not
// a state enum in disguise, so grouping them would obscure more than help.
#[allow(clippy::struct_excessive_bools)]
pub struct SlintEditor<P: Params + ?Sized> {
    params: Arc<P>,
    size: (u32, u32),
    /// Called on each `open()` to create the Slint component and param bindings.
    /// Must be `Fn` (not `FnOnce`) because the host may close and re-open
    /// the editor window multiple times. See [`SetupFn`] for the
    /// `Send + Sync` rationale.
    setup: SetupFn<P>,
    /// Live content-scale factor, shared with the baseview handler via
    /// [`truce_gui::EditorScale`]. Both `set_scale_factor` (host) and
    /// the `Resized` event handler write here; the handler reads it
    /// each frame and reconfigures the slint window / wgpu surface /
    /// blit pipeline when the value diverges from `last_applied_scale`.
    scale: EditorScale,
    /// Standalone hosts set this (via `set_uses_system_scale`) so the
    /// editor honors the desktop `Xft.dpi` scale on Linux; plugins leave
    /// it false and drive scale from the host instead. See
    /// [`truce_gui::platform::editor_window_scale`]. No effect off Linux.
    use_system_scale: bool,
    /// Whether the host announced a content scale via `set_scale_factor`.
    /// On Linux this gates whether an embedded editor trusts `scale`
    /// (host-announced) or defaults to 1.0.
    host_scale_set: bool,
    window: Option<baseview::WindowHandle>,
    /// Pending logical size shared with the baseview handler. Packed
    /// as `(width << 32) | height`. `Editor::set_size` writes here;
    /// the handler's `on_frame` picks the change up and runs the
    /// same `set_size` / `surface.configure` / blit-resize sequence
    /// the `WindowEvent::Resized` branch uses for OS-driven drags.
    /// `0` is the sentinel "no resize pending."
    pending_size: Arc<AtomicU64>,
    /// Resize-capability + constraints surfaced through the `Editor`
    /// trait. Defaults to `can_resize = false`; opt in with
    /// `.resizable(true)`. Constraints feed CLAP
    /// `gui_get_resize_hints` and VST3 `checkSizeConstraint` so
    /// hosts honour the editor's limits.
    can_resize: bool,
    /// Whether the standalone host may maximize the window, exposed
    /// via `Editor::can_maximize`. Defaults to `false`; only consulted
    /// for resizable editors. Opt in with `.maximizable(true)` for
    /// editors that render correctly at any size.
    can_maximize: bool,
    min_size: (u32, u32),
    max_size: (u32, u32),
    aspect_ratio: Option<(u32, u32)>,
    prefers_pow2: bool,
}

/// Pack a `(width, height)` into a single `u64` for the
/// `pending_size` `AtomicU64` handoff to the baseview handler. `0`
/// in both halves is the sentinel "no resize pending."
#[inline]
fn pack_size(size: (u32, u32)) -> u64 {
    (u64::from(size.0) << 32) | u64::from(size.1)
}

#[inline]
fn unpack_size(packed: u64) -> (u32, u32) {
    #[allow(clippy::cast_possible_truncation)]
    {
        ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32)
    }
}

/// Extract a readable message from a `catch_unwind` panic payload.
fn panic_message(e: &(dyn std::any::Any + Send)) -> String {
    e.downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| e.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// Created wgpu state for a slint editor surface: the device/queue/surface
/// plus its configuration. Shared shape between `open` and device-loss
/// recovery.
struct SlintWgpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
}

/// Build the wgpu device + surface for `window` at the given physical size.
/// `device_lost` is raised by the device's lost callback so `on_frame` can
/// rebuild. Returns `None` (editor stays blank, host survives) on any failure.
fn build_wgpu(
    window: &Window,
    phys_w: u32,
    phys_h: u32,
    device_lost: Arc<AtomicBool>,
) -> Option<SlintWgpu> {
    let instance = wgpu::Instance::new(truce_gui::platform::editor_instance_descriptor());
    let surface = unsafe { platform::create_wgpu_surface(&instance, window) }?;
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    }))
    .ok()?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("truce-slint"),
        required_features: wgpu::Features::empty(),
        required_limits: adapter.limits(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::Performance,
        trace: wgpu::Trace::Off,
    }))
    .ok()?;
    device.set_device_lost_callback(move |reason, msg| {
        device_lost.store(true, Ordering::Release);
        log::warn!("slint wgpu device lost: {reason:?} - {msg}");
    });

    let caps = surface.get_capabilities(&adapter);
    let format = caps
        .formats
        .iter()
        .find(|f| f.is_srgb())
        .copied()
        .unwrap_or(caps.formats[0]);
    let surface_config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: phys_w.max(1),
        height: phys_h.max(1),
        // Windows: `on_frame` runs on the host's GUI thread, and a
        // Fifo (AutoVsync) present blocks that thread when the
        // child-window swapchain backs up - freezing the host (REAPER).
        // baseview now drives frames at a steady cadence, so a blocking
        // present stalls every frame instead of rarely; present
        // non-blocking here to keep the host's message loop alive.
        // Matches truce-iced / truce-egui. Other platforms keep vsync.
        #[cfg(target_os = "windows")]
        present_mode: wgpu::PresentMode::AutoNoVsync,
        #[cfg(not(target_os = "windows"))]
        present_mode: wgpu::PresentMode::AutoVsync,
        desired_maximum_frame_latency: 2,
        alpha_mode: wgpu::CompositeAlphaMode::Auto,
        view_formats: vec![],
    };
    surface.configure(&device, &surface_config);
    Some(SlintWgpu {
        device,
        queue,
        surface,
        surface_config,
    })
}

// SAFETY: `baseview::WindowHandle` holds a raw native window pointer
// (HWND / NSView / X11 Window) and is not auto-`Send`. Hosts call
// `Editor::open` / `idle` / `close` from a single dedicated GUI thread
// - never concurrently and never from the audio thread - so the
// handle is only ever touched on the thread that created it. The
// `Editor` trait requires `Send` so the editor can live behind a
// trait object; this impl asserts that the type doesn't escape its
// thread in practice. The `setup` closure is already `Send +
// Sync`-bounded at construction.
unsafe impl<P: Params + ?Sized> Send for SlintEditor<P> {}

impl<P: Params + 'static> SlintEditor<P> {
    /// Create a Slint editor.
    ///
    /// `size` is the window size in logical points. `setup` is called
    /// on the UI thread each time the editor opens. It must create a
    /// fresh Slint component and return a per-frame sync closure.
    pub fn new(
        params: Arc<P>,
        size: (u32, u32),
        setup: impl Fn(PluginContext<P>) -> SyncFn<P> + Send + Sync + 'static,
    ) -> Self {
        Self {
            params,
            size,
            setup: Arc::new(setup),
            scale: EditorScale::new(truce_gui::backing_scale()),
            use_system_scale: false,
            host_scale_set: false,
            window: None,
            pending_size: Arc::new(AtomicU64::new(0)),
            can_resize: false,
            can_maximize: false,
            min_size: (1, 1),
            max_size: (u32::MAX, u32::MAX),
            aspect_ratio: None,
            prefers_pow2: false,
        }
    }

    /// Opt out of host-driven resizing. Slint editors default to
    /// resizable because the markup re-flows for free; pass
    /// `false` for plugins that ship a deliberately fixed-size GUI.
    #[must_use]
    pub fn resizable(mut self, resizable: bool) -> Self {
        self.can_resize = resizable;
        self
    }

    /// Opt into the standalone host's maximize button. Defaults to
    /// `false` (maximize is removed for resizable editors so the window
    /// can't grow past the editor's bounds into an empty margin); pass
    /// `true` for editors that render correctly at any size. Only the
    /// standalone host consults this, and only when `resizable(true)`.
    #[must_use]
    pub fn maximizable(mut self, maximizable: bool) -> Self {
        self.can_maximize = maximizable;
        self
    }

    /// Minimum logical-point dimensions surfaced to the wrappers.
    #[must_use]
    pub fn min_size(mut self, min: (u32, u32)) -> Self {
        self.min_size = min;
        self
    }

    /// Maximum logical-point dimensions surfaced to the wrappers.
    #[must_use]
    pub fn max_size(mut self, max: (u32, u32)) -> Self {
        self.max_size = max;
        self
    }

    /// Lock the aspect ratio as `(numerator, denominator)`.
    #[must_use]
    pub fn aspect_ratio(mut self, ratio: Option<(u32, u32)>) -> Self {
        self.aspect_ratio = ratio;
        self
    }

    #[must_use]
    pub fn prefers_pow2(mut self, prefers: bool) -> Self {
        self.prefers_pow2 = prefers;
        self
    }
}

// Baseview WindowHandler

struct SlintWindowHandler<P: Params + ?Sized> {
    slint_window: Rc<MinimalSoftwareWindow>,
    sync_fn: SyncFn<P>,
    state: PluginContext<P>,
    blit: Option<BlitPipeline>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    px_buf: Vec<PremultipliedRgbaColor>,
    rgba_buf: Vec<u8>,
    width: u32,
    height: u32,
    /// Shared with the parent `SlintEditor`; both `set_scale_factor`
    /// (host) and the `Resized` handler write here. `on_frame`
    /// compares against `last_applied_scale` to pick up host-driven
    /// changes that didn't come through a `Resized` event.
    scale: EditorScale,
    last_applied_scale: f32,
    /// Cached physical extents derived from `(width, height,
    /// last_applied_scale)`. Updated only when the scale-change branch
    /// fires - `on_frame`'s render path reads these directly instead
    /// of re-calling `to_physical_px` twice per frame.
    last_phys_w: u32,
    last_phys_h: u32,
    /// Paces paints to the compositor's measured consumption rate so
    /// the per-tick render/blit can't park the host's GUI thread in
    /// the swapchain acquire - see [`truce_gui::PaintPacer`].
    pacer: truce_gui::PaintPacer,
    /// Last known cursor position in logical points.
    last_pos: LogicalPosition,
    /// Shared with the parent `SlintEditor`. `Editor::set_size`
    /// writes a packed `(w, h)` here; `on_frame` swaps it back to
    /// `0` and applies the same resize sequence the
    /// `WindowEvent::Resized` branch runs for OS-driven drags.
    pending_size: Arc<AtomicU64>,
    /// Raised by the device's lost callback (or a swallowed render panic).
    /// Polled in `on_frame`, which rebuilds the wgpu device/surface/blit.
    device_lost: Arc<AtomicBool>,
    /// Constraint copy from the parent `SlintEditor`, applied to
    /// host-driven `Resized` events that bypassed the format's
    /// negotiation hooks (Linux hosts resizing the embed window
    /// directly), plus the corrective push-back guard.
    min_size: (u32, u32),
    max_size: (u32, u32),
    aspect_ratio: Option<(u32, u32)>,
    resize_corrector: ResizeCorrector,
}

/// Wraps the live handler so a wgpu init failure at `open()` time
/// degrades to a no-op instead of a panic. The open closure runs on
/// baseview's window thread; an `.expect()` there unwinds across
/// baseview's FFI boundary and aborts the host process (a DAW crash).
/// Returning `Dead` leaves the editor blank but keeps the host alive,
/// matching how iced/egui tolerate a failed surface.
enum SlintHandler<P: Params + ?Sized> {
    Live(Box<SlintWindowHandler<P>>),
    Dead,
}

impl<P: Params + ?Sized + 'static> WindowHandler for SlintHandler<P> {
    fn on_frame(&mut self, window: &mut Window) {
        // Catch panics at the FFI boundary: baseview drives this from an
        // `extern "system"` window proc (Windows) / AppKit callback (macOS),
        // so an unwinding panic - e.g. a wgpu device loss mid-resize - would
        // cross a C frame and abort the host. Swallow, and arm recovery so the
        // next frame rebuilds.
        if let Self::Live(handler) = self {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler.on_frame(window)));
            if let Err(e) = result {
                log::error!("slint on_frame panic swallowed: {}", panic_message(&e));
                handler.device_lost.store(true, Ordering::Release);
            }
        }
    }

    fn on_event(&mut self, window: &mut Window, event: Event) -> EventStatus {
        // Catch panics at the FFI boundary, like `on_frame`; report the event
        // as `Ignored` on panic instead of aborting the host.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match self {
            Self::Live(handler) => handler.on_event(window, event),
            Self::Dead => EventStatus::Ignored,
        }));
        match result {
            Ok(status) => status,
            Err(e) => {
                log::error!("slint on_event panic swallowed: {}", panic_message(&e));
                EventStatus::Ignored
            }
        }
    }
}

impl<P: Params + ?Sized> SlintWindowHandler<P> {
    /// Rebuild the wgpu device/surface/blit after a device loss. The Slint
    /// software window is independent of wgpu and is kept; only the GPU side
    /// (and the lazily-recreated blit pipeline) are rebuilt. Returns whether
    /// the rebuild succeeded; on failure the next `on_frame` retries.
    fn recover_device(&mut self, window: &Window) -> bool {
        let device_lost = Arc::new(AtomicBool::new(false));
        let Some(SlintWgpu {
            device,
            queue,
            surface,
            surface_config,
        }) = build_wgpu(
            window,
            self.last_phys_w.max(1),
            self.last_phys_h.max(1),
            device_lost.clone(),
        )
        else {
            return false;
        };
        self.device = device;
        self.queue = queue;
        self.surface = surface;
        self.surface_config = surface_config;
        self.blit = None;
        self.device_lost = device_lost;
        true
    }
}

impl<P: Params + ?Sized + 'static> WindowHandler for SlintWindowHandler<P> {
    fn on_frame(&mut self, window: &mut Window) {
        // Rebuild if the device was lost (flagged by the device-lost callback
        // or a swallowed render panic). Skip the rest of this frame.
        if self.device_lost.load(Ordering::Acquire) {
            let ok = self.recover_device(window);
            log::warn!("slint device-loss recovery: rebuilt ok={ok}");
            return;
        }
        // Skip the whole frame while the editor isn't presentable:
        // detached / occluded on macOS, host child window hidden /
        // minimized on Windows (no-op on Linux). On Windows this runs
        // on the host's GUI thread, so skipping an unpresentable frame
        // keeps a blocking present from freezing the host.
        {
            use raw_window_handle::HasRawWindowHandle;
            if truce_gui::platform::should_skip_frame(window.raw_window_handle()) {
                return;
            }
        }
        // Re-anchor on every frame so the child NSView's origin
        // tracks size changes against the host's plug-in pane.
        // Without this the editor drifts upward as the canvas grows,
        // clipping the layout's top off the visible area.
        #[cfg(target_os = "macos")]
        {
            use raw_window_handle::HasRawWindowHandle;
            truce_gui::platform::reanchor_to_superview_top(window.raw_window_handle());
        }
        // Pick up host-driven `set_size` requests posted to the
        // shared `pending_size` cell since the last frame. Calls
        // `window.resize` (which on Linux / Win32 fires a
        // `Resized` event the existing branch handles, idempotently
        // on macOS where the call only mutates the NSView frame)
        // then reconfigures the slint window + wgpu surface + blit
        // inline so the next render lands at the new size.
        let pending = unpack_size(self.pending_size.swap(0, Ordering::Acquire));
        if pending.0 > 0 && pending.1 > 0 && pending != (self.width, self.height) {
            let scale = f64::from(self.last_applied_scale);
            // Reflow to fill: render the scene, blit texture, and surface all
            // at the same physical extent so the UI fills the window. A
            // host-driven `Resized` handles the window case directly; this
            // path covers programmatic `set_size` (window then follows).
            let phys_w = truce_gui::to_physical_px(pending.0, scale);
            let phys_h = truce_gui::to_physical_px(pending.1, scale);
            window.resize(baseview::Size::new(
                f64::from(pending.0),
                f64::from(pending.1),
            ));
            self.slint_window
                .set_size(slint::WindowSize::Physical(PhysicalSize::new(
                    phys_w, phys_h,
                )));
            self.surface_config.width = phys_w.max(1);
            self.surface_config.height = phys_h.max(1);
            self.surface.configure(&self.device, &self.surface_config);
            if let Some(ref mut blit) = self.blit {
                blit.resize(&self.device, phys_w, phys_h);
            }
            self.width = pending.0;
            self.height = pending.1;
            self.last_phys_w = phys_w;
            self.last_phys_h = phys_h;
        }
        // Pick up host-driven scale changes (CLAP `set_scale`, VST3
        // `IPlugViewContentScaleSupport`) that landed in the shared
        // cell since the last frame. The Resized path applies its own
        // scale changes inline, so this only fires when scale moved
        // without a corresponding window event.
        if let Some(cur_scale) = self.scale.take_change(&mut self.last_applied_scale) {
            let phys_w = truce_gui::to_physical_px(self.width, f64::from(cur_scale));
            let phys_h = truce_gui::to_physical_px(self.height, f64::from(cur_scale));
            self.slint_window
                .window()
                .dispatch_event(WindowEvent::ScaleFactorChanged {
                    scale_factor: cur_scale,
                });
            self.slint_window
                .set_size(slint::WindowSize::Physical(PhysicalSize::new(
                    phys_w, phys_h,
                )));
            self.surface_config.width = phys_w.max(1);
            self.surface_config.height = phys_h.max(1);
            self.surface.configure(&self.device, &self.surface_config);
            if let Some(ref mut blit) = self.blit {
                blit.resize(&self.device, phys_w, phys_h);
            }
            self.last_phys_w = phys_w;
            self.last_phys_h = phys_h;
        }

        // Compositor pacing veto - see `pacer`. Resize / scale
        // handling above still applies during a hold; the render +
        // present below wait for the compositor to catch up.
        if self.pacer.should_hold() {
            return;
        }

        // 1. Drive Slint timers/animations
        slint::platform::update_timers_and_animations();

        // 2. Sync host params → Slint properties
        (self.sync_fn)(&self.state);

        // 3. Force redraw - params/meters change externally every frame
        self.slint_window.request_redraw();

        // 4. Render Slint to pixel buffer. Reuse the cached physical
        // extents - the scale-change branch above is the only writer,
        // so re-multiplying every frame would just duplicate work.
        let phys_w = self.last_phys_w;
        let phys_h = self.last_phys_h;
        platform::render_to_rgba(
            &self.slint_window,
            phys_w,
            phys_h,
            &mut self.px_buf,
            &mut self.rgba_buf,
        );

        // 4. Blit to screen
        let blit = self.blit.get_or_insert_with(|| {
            BlitPipeline::new(&self.device, self.surface_config.format, phys_w, phys_h)
        });

        blit.update(&self.queue, &self.rgba_buf);

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
            match self.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(frame)
                | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                    acquired = Some(frame);
                    break;
                }
                wgpu::CurrentSurfaceTexture::Outdated
                | wgpu::CurrentSurfaceTexture::Lost
                | wgpu::CurrentSurfaceTexture::Validation => {
                    self.surface.configure(&self.device, &self.surface_config);
                }
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    transient_skip = true;
                    break;
                }
            }
        }
        self.pacer.record_acquire(acquire_start.elapsed());
        if transient_skip {
            return;
        }
        let Some(frame) = acquired else {
            return;
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        blit.render(
            &self.queue,
            &mut encoder,
            &view,
            self.surface_config.width,
            self.surface_config.height,
        );
        self.queue.submit(iter::once(encoder.finish()));
        frame.present();
    }

    // `_window` is unused on macOS / Linux - only the Windows
    // ButtonPressed branch reads it, to SetFocus on the child HWND
    // so text widgets see WM_KEYDOWN. Underscore-prefix keeps the
    // unused-arg lint quiet on the non-Windows builds.
    #[allow(clippy::too_many_lines, clippy::used_underscore_binding)]
    fn on_event(&mut self, _window: &mut Window, event: Event) -> EventStatus {
        match event {
            Event::Mouse(mouse) => {
                use baseview::MouseEvent::{
                    ButtonPressed, ButtonReleased, CursorLeft, CursorMoved, WheelScrolled,
                };
                match mouse {
                    CursorMoved { position, .. } => {
                        // Window dimensions stay below 2^23; the f64
                        // → f32 narrowing is invisible.
                        #[allow(clippy::cast_possible_truncation)]
                        let pos = LogicalPosition::new(position.x as f32, position.y as f32);
                        self.last_pos = pos;
                        self.slint_window
                            .window()
                            .dispatch_event(WindowEvent::PointerMoved {
                                position: self.last_pos,
                            });
                        EventStatus::Captured
                    }
                    ButtonPressed { button, .. } => {
                        let Some(button) = convert_mouse_button(button) else {
                            return EventStatus::Ignored;
                        };
                        // WS_CHILD plugin windows don't receive WM_KEYDOWN
                        // until focused; baseview doesn't SetFocus on click,
                        // so we do it here. Without this, text-edit widgets
                        // never see keystrokes on Windows.
                        #[cfg(target_os = "windows")]
                        {
                            if !_window.has_focus() {
                                _window.focus();
                            }
                        }
                        self.slint_window
                            .window()
                            .dispatch_event(WindowEvent::PointerPressed {
                                position: self.last_pos,
                                button,
                            });
                        EventStatus::Captured
                    }
                    ButtonReleased { button, .. } => {
                        let Some(button) = convert_mouse_button(button) else {
                            return EventStatus::Ignored;
                        };
                        self.slint_window
                            .window()
                            .dispatch_event(WindowEvent::PointerReleased {
                                position: self.last_pos,
                                button,
                            });
                        EventStatus::Captured
                    }
                    WheelScrolled { delta, .. } => {
                        let (dx, dy) = match delta {
                            baseview::ScrollDelta::Lines { x, y } => (x * 20.0, y * 20.0),
                            baseview::ScrollDelta::Pixels { x, y } => (x, y),
                        };
                        self.slint_window
                            .window()
                            .dispatch_event(WindowEvent::PointerScrolled {
                                position: self.last_pos,
                                delta_x: dx,
                                delta_y: dy,
                            });
                        EventStatus::Captured
                    }
                    CursorLeft => {
                        self.slint_window
                            .window()
                            .dispatch_event(WindowEvent::PointerExited);
                        EventStatus::Captured
                    }
                    _ => EventStatus::Ignored,
                }
            }
            Event::Window(win) => {
                if let baseview::WindowEvent::Resized(info) = win {
                    let phys_w = info.physical_size().width;
                    let phys_h = info.physical_size().height;
                    let scale = info.scale();
                    truce_gui::platform::note_linux_scale_factor(scale);
                    // Logical dimensions stay below `u32::MAX` and
                    // display scale never exceeds 4.0.
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let (lw, lh) = (
                        (f64::from(phys_w) / scale) as u32,
                        (f64::from(phys_h) / scale) as u32,
                    );
                    // A host that resized the embed window directly never
                    // ran the format's constraint preflight - fit here,
                    // push the corrected size back to the host, and queue
                    // the fitted size through the pending cell so
                    // `on_frame` counter-resizes the child window.
                    let ((fw, fh), correct) = self.resize_corrector.fit(
                        lw,
                        lh,
                        self.min_size,
                        self.max_size,
                        self.aspect_ratio,
                    );
                    if let Some((rw, rh)) = correct {
                        // On Linux, hosts that bypass size negotiation (Bitwig)
                        // ignore this request and react by *growing* the embed
                        // window - a resize loop. Counter-resize our own child
                        // to the fitted size but never ask the host to resize
                        // its frame. mac/windows honor it (and negotiate via
                        // `checkSizeConstraint`) anyway.
                        #[cfg(not(target_os = "linux"))]
                        let _ = self.state.request_resize(rw, rh);
                        #[cfg(target_os = "linux")]
                        let _ = (rw, rh);
                        self.pending_size
                            .store(pack_size((fw, fh)), Ordering::Release);
                    }
                    self.width = lw;
                    self.height = lh;
                    // Mirror the OS-reported scale into the shared
                    // cell (so a follow-up host `set_scale_factor`
                    // reads a fresh baseline) and bump `last_applied`
                    // so `on_frame`'s diff-check stays a no-op - we
                    // apply the reconfigure inline below.
                    self.scale.set(scale);
                    #[allow(clippy::cast_possible_truncation)]
                    let scale_f32 = scale as f32;
                    self.last_applied_scale = scale_f32;

                    self.slint_window
                        .window()
                        .dispatch_event(WindowEvent::ScaleFactorChanged {
                            scale_factor: scale_f32,
                        });
                    self.slint_window
                        .set_size(slint::WindowSize::Physical(PhysicalSize::new(
                            phys_w, phys_h,
                        )));

                    self.surface_config.width = phys_w;
                    self.surface_config.height = phys_h;
                    self.surface.configure(&self.device, &self.surface_config);
                    self.last_phys_w = phys_w;
                    self.last_phys_h = phys_h;

                    if let Some(ref mut blit) = self.blit {
                        blit.resize(&self.device, phys_w, phys_h);
                    }
                }
                EventStatus::Ignored
            }
            Event::Keyboard(kb) => {
                // Keys only arrive when the host grants the editor window OS
                // focus, which varies by DAW. Slint tracks modifier state
                // from the modifier keys' own press/release events, so we
                // forward every key (including Shift/Ctrl/...) verbatim.
                let Some(text) = slint_key_text(&kb.key) else {
                    return EventStatus::Ignored;
                };
                let window = self.slint_window.window();
                match kb.state {
                    keyboard_types::KeyState::Down if kb.repeat => {
                        window.dispatch_event(WindowEvent::KeyPressRepeated { text });
                    }
                    keyboard_types::KeyState::Down => {
                        window.dispatch_event(WindowEvent::KeyPressed { text });
                    }
                    keyboard_types::KeyState::Up => {
                        window.dispatch_event(WindowEvent::KeyReleased { text });
                    }
                }
                EventStatus::Captured
            }
        }
    }
}

// All buttons forward to Slint, not just Left - widgets rely on
// right-click (reset to default) and middle-click. `None` skips buttons
// Slint has no variant for.
fn convert_mouse_button(button: baseview::MouseButton) -> Option<PointerEventButton> {
    match button {
        baseview::MouseButton::Left => Some(PointerEventButton::Left),
        baseview::MouseButton::Right => Some(PointerEventButton::Right),
        baseview::MouseButton::Middle => Some(PointerEventButton::Middle),
        baseview::MouseButton::Back => Some(PointerEventButton::Back),
        baseview::MouseButton::Forward => Some(PointerEventButton::Forward),
        baseview::MouseButton::Other(_) => None,
    }
}

/// Translate a baseview logical key into the text Slint's `WindowEvent`
/// keyboard events carry: printable keys use their character(s); named keys
/// map to the private-use chars from `slint::platform::Key`. Keys Slint
/// doesn't model return `None` and are dropped.
fn slint_key_text(key: &keyboard_types::Key) -> Option<slint::SharedString> {
    use keyboard_types::Key as K;
    use slint::platform::Key as SK;
    let named = match key {
        K::Character(s) => return Some(s.as_str().into()),
        K::Enter => SK::Return,
        K::Tab => SK::Tab,
        K::Backspace => SK::Backspace,
        K::Escape => SK::Escape,
        K::Delete => SK::Delete,
        K::ArrowUp => SK::UpArrow,
        K::ArrowDown => SK::DownArrow,
        K::ArrowLeft => SK::LeftArrow,
        K::ArrowRight => SK::RightArrow,
        K::Home => SK::Home,
        K::End => SK::End,
        K::PageUp => SK::PageUp,
        K::PageDown => SK::PageDown,
        K::Insert => SK::Insert,
        K::ContextMenu => SK::Menu,
        K::Shift => SK::Shift,
        K::Control => SK::Control,
        K::Alt => SK::Alt,
        K::AltGraph => SK::AltGr,
        K::Meta => SK::Meta,
        K::CapsLock => SK::CapsLock,
        K::F1 => SK::F1,
        K::F2 => SK::F2,
        K::F3 => SK::F3,
        K::F4 => SK::F4,
        K::F5 => SK::F5,
        K::F6 => SK::F6,
        K::F7 => SK::F7,
        K::F8 => SK::F8,
        K::F9 => SK::F9,
        K::F10 => SK::F10,
        K::F11 => SK::F11,
        K::F12 => SK::F12,
        _ => return None,
    };
    Some(named.into())
}

// Editor trait

impl<P: Params + 'static> Editor for SlintEditor<P> {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        platform::ensure_platform();

        let (lw, lh) = self.size;
        // Reset the resize-handoff cell so a stale `set_size` from
        // before this open() doesn't immediately re-resize the
        // freshly-built window.
        self.pending_size.store(0, Ordering::Relaxed);
        let pending_size_handle = Arc::clone(&self.pending_size);
        let min_size = self.min_size;
        let max_size = self.max_size;
        let aspect_ratio = self.aspect_ratio;
        // Refresh shared scale from the parent window - on macOS the
        // parent's NSWindow may live on a non-main display whose
        // `backingScaleFactor` differs from `NSScreen.mainScreen`'s.
        // Any `set_scale_factor` the host issues *after* open will
        // override on the next frame via the shared cell.
        // Pick the baseview scale policy. On Linux an embedded plugin
        // follows the host's scale (default 1.0) rather than the desktop
        // Xft.dpi, which a non-DPI-aware host (Bitwig) doesn't share; the
        // standalone and every macOS/Windows path keep SystemScaleFactor.
        let scale_policy = if let Some(s) = truce_gui::platform::editor_window_scale(
            self.use_system_scale,
            self.host_scale_set,
            self.scale.get(),
        ) {
            self.scale.set(s);
            WindowScalePolicy::ScaleFactor(s)
        } else {
            self.scale.set(platform::query_backing_scale(&parent));
            WindowScalePolicy::SystemScaleFactor
        };
        let scale = self.scale.get();
        let typed_ctx = context.with_params(self.params.clone());
        let setup = Arc::clone(&self.setup);
        let scale_handle = self.scale.clone();

        // --- baseview + wgpu ---
        let options = WindowOpenOptions {
            title: String::from("truce-slint"),
            size: baseview::Size::new(f64::from(lw), f64::from(lh)),
            scale: scale_policy,
        };

        let parent_wrapper = ParentWindow(parent);

        let window = baseview::Window::open_parented(
            &parent_wrapper,
            options,
            move |window: &mut Window| {
                // baseview spawns this closure on a new thread; Slint's
                // set_platform is per-thread, so re-register here.
                platform::ensure_platform();

                // Build wgpu. Any failure returns a `Dead` handler rather than
                // panicking - the open closure runs on baseview's thread, so a
                // panic would unwind across its FFI boundary and crash the host.
                let phys_w = truce_gui::to_physical_px(lw, scale);
                let phys_h = truce_gui::to_physical_px(lh, scale);
                let device_lost = Arc::new(AtomicBool::new(false));
                let Some(SlintWgpu {
                    device,
                    queue,
                    surface,
                    surface_config,
                }) = build_wgpu(window, phys_w, phys_h, device_lost.clone())
                else {
                    log::error!("truce-slint: failed to create wgpu state; editor disabled");
                    return SlintHandler::Dead;
                };

                // Create the MinimalSoftwareWindow and register it in the
                // thread-local so the next Slint component creation (inside
                // the setup closure) attaches to it.
                let slint_window = platform::create_slint_window();
                slint_window.set_size(slint::WindowSize::Physical(PhysicalSize::new(
                    phys_w, phys_h,
                )));
                // Display scale never exceeds 4.0.
                #[allow(clippy::cast_possible_truncation)]
                let scale_f32 = scale as f32;
                slint_window
                    .window()
                    .dispatch_event(WindowEvent::ScaleFactorChanged {
                        scale_factor: scale_f32,
                    });

                // Developer creates the Slint component here - it attaches
                // to slint_window via create_window_adapter().
                let sync_fn = setup(typed_ctx.clone());

                SlintHandler::Live(Box::new(SlintWindowHandler::<P> {
                    slint_window,
                    sync_fn,
                    state: typed_ctx,
                    blit: None,
                    device,
                    queue,
                    surface,
                    surface_config,
                    px_buf: Vec::new(),
                    rgba_buf: Vec::new(),
                    width: lw,
                    height: lh,
                    scale: scale_handle,
                    last_applied_scale: scale_f32,
                    last_phys_w: phys_w,
                    last_phys_h: phys_h,
                    pacer: truce_gui::PaintPacer::default(),
                    last_pos: LogicalPosition::default(),
                    pending_size: pending_size_handle,
                    device_lost,
                    min_size,
                    max_size,
                    aspect_ratio,
                    resize_corrector: ResizeCorrector::default(),
                }))
            },
        );

        self.window = Some(window);
    }

    fn set_scale_factor(&mut self, factor: f64) {
        // Write to the shared cell; the baseview handler picks up the
        // change on its next frame and reconfigures the slint window
        // / wgpu surface / blit pipeline. The trait's default no-op
        // would silently swallow host scale changes here.
        self.host_scale_set = true;
        self.scale.set(factor);
    }

    fn set_uses_system_scale(&mut self, yes: bool) {
        self.use_system_scale = yes;
    }

    fn can_resize(&self) -> bool {
        self.can_resize
    }

    fn can_maximize(&self) -> bool {
        self.can_maximize
    }

    fn min_size(&self) -> (u32, u32) {
        self.min_size
    }

    fn max_size(&self) -> (u32, u32) {
        self.max_size
    }

    fn aspect_ratio(&self) -> Option<(u32, u32)> {
        self.aspect_ratio
    }

    fn prefers_pow2(&self) -> bool {
        self.prefers_pow2
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 {
            return false;
        }
        self.size = (width, height);
        // Hand the new logical size to the live baseview handler;
        // it picks the change up at the top of `on_frame` and runs
        // the same `slint_window.set_size` + `surface.configure` +
        // `blit.resize` sequence the `WindowEvent::Resized` branch
        // uses for OS-driven drags.
        self.pending_size
            .store(pack_size((width, height)), Ordering::Release);
        true
    }

    fn close(&mut self) {
        if let Some(mut window) = self.window.take() {
            window.close();
        }
    }

    fn idle(&mut self) {
        // baseview drives its own frame loop.
    }

    fn screenshot(
        &mut self,
        _params: Arc<dyn truce_params::Params>,
    ) -> Option<(Vec<u8>, u32, u32)> {
        let state = truce_core::editor::for_test_params(self.params.clone() as Arc<dyn Params>)
            .with_params(self.params.clone());
        let setup = Arc::clone(&self.setup);
        // Match the live editor's content scale; `EditorScale` falls
        // back to `backing_scale()` for pre-open / headless calls.
        let scale = self.scale.get_f32();
        crate::screenshot::render_with_state::<P>(&state, self.size, scale, move |s| {
            setup(s.clone())
        })
    }
}

impl<P: Params + ?Sized> Drop for SlintEditor<P> {
    fn drop(&mut self) {
        // `baseview::WindowHandle` does not cancel the macOS frame timer
        // on drop, so a host that drops the editor without calling
        // `Editor::close` leaves the timer firing `on_frame`. Unlike the
        // cpu/iced raw-pointer handlers this can't use-after-free (the
        // handler owns its own wgpu surface and an `EditorScale` clone),
        // but it keeps rendering into a torn-down surface. Mirror
        // `close`'s window teardown here; idempotent via
        // `self.window.take()`. (Inlined rather than calling
        // `Editor::close` because that impl requires `P: Sized` while
        // this `Drop` must match the struct's `?Sized`.)
        if let Some(mut window) = self.window.take() {
            window.close();
        }
    }
}
