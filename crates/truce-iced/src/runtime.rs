//! Surface-agnostic iced render pipeline shared by the desktop
//! (baseview) and iOS (`CAMetalLayer`) editors.
//!
//! `IcedRuntime` owns the wgpu device/surface/renderer plus the iced
//! `UserInterface` build/update/draw/cache cycle. The windowing host
//! creates the wgpu surface and feeds input events; everything from
//! `init_render` onward is identical across platforms. Only
//! `recover_device` (a baseview-driven GPU-loss rebuild) is desktop-only.

use std::fmt::Debug;
use std::sync::Arc;

use crate::iced::{Color, Event, Point, Size, Task};
use iced_wgpu::wgpu;
use truce_core::editor::PluginContext;
use truce_gui::EditorScale;
use truce_gui::layout::GridLayout;
use truce_params::Params;

use crate::auto_layout;
use crate::param_cache::ParamCache;
use crate::param_message::{Message, ParamMessage};

/// Extract a readable message from a `catch_unwind` panic payload.
#[cfg(not(target_os = "ios"))]
pub(crate) fn panic_message(e: &(dyn std::any::Any + Send)) -> String {
    e.downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| e.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// wgpu backends for the editor surface. DX12 on Windows; Metal on macOS;
/// `PRIMARY` (Vulkan) on Linux.
#[cfg(not(target_os = "ios"))]
pub(crate) fn editor_backends() -> wgpu::Backends {
    #[cfg(target_os = "windows")]
    {
        wgpu::Backends::DX12
    }
    #[cfg(target_os = "macos")]
    {
        wgpu::Backends::METAL
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        wgpu::Backends::PRIMARY
    }
}

// IcedPlugin trait - what plugin authors implement

/// Trait for plugin-specific iced UI logic.
///
/// Plugin authors implement this for full control over the iced view.
/// For zero-code UIs, use `IcedEditor::from_layout()` instead.
pub trait IcedPlugin<P: Params>: Sized + 'static {
    /// Plugin-specific message type. Use `()` if you have no custom messages.
    type Message: Debug + Clone + Send;

    /// Create the initial model.
    fn new(params: Arc<P>) -> Self;

    /// Handle a message (param change or plugin-specific).
    /// Default: no-op.
    fn update(
        &mut self,
        _message: Message<Self::Message>,
        _params: &ParamCache<P>,
        _ctx: &PluginContext<P>,
    ) -> Task<Message<Self::Message>> {
        Task::none()
    }

    /// Event subscriptions (e.g. `crate::iced::keyboard::listen()`,
    /// `crate::iced::event::listen_with`). truce-iced drives the recipes each
    /// frame and routes their messages back through `update`. Default: none.
    fn subscription(&self) -> crate::iced::Subscription<Message<Self::Message>> {
        crate::iced::Subscription::none()
    }

    /// Build the view.
    fn view<'a>(
        &'a self,
        params: &'a ParamCache<P>,
    ) -> crate::iced::Element<'a, Message<Self::Message>>;

    /// Custom theme (default: truce dark).
    fn theme(&self) -> crate::iced::Theme {
        crate::theme::truce_dark_theme()
    }

    /// Window title.
    fn title(&self) -> String {
        String::from("Plugin")
    }

    /// Plugin state was restored (preset recall, undo, session load).
    /// Re-read any cached custom state. Parameter values update automatically.
    fn state_changed(&mut self) {}

    /// Whether the editor should repaint this frame even with no UI
    /// input, param, or meter change. Default `false` - the idle gate
    /// skips redundant frames. Return `true` while the plugin has new
    /// data the runtime can't see (e.g. a lock-free queue drained inside
    /// `view()`) so it's drained and shown promptly instead of waiting
    /// for a stray UI event.
    fn needs_redraw(&self) -> bool {
        false
    }
}

// AutoPlugin - built-in plugin for GridLayout auto mode

/// Built-in `IcedPlugin` that generates a view from a `GridLayout`.
pub struct AutoPlugin {
    pub(crate) layout: GridLayout,
}

impl<P: Params> IcedPlugin<P> for AutoPlugin {
    type Message = (); // No custom messages in auto mode

    fn new(_params: Arc<P>) -> Self {
        panic!("AutoPlugin must be created via IcedEditor::from_layout");
    }

    fn view<'a>(&'a self, params: &'a ParamCache<P>) -> crate::iced::Element<'a, Message<()>> {
        auto_layout::auto_view(&self.layout, params)
    }
}

// IcedProgram - holds the plugin model + the shadow state the runtime
// reads / writes each frame. Used to implement `iced_runtime::Program`,
// but that trait no longer exists in iced 0.14; the runtime drives
// this type directly via `dispatch` / `view`.

pub(crate) struct IcedProgram<P: Params + 'static, M: IcedPlugin<P>> {
    pub(crate) plugin: M,
    pub(crate) param_cache: ParamCache<P>,
    pub(crate) context: PluginContext<P>,
    pub(crate) meter_ids: Vec<u32>,
}

impl<P: Params + 'static, M: IcedPlugin<P>> IcedProgram<P, M> {
    fn apply_param_message(&self, msg: &ParamMessage) {
        match msg {
            ParamMessage::BeginEdit(id) => self.context.begin_edit(*id),
            ParamMessage::SetNormalized(id, val) => self.context.set_param(*id, *val),
            ParamMessage::EndEdit(id) => self.context.end_edit(*id),
            ParamMessage::Batch(msgs) => {
                for m in msgs {
                    self.apply_param_message(m);
                }
            }
        }
    }

    /// Handle a single message: forward param events to the host, sync
    /// the shadow cache on `Tick`, and otherwise hand the message to
    /// the plugin's own `update`. The plugin may return a `Task` -
    /// truce-iced doesn't run an executor for embedded use, so the
    /// task is dropped. Plugin code that needs async work should
    /// thread it through its own host hooks rather than relying on
    /// iced's task runtime.
    pub(crate) fn dispatch(&mut self, message: Message<M::Message>) {
        if let Message::Param(ref param_msg) = message {
            self.apply_param_message(param_msg);
        }

        match message {
            Message::Tick => {
                let _ = self.poll_data();
            }
            other => {
                let _: Task<Message<M::Message>> =
                    self.plugin.update(other, &self.param_cache, &self.context);
            }
        }
    }

    pub(crate) fn view(&self) -> crate::iced::Element<'_, Message<M::Message>> {
        self.plugin.view(&self.param_cache)
    }

    /// Sync the shadow param/meter caches from the host, returning
    /// whether any value moved this frame. Drives the editor's idle
    /// gate: host automation and live meters that change here force a
    /// repaint even with no UI input.
    pub(crate) fn poll_data(&mut self) -> bool {
        let params_changed = !self.param_cache.sync(&self.context).is_empty();
        let meters_changed = self.param_cache.sync_meters(&self.context, &self.meter_ids);
        params_changed || meters_changed
    }
}

// IcedRuntime - active iced state (exists only while editor is open)

pub(crate) struct IcedRuntime<P: Params, M: IcedPlugin<P>> {
    /// Rendering pipeline - initialized lazily when the baseview window
    /// finishes building and a wgpu surface is available.
    pub(crate) render: Option<RenderState<P, M>>,
    /// Current cursor position in logical coordinates.
    pub(crate) cursor_position: Point,
    /// Pending iced events queued by mouse callbacks.
    pub(crate) pending_events: Vec<Event>,
    /// Plugin creation info (consumed during render init).
    pub(crate) program: Option<IcedProgram<P, M>>,
    /// Editor size for viewport.
    pub(crate) size: (u32, u32),
    /// Live scale factor (clone of the editor's). Source of truth for
    /// every render path; written by `Editor::set_scale_factor` and
    /// the baseview `Resized` handler, observed each `tick()`.
    pub(crate) scale: EditorScale,
    /// Last scale value the surface/viewport were configured for. When
    /// `scale.get()` diverges from this, `tick()` reconfigures and
    /// updates this snapshot.
    pub(crate) last_applied_scale: f64,
    /// Custom font's TrueType bytes. Family name is recovered by
    /// `crate::font::apply_font` from the TTF `name` table.
    pub(crate) font: Option<&'static [u8]>,
    /// Set when the wgpu device is lost (GPU reset) or a render panic is
    /// swallowed in `on_frame`. Polled at the top of `on_frame`, which then
    /// rebuilds the render pipeline; without it the editor would render
    /// against a dead device (a frozen / black surface). Shared into
    /// `set_device_lost_callback`, which fires off-thread.
    pub(crate) device_lost: Arc<std::sync::atomic::AtomicBool>,
    /// Subscription runtime: drives `IcedPlugin::subscription` recipes
    /// (keyboard, event listeners). A 1-thread pool polls the recipe
    /// streams; their messages arrive on `sub_rx` and are drained each
    /// frame. `Send` (`ThreadPool`), so `IcedRuntime` stays `Send`.
    pub(crate) sub_runtime: SubRuntime<Message<M::Message>>,
    pub(crate) sub_rx: crate::iced::futures::channel::mpsc::UnboundedReceiver<Message<M::Message>>,
    /// Stable window id stamped on broadcast events (single-window editor).
    pub(crate) window_id: crate::iced::window::Id,
    /// Idle gate: force a full render on the next `tick()` regardless of
    /// input/data state. Set on the first frame, after a resize, and
    /// after a device-loss rebuild.
    pub(crate) force_render: bool,
    /// Idle gate: a widget asked to redraw on the very next frame
    /// (`RedrawRequest::NextFrame`) - keep rendering continuously while
    /// set (active animation).
    pub(crate) animate: bool,
    /// Idle gate: the time a widget asked to be redrawn at
    /// (`RedrawRequest::At`, e.g. a `text_input` caret blink). `tick()`
    /// renders once this instant passes.
    pub(crate) redraw_at: Option<std::time::Instant>,
    /// Paces paints to the compositor's measured consumption rate so
    /// repaint-heavy editors can't park the host's GUI thread in the
    /// swapchain acquire - see [`truce_gui::PaintPacer`].
    pub(crate) pacer: truce_gui::PaintPacer,
    /// Owns the wgpu surface + every blocking swapchain call (see
    /// `crate::pump`); [`Self::adopt_pump`] builds the pipeline from
    /// its init product. Desktop only - iOS keeps the surface inline
    /// in [`RenderState`].
    #[cfg(not(target_os = "ios"))]
    pub(crate) pump: Option<crate::pump::SurfacePump<PumpProduct>>,
    #[cfg(not(target_os = "ios"))]
    pub(crate) client: Option<crate::pump::PumpClient>,
}

/// What the pump's init closure hands back to the GUI thread: the
/// initialized device bundle the iced pipeline is built from in
/// [`IcedRuntime::adopt_pump`]. The iced program/model must never
/// cross threads (no `Send` bound on plugin models), so the pipeline
/// itself is built at adoption, not inside the closure.
#[cfg(not(target_os = "ios"))]
pub(crate) struct PumpProduct {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: wgpu::SurfaceConfiguration,
}

/// The iced subscription runtime, parameterised by the editor's message
/// type: a thread-pool executor plus the channel its recipes publish to.
type SubRuntime<Msg> = iced_runtime::futures::Runtime<
    crate::iced::futures::executor::ThreadPool,
    crate::iced::futures::channel::mpsc::UnboundedSender<Msg>,
    Msg,
>;

/// Holds the full wgpu + iced rendering pipeline.
///
/// Replaces what `iced_runtime::program::State` used to encapsulate
/// in our 0.13 setup: we own the plugin model + the `UserInterface`
/// cache that lets iced reuse layout work between frames, and drive
/// the build / update / draw / extract-cache cycle by hand each
/// `tick()`.
pub(crate) struct RenderState<P: Params + 'static, M: IcedPlugin<P>> {
    /// Cloned wgpu handle for surface (re)configuration. The "primary"
    /// device + queue handles live inside `renderer`'s `Engine`.
    pub(crate) device: wgpu::Device,
    /// iOS only: the surface owned inline (`CAMetalLayer` path), with
    /// every swapchain call on the calling thread. Desktop editors
    /// leave this `None` - the surface lives with the pump
    /// (`IcedRuntime::client`), which on Windows keeps blocking
    /// swapchain calls off the host's GUI thread.
    pub(crate) surface: Option<wgpu::Surface<'static>>,
    pub(crate) surface_config: wgpu::SurfaceConfiguration,
    pub(crate) renderer: iced_wgpu::Renderer,
    pub(crate) program: IcedProgram<P, M>,
    /// `iced_runtime::UserInterface` cache between frames. Holds widget
    /// internal state (focus, scroll positions, ...) so we don't lose
    /// it between layout passes. `None` only mid-`tick()` between
    /// build and extract.
    pub(crate) ui_cache: Option<iced_runtime::user_interface::Cache>,
    /// Most recent mouse interaction reported by the UI's draw pass.
    /// Polled by the baseview handler to update the OS cursor.
    pub(crate) interaction: crate::iced::mouse::Interaction,
    /// Whether a focused widget (e.g. a `text_input`) currently wants
    /// keyboard input, from the UI's last `InputMethod` strategy. The
    /// iOS host drives `becomeFirstResponder` off this to raise/dismiss
    /// the soft keyboard.
    pub(crate) wants_keyboard: bool,
    pub(crate) viewport: iced_graphics::Viewport,
    pub(crate) theme: crate::iced::Theme,
    pub(crate) bg_color: Color,
}

impl<P: Params + 'static, M: IcedPlugin<P>> IcedRuntime<P, M> {
    /// Build a runtime around a plugin program, before any wgpu surface
    /// exists. The windowing host calls [`Self::init_render`] once it has
    /// a surface. Shared by the desktop (baseview) and iOS (`CAMetalLayer`)
    /// editors so the subscription worker + channel wiring stays in one
    /// place.
    pub(crate) fn new(
        size: (u32, u32),
        scale: EditorScale,
        font: Option<&'static [u8]>,
        program: IcedProgram<P, M>,
    ) -> Self {
        // One worker thread polls the subscription recipe streams; idle
        // when no subscription is active.
        let sub_executor = crate::iced::futures::executor::ThreadPool::builder()
            .pool_size(1)
            .create()
            .expect("spawn subscription executor thread");
        let (sub_tx, sub_rx) = crate::iced::futures::channel::mpsc::unbounded();
        Self {
            render: None,
            cursor_position: Point::ORIGIN,
            pending_events: Vec::new(),
            program: Some(program),
            size,
            scale,
            // init_render writes the real value; this placeholder never
            // reaches a render call.
            last_applied_scale: 0.0,
            font,
            device_lost: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            sub_runtime: iced_runtime::futures::Runtime::new(sub_executor, sub_tx),
            sub_rx,
            window_id: crate::iced::window::Id::unique(),
            // Paint the first frame unconditionally; the gate takes over
            // once there's something on screen.
            force_render: true,
            animate: false,
            redraw_at: None,
            pacer: truce_gui::PaintPacer::default(),
            #[cfg(not(target_os = "ios"))]
            pump: None,
            #[cfg(not(target_os = "ios"))]
            client: None,
        }
    }

    /// Spawn the surface pump for the editor window (see `crate::pump`):
    /// GPU init and every blocking swapchain call run there - off the
    /// host's GUI thread on Windows, where a stalled driver used to
    /// freeze the DAW. [`Self::adopt_pump`] builds the iced pipeline
    /// once init lands. Returns whether the pump spawned.
    #[cfg(not(target_os = "ios"))]
    pub(crate) fn spawn_pump(&mut self, window: &baseview::Window) -> bool {
        let (lw, lh) = self.size;
        let render_scale = self.scale.get();
        let w = truce_gui::to_physical_px(lw, render_scale).max(1);
        let h = truce_gui::to_physical_px(lh, render_scale).max(1);
        let lost_flag = self.device_lost.clone();
        // SAFETY: the editor drops the pump (via this runtime) before
        // closing its baseview window.
        let pump = unsafe {
            crate::pump::SurfacePump::spawn(
                window,
                &self.device_lost,
                Box::new(move |_, adapter, surface| {
                    let (device, queue) =
                        match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                            label: Some("truce-iced"),
                            required_features: wgpu::Features::empty(),
                            required_limits: adapter.limits(),
                            experimental_features: wgpu::ExperimentalFeatures::default(),
                            memory_hints: wgpu::MemoryHints::default(),
                            trace: wgpu::Trace::Off,
                        })) {
                            Ok(dq) => dq,
                            Err(e) => {
                                log::error!("failed to create wgpu device: {e}");
                                return None;
                            }
                        };
                    // Raise the shared flag on device loss (GPU reset) so the
                    // next `on_frame` rebuilds the pipeline instead of rendering
                    // against a dead device.
                    device.set_device_lost_callback(move |reason, msg| {
                        lost_flag.store(true, std::sync::atomic::Ordering::Release);
                        log::warn!("iced wgpu device lost: {reason:?} - {msg}");
                    });

                    let surface_caps = surface.get_capabilities(adapter);
                    if surface_caps.formats.is_empty() {
                        log::warn!("no surface formats available");
                        return None;
                    }
                    let surface_format = surface_caps.formats[0];
                    let alpha_mode = if surface_caps
                        .alpha_modes
                        .contains(&wgpu::CompositeAlphaMode::PostMultiplied)
                    {
                        wgpu::CompositeAlphaMode::PostMultiplied
                    } else {
                        surface_caps.alpha_modes[0]
                    };
                    let surface_config = wgpu::SurfaceConfiguration {
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                        format: surface_format,
                        width: w,
                        height: h,
                        // Windows: a Fifo (AutoVsync) present blocks when the
                        // child-window swapchain backs up - freezing the host
                        // (REAPER) when it lands on the GUI thread and risking
                        // a GPU-watchdog (TDR) hang. Non-blocking present
                        // there; other platforms keep vsync.
                        #[cfg(target_os = "windows")]
                        present_mode: wgpu::PresentMode::AutoNoVsync,
                        #[cfg(not(target_os = "windows"))]
                        present_mode: wgpu::PresentMode::AutoVsync,
                        desired_maximum_frame_latency: 2,
                        alpha_mode,
                        view_formats: vec![],
                    };
                    let product = PumpProduct {
                        adapter: adapter.clone(),
                        device: device.clone(),
                        queue,
                        surface_config: surface_config.clone(),
                    };
                    Some((product, device, surface_config))
                }),
            )
        };
        if let Some(p) = pump {
            self.client = Some(p.client());
            self.pump = Some(p);
            true
        } else {
            log::warn!("iced: failed to spawn surface pump; editor stays blank");
            false
        }
    }

    /// Adopt the pump's init product and build the iced pipeline
    /// around it (first tick on macOS / Linux, whenever the pump
    /// thread finishes on Windows - the editor is blank until then
    /// and the host stays responsive throughout).
    #[cfg(not(target_os = "ios"))]
    pub(crate) fn adopt_pump(&mut self) {
        if self.render.is_some() {
            return;
        }
        let Some(pump) = self.pump.as_mut() else {
            return;
        };
        let Some(product) = pump.take_init() else {
            return;
        };
        let PumpProduct {
            adapter,
            device,
            queue,
            surface_config,
        } = product;
        self.finish_init(&adapter, device, queue, surface_config, None);
    }

    /// Reconfigure the surface to a physical size, keeping the local
    /// configuration copy in step. Routes through the pump's client
    /// (queued latest-wins; never blocks on Windows).
    #[cfg(not(target_os = "ios"))]
    pub(crate) fn reconfigure_surface_px(&mut self, pw: u32, ph: u32) {
        if let Some(render) = self.render.as_mut() {
            render.surface_config.width = pw.max(1);
            render.surface_config.height = ph.max(1);
            if let Some(surface) = &render.surface {
                surface.configure(&render.device, &render.surface_config);
            } else if let Some(client) = &self.client {
                client.resize(pw.max(1), ph.max(1));
            }
        }
    }

    /// Initialize the wgpu + iced rendering pipeline from a pre-created
    /// surface. iOS (`CAMetalLayer`) only: the surface is owned inline
    /// by the `RenderState`. Desktop editors go through
    /// [`Self::spawn_pump`] + [`Self::adopt_pump`] instead.
    //
    // `instance` and `surface` are threaded into the iced renderer; the
    // owned-arg shape avoids a clone at the call site.
    #[cfg(target_os = "ios")]
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn init_render(
        &mut self,
        instance: wgpu::Instance,
        surface: wgpu::Surface<'static>,
    ) -> bool {
        let (lw, lh) = self.size;
        // Read from the shared cell (clone of the editor's scale). Re-
        // querying `truce_gui::backing_scale()` would drop a host-
        // supplied value.
        let render_scale = self.scale.get();
        let w = truce_gui::to_physical_px(lw, render_scale);
        let h = truce_gui::to_physical_px(lh, render_scale);

        let adapter =
            match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })) {
                Ok(a) => a,
                Err(e) => {
                    log::warn!("no suitable GPU adapter found: {e}");
                    return false;
                }
            };

        let (device, queue) =
            match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("truce-iced"),
                required_features: wgpu::Features::empty(),
                required_limits: adapter.limits(),
                experimental_features: wgpu::ExperimentalFeatures::default(),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::Off,
            })) {
                Ok(dq) => dq,
                Err(e) => {
                    log::error!("failed to create wgpu device: {e}");
                    return false;
                }
            };
        // Raise the shared flag on device loss (GPU reset) so the next
        // frame rebuilds the pipeline instead of rendering against a dead
        // device. The flag is per-generation (see `recover_device`).
        let lost_flag = self.device_lost.clone();
        device.set_device_lost_callback(move |reason, msg| {
            lost_flag.store(true, std::sync::atomic::Ordering::Release);
            log::warn!("iced wgpu device lost: {reason:?} - {msg}");
        });

        let surface_caps = surface.get_capabilities(&adapter);
        if surface_caps.formats.is_empty() {
            log::warn!("no surface formats available");
            return false;
        }

        let surface_format = surface_caps.formats[0];
        let alpha_mode = if surface_caps
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::PostMultiplied)
        {
            wgpu::CompositeAlphaMode::PostMultiplied
        } else {
            surface_caps.alpha_modes[0]
        };

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: w.max(1),
            height: h.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &surface_config);

        self.finish_init(&adapter, device, queue, surface_config, Some(surface))
    }

    /// Resize the runtime to a new logical size (iOS host-driven
    /// resize). Updates the cached logical size, reconfigures the inline
    /// wgpu surface to the new physical pixel count, and rebuilds the
    /// iced viewport so the next frame lays out against the new size.
    /// No-op until the render pipeline is live.
    #[cfg(target_os = "ios")]
    pub(crate) fn resize(&mut self, logical_w: u32, logical_h: u32) {
        self.size = (logical_w, logical_h);
        let render_scale = self.scale.get();
        let pw = truce_gui::to_physical_px(logical_w, render_scale).max(1);
        let ph = truce_gui::to_physical_px(logical_h, render_scale).max(1);
        let Some(render) = self.render.as_mut() else {
            return;
        };
        render.surface_config.width = pw;
        render.surface_config.height = ph;
        if let Some(surface) = &render.surface {
            surface.configure(&render.device, &render.surface_config);
        }
        #[allow(clippy::cast_possible_truncation)] // display DPI; bounded
        let scale_f32 = render_scale as f32;
        render.viewport = iced_graphics::Viewport::with_physical_size(Size::new(pw, ph), scale_f32);
        // The idle gate is exempt on iOS (every tick renders), but flag a
        // forced paint so the reflow lands even if that ever changes.
        self.force_render = true;
    }

    /// Build the iced `Engine` / renderer / [`RenderState`] around an
    /// initialized device. `surface` is `Some` on iOS (inline
    /// swapchain) and `None` on desktop, where the pump owns it.
    fn finish_init(
        &mut self,
        adapter: &wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        mut surface_config: wgpu::SurfaceConfiguration,
        surface: Option<wgpu::Surface<'static>>,
    ) -> bool {
        let Some(program) = self.program.take() else {
            return false;
        };

        let (lw, lh) = self.size;
        let render_scale = self.scale.get();
        self.last_applied_scale = render_scale;
        let w = truce_gui::to_physical_px(lw, render_scale).max(1);
        let h = truce_gui::to_physical_px(lh, render_scale).max(1);
        // The configuration was sized when init started; a host resize
        // or scale change may have landed since (on Windows init runs
        // on the pump thread). Re-sync before the first paint.
        if (surface_config.width, surface_config.height) != (w, h) {
            surface_config.width = w;
            surface_config.height = h;
            if let Some(s) = &surface {
                s.configure(&device, &surface_config);
            }
            #[cfg(not(target_os = "ios"))]
            if surface.is_none()
                && let Some(client) = &self.client
            {
                client.resize(w, h);
            }
        }

        // wgpu::Device / Queue are cheaply Clone-able (internally Arc'd);
        // hand the canonical pair to `Engine::new` and keep clones for
        // post-init surface reconfiguration.
        let surface_device = device.clone();
        let engine = iced_wgpu::Engine::new(
            adapter,
            device,
            queue,
            surface_config.format,
            Some(iced_graphics::Antialiasing::MSAAx4),
            iced_graphics::Shell::headless(),
        );

        let default_font = if let Some(data) = self.font {
            crate::font::apply_font(data)
        } else {
            crate::iced::Font::DEFAULT
        };
        let renderer = iced_wgpu::Renderer::new(engine, default_font, crate::iced::Pixels(14.0));

        // Scale is a display DPI factor (typically 1.0..=3.0); the
        // narrowing here is a documented host convention loss, not a
        // numeric overflow.
        #[allow(clippy::cast_possible_truncation)]
        let viewport =
            iced_graphics::Viewport::with_physical_size(Size::new(w, h), render_scale as f32);
        let theme = program.plugin.theme();

        let bg = crate::theme::truce_dark_theme().palette().background;

        self.render = Some(RenderState {
            device: surface_device,
            surface,
            surface_config,
            renderer,
            program,
            ui_cache: Some(iced_runtime::user_interface::Cache::new()),
            interaction: crate::iced::mouse::Interaction::default(),
            wants_keyboard: false,
            viewport,
            theme,
            bg_color: bg,
        });

        // The fresh pipeline must paint even if the idle gate sees no
        // other change.
        self.force_render = true;
        log::info!("gpu active (wgpu, {w}x{h})");
        true
    }

    /// Rebuild the pump + renderer after a device loss, salvaging the
    /// plugin program. Widget state in `ui_cache` is lost. Returns
    /// whether the pump respawned; the pipeline itself is rebuilt by
    /// `adopt_pump` once the new init lands.
    #[cfg(not(target_os = "ios"))]
    pub(crate) fn recover_device(&mut self, window: &baseview::Window) -> bool {
        // Give the new device generation a fresh lost-flag so the dying
        // device's own callback can't re-arm recovery and cause a redundant
        // second rebuild; `spawn_pump` clones this into the new callback.
        self.device_lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The rebuilt surface starts blank - force a paint on the next
        // tick even if the idle gate would otherwise skip it.
        self.force_render = true;
        // Drop the old pump/device/renderer; keep the program.
        if let Some(RenderState { program, .. }) = self.render.take() {
            self.program = Some(program);
        }
        self.pump = None;
        self.client = None;
        self.spawn_pump(window)
    }

    /// Drive one frame: update iced state + present to surface.
    // One frame's full pipeline (sync, gate, build, draw, present) in
    // source order; splitting it would scatter the borrow of `render`.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn tick(&mut self) {
        // Adopt the pump's GPU init product once it lands (first tick
        // on macOS / Linux; whenever the pump thread finishes on
        // Windows - blank but responsive until then).
        #[cfg(not(target_os = "ios"))]
        self.adopt_pump();
        // Compositor pacing veto - see `pacer`. Everything this tick
        // would do (state sync, view rebuild, present) is deferred a
        // few ticks; input events stay queued in `pending_events` and
        // subscription messages stay in the channel, so nothing is
        // lost, and the paced paint drains them. Windows skips the
        // veto: the pump pre-acquires frames off-thread and
        // `try_take_frame` returning `None` already paces paints to
        // the compositor, so holding here only adds latency (a
        // resize-time acquire stall inflates the pace estimate for
        // seconds).
        if cfg!(not(target_os = "windows")) && self.pacer.should_hold() {
            return;
        }
        let Some(render) = self.render.as_mut() else {
            return;
        };

        // Pick up host-driven scale changes (CLAP `set_scale`, VST3
        // `IPlugViewContentScaleSupport`) that landed in the shared
        // cell since the last frame. The Resized path applies its own
        // scale changes inline so this branch only fires when scale
        // moved without a corresponding window event.
        //
        // Bit-level comparison rather than `!=` so the implicit
        // invariant - "values come through `EditorScale::set` /
        // `.get()`, both of which round-trip via `to_bits` /
        // `from_bits`, so equal inputs produce equal stored bits" -
        // is explicit at the comparison site. `2.0 != 2.0` would
        // never be true via this path today, but a clippy lint and
        // a future refactor that narrowed the type to `f32` somewhere
        // could turn the implicit guarantee into an actual NaN-flavored
        // bug.
        let cur_scale = self.scale.get();
        let scale_changed = cur_scale.to_bits() != self.last_applied_scale.to_bits();
        if scale_changed {
            let (lw, lh) = self.size;
            let pw = truce_gui::to_physical_px(lw, cur_scale);
            let ph = truce_gui::to_physical_px(lh, cur_scale);
            render.surface_config.width = pw;
            render.surface_config.height = ph;
            if let Some(surface) = &render.surface {
                surface.configure(&render.device, &render.surface_config);
            }
            #[cfg(not(target_os = "ios"))]
            if render.surface.is_none()
                && let Some(client) = &self.client
            {
                client.resize(pw.max(1), ph.max(1));
            }
            #[allow(clippy::cast_possible_truncation)] // display DPI; bounded
            let scale_f32 = cur_scale as f32;
            render.viewport =
                iced_graphics::Viewport::with_physical_size(Size::new(pw, ph), scale_f32);
            self.last_applied_scale = cur_scale;
        }

        // Sync params + meters from the host first (cheap atomic reads)
        // so the view rebuilt below sees fresh shadow values, and learn
        // whether any host-side value moved this frame.
        let data_changed = render.program.poll_data();

        // Drain subscription messages that arrived since the last frame
        // into a holding buffer. A non-empty drain forces a render so
        // time-driven subscriptions (e.g. `iced::time::every`) aren't
        // stalled by the idle gate; the messages are dispatched below.
        let mut queued_sub_msgs: Vec<Message<M::Message>> = Vec::new();
        while let Ok(message) = self.sub_rx.try_recv() {
            queued_sub_msgs.push(message);
        }

        // Idle gate: skip the whole frame - no view rebuild, no GPU
        // present - when nothing needs redrawing. This is what keeps the
        // host responsive: baseview's frame timer still fires on the
        // host's GUI thread every tick, but an idle editor returns
        // immediately instead of rebuilding + presenting. Errs toward
        // rendering; any uncertainty paints.
        let timer_due = self
            .redraw_at
            .is_some_and(|t| std::time::Instant::now() >= t);
        // iOS is exempt: it's driven by `CADisplayLink` (no host
        // message pump to free), and its per-frame `RedrawRequested`
        // re-issues `request_input_method` to keep the soft keyboard up,
        // so every frame must run.
        let should_render = cfg!(target_os = "ios")
            || self.force_render
            || scale_changed
            || !self.pending_events.is_empty()
            || data_changed
            || self.animate
            || timer_due
            || render.program.plugin.needs_redraw()
            || !queued_sub_msgs.is_empty();
        if !should_render {
            return;
        }
        self.force_render = false;

        let cursor = crate::iced::mouse::Cursor::Available(self.cursor_position);
        let logical_size = render.viewport.logical_size();
        let style = iced_runtime::core::renderer::Style {
            text_color: Color::from_rgb(0.90, 0.90, 0.92),
        };

        // Build the user interface for this frame from the current
        // model. The borrow of `render.program` is dropped at
        // `into_cache()`, after which we can re-enter `dispatch` for
        // each collected message.
        let mut messages: Vec<Message<M::Message>> = Vec::new();
        let cache = render
            .ui_cache
            .take()
            .unwrap_or_else(iced_runtime::user_interface::Cache::new);
        let view_element = render.program.view();
        let mut user_interface = iced_runtime::UserInterface::build(
            view_element,
            logical_size,
            cache,
            &mut render.renderer,
        );

        let mut pending_events = std::mem::take(&mut self.pending_events);
        // Feed a per-frame `RedrawRequested` like iced_winit does: focused
        // widgets re-evaluate on it (text_input blinks its caret and, while
        // focused, re-issues its `request_input_method` - the signal the iOS
        // host reads to keep the soft keyboard up). Without it, on a frame
        // with no input events nothing requests IME and the keyboard would
        // drop. Appended last so it observes focus set by this frame's input.
        pending_events.push(Event::Window(crate::iced::window::Event::RedrawRequested(
            std::time::Instant::now(),
        )));
        let (ui_state, statuses) = user_interface.update(
            &pending_events,
            cursor,
            &mut render.renderer,
            &mut iced_runtime::core::clipboard::Null,
            &mut messages,
        );
        // `UserInterface::update` is where the mouse interaction is
        // reported in iced 0.14 (0.13 returned it from `draw`).
        // `Outdated` means the widget tree changed and we'd want to
        // rebuild for accuracy; defer to the next frame and keep the
        // previous interaction value in the meantime.
        if let iced_runtime::user_interface::State::Updated {
            mouse_interaction,
            input_method,
            redraw_request,
            ..
        } = ui_state
        {
            render.interaction = mouse_interaction;
            // `InputMethod::Enabled` means a focused widget wants text
            // input; the iOS host raises the soft keyboard on this.
            render.wants_keyboard =
                matches!(input_method, crate::iced::InputMethod::Enabled { .. });
            // Feed the idle gate: a widget that wants to animate asks for
            // the next frame (`NextFrame`) or a specific time (`At`, e.g.
            // a caret blink); `Wait` means it's idle until input.
            match redraw_request {
                crate::iced::window::RedrawRequest::NextFrame => {
                    self.animate = true;
                    self.redraw_at = None;
                }
                crate::iced::window::RedrawRequest::At(t) => {
                    self.animate = false;
                    self.redraw_at = Some(t);
                }
                crate::iced::window::RedrawRequest::Wait => {
                    self.animate = false;
                    self.redraw_at = None;
                }
            }
        } else {
            // `Outdated`: the widget tree changed under us; rebuild and
            // repaint next frame rather than trusting this frame's state.
            self.force_render = true;
        }

        user_interface.draw(&mut render.renderer, &render.theme, &style, cursor);

        render.ui_cache = Some(user_interface.into_cache());

        // Subscription pump: keep `IcedPlugin::subscription` recipes tracked
        // and broadcast this frame's events to them, so `keyboard::listen` /
        // `event::listen_with` fire. The worker thread polls the streams, so
        // their messages may land a frame later; drain whatever is ready and
        // fold it in with the widget messages.
        let recipes =
            iced_runtime::futures::subscription::into_recipes(render.program.plugin.subscription());
        self.sub_runtime.track(recipes);
        for (event, status) in pending_events.iter().zip(&statuses) {
            self.sub_runtime
                .broadcast(iced_runtime::futures::subscription::Event::Interaction {
                    window: self.window_id,
                    event: event.clone(),
                    status: *status,
                });
        }
        while let Ok(message) = self.sub_rx.try_recv() {
            messages.push(message);
        }
        // Subscription messages drained before the idle gate (so they
        // could trigger this render) still need dispatching.
        messages.append(&mut queued_sub_msgs);

        // Now we can mutate the program again - drain any messages the
        // event handlers produced.
        for message in messages {
            render.program.dispatch(message);
        }

        // Present: get surface texture, render, submit. iced 0.14's
        // `Renderer::present` builds its own encoder + submits to the
        // queue internally, so we no longer manage either by hand.
        let inline_surface = render.surface.is_some();
        let frame = if let Some(surface) = &render.surface {
            // iOS: acquire inline on the calling thread.
            let acquire_start = std::time::Instant::now();
            let acquire_result = surface.get_current_texture();
            self.pacer.record_acquire(acquire_start.elapsed());
            match acquire_result {
                Ok(f) => f,
                Err(wgpu::SurfaceError::Timeout | wgpu::SurfaceError::Outdated) => {
                    surface.configure(&render.device, &render.surface_config);
                    return;
                }
                Err(e) => {
                    log::warn!("surface error: {e}");
                    return;
                }
            }
        } else {
            // Desktop: take the pump's frame. On Windows this never
            // blocks (the pump pre-acquires on its own thread).
            #[cfg(target_os = "ios")]
            return;
            #[cfg(not(target_os = "ios"))]
            {
                let Some(client) = &self.client else {
                    return;
                };
                let frame = client.try_take_frame();
                self.pacer.record_acquire(client.last_acquire_wait());
                let Some(frame) = frame else {
                    // Pump still acquiring, or a transient error - the
                    // frame's CPU work is done; repaint once one is
                    // ready instead of waiting for the next change.
                    self.force_render = true;
                    return;
                };
                if (frame.texture.width(), frame.texture.height())
                    != (render.surface_config.width, render.surface_config.height)
                {
                    // A resize raced the acquire; discard the stale-
                    // size frame (the pump reconfigures + reacquires).
                    client.discard(frame);
                    self.force_render = true;
                    return;
                }
                frame
            }
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let _ = render.renderer.present(
            Some(render.bg_color),
            render.surface_config.format,
            &view,
            &render.viewport,
        );

        if inline_surface {
            frame.present();
            return;
        }
        #[cfg(not(target_os = "ios"))]
        if let Some(client) = &self.client {
            client.present(frame);
        }
    }

    /// Queue a cursor move event. Coordinates are in logical points.
    pub(crate) fn queue_cursor_move(&mut self, x: f32, y: f32) {
        self.cursor_position = Point::new(x, y);
        self.pending_events
            .push(Event::Mouse(crate::iced::mouse::Event::CursorMoved {
                position: self.cursor_position,
            }));
    }

    /// Whether the UI's last frame had a focused widget wanting keyboard
    /// input. Drives the iOS soft keyboard. `false` until the first frame
    /// renders.
    #[cfg_attr(not(target_os = "ios"), allow(dead_code))]
    pub(crate) fn wants_keyboard(&self) -> bool {
        self.render.as_ref().is_some_and(|r| r.wants_keyboard)
    }
}
