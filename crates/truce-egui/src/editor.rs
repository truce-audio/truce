//! `EguiEditor`: implements `truce_core::Editor` using egui + baseview + wgpu.
//!
//! On `open()`, creates a baseview child window and a wgpu surface.
//! Each `on_frame()` tick, runs the egui frame, tessellates, and renders.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions, WindowScalePolicy};

use truce_core::editor::{
    Editor, PluginContext, PluginContextReadF64, RawWindowHandle, ResizeCorrector,
};
use truce_params::Params;

use crate::platform::ParentWindow;
#[cfg(target_os = "windows")]
use crate::render_thread::{FramePacket, RenderThread};
#[cfg(not(target_os = "windows"))]
use crate::renderer::EguiRenderer;
use truce_gui::EditorScale;

/// Trait for stateful egui UI implementations.
///
/// Implement this for complex UIs that need internal state. For simple
/// closure-based UIs, use `EguiEditor::new()` instead.
pub trait EditorUi<P: Params + ?Sized>: Send {
    fn ui(&mut self, ui: &mut egui::Ui, state: &PluginContext<P>);

    /// Called once when the editor window opens. Use to create `StateBindings`.
    fn opened(&mut self, _state: &PluginContext<P>) {}

    /// Plugin state was restored (preset recall, undo, session load).
    /// Re-read any cached custom state. Parameter values update automatically.
    fn state_changed(&mut self, _state: &PluginContext<P>) {}
}

impl<P: Params + ?Sized, F: FnMut(&mut egui::Ui, &PluginContext<P>) + Send> EditorUi<P> for F {
    fn ui(&mut self, ui: &mut egui::Ui, state: &PluginContext<P>) {
        self(ui, state);
    }
}

/// No-op placeholder for `mem::replace` during builder chain.
struct NopUi<P: ?Sized>(PhantomData<fn(&P)>);
impl<P: Params + ?Sized> EditorUi<P> for NopUi<P> {
    fn ui(&mut self, _ui: &mut egui::Ui, _state: &PluginContext<P>) {}
}

/// Type alias to keep the `WithStateChanged` field signature within
/// clippy's complexity budget without losing the `Send` bound.
type StateChangedFn<P> = Box<dyn FnMut(&PluginContext<P>) + Send>;

/// Wraps an `EditorUi` with an additional `state_changed` callback.
struct WithStateChanged<P: Params + ?Sized> {
    inner: Box<dyn EditorUi<P>>,
    on_changed: StateChangedFn<P>,
}

impl<P: Params + ?Sized> EditorUi<P> for WithStateChanged<P> {
    fn ui(&mut self, ui: &mut egui::Ui, state: &PluginContext<P>) {
        self.inner.ui(ui, state);
    }

    fn opened(&mut self, state: &PluginContext<P>) {
        self.inner.opened(state);
    }

    fn state_changed(&mut self, state: &PluginContext<P>) {
        (self.on_changed)(state);
    }
}

/// egui-based editor implementing truce's `Editor` trait.
///
/// Owns the egui context, wgpu renderer, and baseview window. On each
/// `on_frame()` tick, runs the egui frame, executes the user's UI function,
/// tessellates, and presents via egui-wgpu.
///
/// Generic in the plugin's `Params` type so the closure / struct UI can
/// `Deref` straight to typed parameter fields:
/// `state.gain.read()`, `state.bypass.value()`. Stores its own
/// `Arc<P>` from construction; rebuilds the typed `PluginContext<P>`
/// every time the host opens the window via [`PluginContext::with_params`].
// Several independent one-shot flags (resize/maximize opt-ins, scale
// mode). They're genuinely distinct booleans, not a state enum in
// disguise, so the grouping the lint wants would obscure more than it'd
// clarify.
#[allow(clippy::struct_excessive_bools)]
pub struct EguiEditor<P: Params + ?Sized> {
    params: Arc<P>,
    size: (u32, u32),
    /// Pending logical size shared with the baseview handler. Packed as
    /// `(width << 32) | height`. `set_size` writes here; the handler's
    /// `on_frame` checks for divergence from its own cached size and
    /// resizes the baseview window + wgpu surface inline. baseview's
    /// macOS `Window::resize` doesn't synthesise a `Resized` event, so
    /// the diff-on-frame pattern is the only thing that catches a
    /// host-driven resize before the next paint.
    pending_size: Arc<AtomicU64>,
    /// Shared with the baseview `WindowHandler` so it survives open/close cycles.
    ui: Arc<Mutex<Box<dyn EditorUi<P>>>>,
    visuals: Option<egui::Visuals>,
    font: Option<&'static [u8]>,
    /// Resize-capability flag exposed via `Editor::can_resize`.
    /// Defaults to `false`; egui plugins that have been designed
    /// with a flexible panel layout (and want hosts to draw
    /// resize handles) opt in with `.resizable(true)`. The
    /// default keeps every existing fixed-size plugin pinned to
    /// its built dimensions instead of silently following an
    /// autoresize-driven parent `NSView` grow.
    can_resize: bool,
    /// Whether the standalone host may maximize the window, exposed
    /// via `Editor::can_maximize`. Defaults to `false`; only consulted
    /// for resizable editors (a fixed-size editor is pinned anyway).
    /// Opt in with `.maximizable(true)` for editors that render
    /// correctly at any size.
    can_maximize: bool,
    /// Optional min/max/aspect constraints reported through the
    /// `Editor::min_size` / `max_size` / `aspect_ratio` trait
    /// methods so CLAP `gui_get_resize_hints` and VST3
    /// `checkSizeConstraint` can hand the host honest bounds.
    min_size: (u32, u32),
    max_size: (u32, u32),
    aspect_ratio: Option<(u32, u32)>,
    prefers_pow2: bool,
    /// Live content-scale factor (a [`truce_gui::EditorScale`]). The
    /// editor writes here from `set_scale_factor`; the baseview
    /// handler holds a clone and applies surface/renderer
    /// reconfiguration on the next frame when the value diverges
    /// from its last-applied snapshot.
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
    /// Active baseview window handle - exists only while editor is open.
    window: Option<baseview::WindowHandle>,
    /// Typed editor context stored at `open()` for `state_changed` forwarding.
    context: Option<PluginContext<P>>,
}

// SAFETY: `baseview::WindowHandle` holds a raw native window pointer
// (HWND / NSView / X11 Window) and is not auto-`Send`. Hosts call
// `Editor::open` / `idle` / `close` from a single dedicated GUI thread
// - never concurrently and never from the audio thread - so the
// handle is only ever touched on the thread that created it. The
// `Editor` trait requires `Send` so the editor can live behind a
// trait object; this impl asserts that the type doesn't escape its
// thread in practice.
unsafe impl<P: Params + ?Sized> Send for EguiEditor<P> {}

impl<P: Params + 'static> EguiEditor<P> {
    /// Create an egui editor with a closure-based UI.
    ///
    /// `size` is the initial window size in pixels (physical).
    pub fn new(
        params: Arc<P>,
        size: (u32, u32),
        ui_fn: impl FnMut(&mut egui::Ui, &PluginContext<P>) + Send + 'static,
    ) -> Self {
        Self {
            params,
            size,
            pending_size: Arc::new(AtomicU64::new(pack_size(size))),
            ui: Arc::new(Mutex::new(Box::new(ui_fn))),
            visuals: None,
            font: None,
            scale: EditorScale::new(truce_gui::backing_scale()),
            use_system_scale: false,
            host_scale_set: false,
            window: None,
            context: None,
            can_resize: false,
            can_maximize: false,
            min_size: (1, 1),
            max_size: (u32::MAX, u32::MAX),
            aspect_ratio: None,
            prefers_pow2: false,
        }
    }

    /// Create an egui editor with a trait-based UI (for stateful UIs).
    pub fn with_ui(params: Arc<P>, size: (u32, u32), ui: impl EditorUi<P> + 'static) -> Self {
        Self {
            params,
            size,
            pending_size: Arc::new(AtomicU64::new(pack_size(size))),
            ui: Arc::new(Mutex::new(Box::new(ui))),
            visuals: None,
            font: None,
            scale: EditorScale::new(truce_gui::backing_scale()),
            use_system_scale: false,
            host_scale_set: false,
            window: None,
            context: None,
            can_resize: false,
            can_maximize: false,
            min_size: (1, 1),
            max_size: (u32::MAX, u32::MAX),
            aspect_ratio: None,
            prefers_pow2: false,
        }
    }

    /// Add a callback for when plugin state is restored (preset recall, undo).
    ///
    /// Only needed with the closure API (`EguiEditor::new`). For the struct
    /// API (`EguiEditor::with_ui`), implement `EditorUi::state_changed` instead.
    ///
    /// ```ignore
    /// EguiEditor::new(params, (400, 300), |ui, state| { /* ui */ })
    ///     .on_state_changed(|state| { /* re-read cached state */ })
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if called after `open()` - by then the `Arc<Mutex<_>>`
    /// holding the UI has been cloned for the running editor and
    /// can't be unwrapped. Configure callbacks during construction.
    #[must_use]
    pub fn on_state_changed(mut self, f: impl FnMut(&PluginContext<P>) + Send + 'static) -> Self {
        let old = std::mem::replace(
            &mut self.ui,
            Arc::new(Mutex::new(
                Box::new(NopUi::<P>(PhantomData)) as Box<dyn EditorUi<P>>
            )),
        );
        let inner = Arc::try_unwrap(old)
            .ok()
            .and_then(|m| m.into_inner().ok())
            .expect("on_state_changed must be called during construction, not after open()");
        self.ui = Arc::new(Mutex::new(Box::new(WithStateChanged::<P> {
            inner,
            on_changed: Box::new(f),
        })));
        self
    }

    /// Set custom visuals (theme). Use `truce_egui::theme::dark()` for
    /// the default dark theme matching truce-gui.
    #[must_use]
    pub fn with_visuals(mut self, visuals: egui::Visuals) -> Self {
        self.visuals = Some(visuals);
        self
    }

    /// Opt out of host-driven resizing. egui editors default to
    /// resizable because the panel layout reflows for free; pass
    /// `false` here for plugins that ship a deliberately fixed-size
    /// GUI and don't want hosts painting resize handles.
    #[must_use]
    pub fn resizable(mut self, resizable: bool) -> Self {
        self.can_resize = resizable;
        self
    }

    /// Opt into the standalone host's maximize button. Defaults to
    /// `false` (maximize is removed for resizable editors so the window
    /// can't grow past the editor's bounds into an empty margin); pass
    /// `true` for editors that render correctly at any size. Only the
    /// standalone host consults this (plugin formats let the DAW own
    /// the window frame), and only when `resizable(true)`.
    #[must_use]
    pub fn maximizable(mut self, maximizable: bool) -> Self {
        self.can_maximize = maximizable;
        self
    }

    /// Minimum logical-point dimensions the editor accepts. Surfaced
    /// to CLAP `gui_get_resize_hints` and VST3 `checkSizeConstraint`.
    #[must_use]
    pub fn min_size(mut self, min: (u32, u32)) -> Self {
        self.min_size = min;
        self
    }

    /// Maximum logical-point dimensions the editor accepts. Same
    /// wrapper consumers as `min_size`.
    #[must_use]
    pub fn max_size(mut self, max: (u32, u32)) -> Self {
        self.max_size = max;
        self
    }

    /// Lock the aspect ratio as `(numerator, denominator)`. Pass
    /// `(4, 3)` for a 4:3 lock; pass `None` (the default) for free
    /// resizing.
    #[must_use]
    pub fn aspect_ratio(mut self, ratio: Option<(u32, u32)>) -> Self {
        self.aspect_ratio = ratio;
        self
    }

    /// Hint that the renderer prefers power-of-two surface sizes.
    /// Only the CLAP wrapper threads this through today; other
    /// formats ignore.
    #[must_use]
    pub fn prefers_pow2(mut self, prefers: bool) -> Self {
        self.prefers_pow2 = prefers;
        self
    }

    /// Set a custom default font (TrueType data).
    ///
    /// ```ignore
    /// EguiEditor::new(params, (400, 300), my_ui)
    ///     .with_font(truce_gui::font::JETBRAINS_MONO)
    /// ```
    #[must_use]
    pub fn with_font(mut self, font_data: &'static [u8]) -> Self {
        self.font = Some(font_data);
        self
    }
}

#[inline]
fn pack_size(size: (u32, u32)) -> u64 {
    (u64::from(size.0) << 32) | u64::from(size.1)
}

// Bit-extraction: each `as u32` deliberately truncates the packed
// `u64` to the low 32 bits.
#[allow(clippy::cast_possible_truncation)]
#[inline]
fn unpack_size(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

// Baseview WindowHandler - owns the egui frame loop + wgpu renderer

/// Whether paints and swapchain reconfigures must be deferred around
/// size churn (and paced to the compositor) to protect the thread
/// running `on_frame`. True on the platforms that render inline on
/// the host's GUI thread; on Windows the render thread owns every
/// blocking wgpu call, so the GUI thread can apply resizes and paint
/// straight through a drag - deferring there only delays the visible
/// reflow.
const GUI_THREAD_BLOCKS_ON_GPU: bool = cfg!(not(target_os = "windows"));

/// Ceiling on how long a pending resize may be debounced during a
/// continuous drag (see `EguiWindowHandler::resize_seen`). ~4
/// reconfigures per second is what the slowest measured driver path
/// absorbs without backing up the GUI thread.
const RESIZE_DEBOUNCE_CAP: std::time::Duration = std::time::Duration::from_millis(250);

/// How long the window size must hold still before `on_frame` paints
/// again. While a child window is being live-resized, DWM holds its
/// presented frames and wgpu's swapchain acquire
/// (`Surface::get_current_texture`) blocks the host's GUI thread for
/// its full internal timeout - ~1 s per paint measured in REAPER on
/// Windows/AMD. Widgets that request continuous repaints (meters)
/// would otherwise stack those stalls into a multi-second host freeze.
///
/// The window must exceed the quiet gap a blocked paint itself creates:
/// while one paint sits in the 1 s acquire, the host finishes its
/// resize cascade, so a short settle re-opens the gate the moment the
/// stall ends and the freeze becomes self-sustaining (measured at a
/// steady 4 paint-stalls/s with 50 ms). 300 ms sits past that gap
/// while keeping the post-resize repaint prompt.
const RESIZE_SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

struct EguiWindowHandler<P: Params + ?Sized> {
    ui: Arc<Mutex<Box<dyn EditorUi<P>>>>,
    context: PluginContext<P>,
    egui_ctx: egui::Context,
    /// wgpu renderer, owned inline on the GUI thread. macOS / Linux
    /// only - their drivers don't exhibit the unbounded blocking that
    /// motivated the Windows render thread, and macOS ties Metal layer
    /// updates to the main thread anyway.
    #[cfg(not(target_os = "windows"))]
    renderer: Option<EguiRenderer>,
    /// Windows: the renderer lives on a dedicated render thread, which
    /// owns every blocking wgpu call (init, configure, acquire,
    /// present) so a stalled graphics driver can't freeze the host's
    /// GUI thread. `on_frame` ships egui output to it and never
    /// blocks. `None` means the thread failed to spawn or the window
    /// opened zero-sized; the editor stays blank.
    #[cfg(target_os = "windows")]
    render_thread: Option<RenderThread>,
    pending_events: Vec<egui::Event>,
    modifiers: egui::Modifiers,
    start_time: std::time::Instant,
    size: (u32, u32),
    /// Shared with the parent `EguiEditor::set_size`. Re-checked at the
    /// top of `on_frame`; if the packed value diverges from `self.size`,
    /// the handler resizes the baseview window and wgpu surface inline.
    pending_size: Arc<AtomicU64>,
    /// Shared with the parent `EguiEditor`; the editor's
    /// `set_scale_factor` and the baseview `Resized` handler both write
    /// here. `run_frame` compares against `last_applied_scale` to
    /// detect host-driven scale changes that didn't come through a
    /// `Resized` event (Reaper on Windows is the typical case).
    scale: EditorScale,
    last_applied_scale: f32,
    last_cursor_pos: egui::Pos2,
    /// Raised by the renderer's device-lost callback (or a swallowed render
    /// panic). Polled in `on_frame`, which rebuilds the renderer + recreates
    /// the `egui::Context` so the font atlas re-uploads to the fresh device.
    device_lost: Arc<AtomicBool>,
    /// Kept to rebuild on device loss: the custom font and visuals applied to
    /// a freshly recreated `egui::Context`.
    font: Option<&'static [u8]>,
    visuals: egui::Visuals,
    /// Cached param IDs + the last-seen normalized values, polled each
    /// frame to detect host automation / preset recall. The UI closure
    /// reads params straight from `context` with no change signal of its
    /// own, so without this poll the idle gate below would freeze
    /// automation when the user isn't interacting.
    param_ids: Vec<u32>,
    param_snapshot: Vec<f64>,
    /// Force a paint on the next frame regardless of the idle gate. Set
    /// on the first frame, after a resize, and after a device-loss
    /// rebuild - cases where the surface must be repainted even though
    /// no input arrived and egui didn't request a frame.
    force_paint: bool,
    /// When egui next wants to paint, derived from the previous frame's
    /// `repaint_delay`. `None` means egui reported itself idle (no
    /// animation, caret blink, or pending `request_repaint`).
    next_paint_at: Option<std::time::Instant>,
    /// Constraint copy from the parent `EguiEditor`, applied to
    /// host-driven `Resized` events that bypassed the format's
    /// negotiation hooks (Linux hosts resizing the embed window
    /// directly), plus the corrective push-back guard.
    min_size: (u32, u32),
    max_size: (u32, u32),
    aspect_ratio: Option<(u32, u32)>,
    resize_corrector: ResizeCorrector,
    /// Corrective size to push back to the host, queued by the
    /// `Resized` handler and issued from `on_frame` - never from
    /// inside the host's own resize dispatch.
    #[cfg(not(target_os = "linux"))]
    pending_correct: Option<(u32, u32)>,
    /// `pending_size` value observed on the previous `on_frame` tick.
    /// A pending resize is applied only once it has survived one full
    /// tick unchanged (or [`Self::resize_burst_start`] passes the
    /// debounce cap). Reconfiguring the wgpu swapchain calls DXGI
    /// `ResizeBuffers`, which blocks the host's GUI thread inside the
    /// driver until the GPU queue drains - doing that on every tick of
    /// a live drag stacked those waits into multi-second host freezes
    /// (REAPER on Windows/AMD measured ~235 ms per reconfigure).
    resize_seen: (u32, u32),
    /// When the current burst of pending-size changes began. Bounds the
    /// debounce: a drag that never pauses a full tick still reconfigures
    /// every [`RESIZE_DEBOUNCE_CAP`] so the surface tracks the window.
    resize_burst_start: Option<std::time::Instant>,
    /// When the window size last changed (new pending observed or a
    /// resize applied). Painting is suppressed until this is at least
    /// [`RESIZE_SETTLE`] ago - see that constant for why.
    last_size_change: Option<std::time::Instant>,
    /// Paces paints to the compositor's measured consumption rate so
    /// a repaint-heavy editor (meters) can't park the host's GUI
    /// thread in the swapchain acquire - see
    /// [`truce_gui::PaintPacer`].
    pacer: truce_gui::PaintPacer,
    /// The window's *actual* physical size from the last host-driven
    /// `Resized` (`(0,0)` = none). The wgpu surface is configured to cover
    /// this exact extent - which may exceed `to_physical_px(fitted)` when a
    /// Linux host (Bitwig) forces an oversized window - so the editor
    /// content renders at its fitted size *centered* in the surface with a
    /// solid margin, instead of stretching or leaving bare window
    /// background. Paired with [`Self::last_resize_fitted`] to tell a
    /// host-driven resize (adopt this size) from a programmatic / macOS one
    /// (size the surface from the logical value we're resizing to).
    last_resize_phys: (u32, u32),
    /// The fitted (bounds- + aspect-clamped) logical size the last
    /// `Resized` produced, i.e. what it wrote to `pending_size`. When
    /// `on_frame`'s pending equals this, the resize is host-driven and
    /// `last_resize_phys` is its authoritative window extent; otherwise the
    /// pending came from `set_size` and `last_resize_phys` is stale.
    last_resize_fitted: (u32, u32),
}

impl<P: Params + ?Sized> EguiWindowHandler<P> {
    /// Rebuild the wgpu renderer and recreate the `egui::Context` after a
    /// device loss. The new renderer starts with an empty texture map, so the
    /// context must be recreated to re-emit the font atlas on the next frame.
    /// UI memory (widget state) is lost - acceptable after a GPU reset.
    fn recover_device(&mut self, window: &mut Window) {
        let device_lost = Arc::new(AtomicBool::new(false));
        let phys_w = truce_gui::to_physical_px(self.size.0, f64::from(self.last_applied_scale));
        let phys_h = truce_gui::to_physical_px(self.size.1, f64::from(self.last_applied_scale));
        #[cfg(not(target_os = "windows"))]
        {
            self.renderer = None;
            self.renderer =
                unsafe { EguiRenderer::from_window(window, phys_w, phys_h, device_lost.clone()) };
        }
        #[cfg(target_os = "windows")]
        {
            // Dropping the handle shuts the old thread down (bounded
            // join, else detach); the fresh thread re-runs GPU init
            // against the same child HWND.
            self.render_thread = None;
            self.render_thread = crate::render_thread::hwnd_for(window)
                .and_then(|hwnd| RenderThread::spawn(hwnd, phys_w, phys_h, device_lost.clone()));
        }
        let egui_ctx = egui::Context::default();
        egui_ctx.set_visuals(self.visuals.clone());
        if let Some(font_data) = self.font {
            crate::font::apply_font(&egui_ctx, font_data);
        }
        self.egui_ctx = egui_ctx;
        self.device_lost = device_lost;
    }
    /// Notice the render thread finishing its GPU init. Frames are
    /// only run/submitted once it's ready (`painter_ready`), so the
    /// first observation forces a paint of the so-far-blank window.
    /// Any resize that happened during init is already queued in the
    /// thread's mailbox and applies before that paint.
    #[cfg(target_os = "windows")]
    fn adopt_ready_renderer(&mut self) {
        if let Some(rt) = &mut self.render_thread
            && rt.take_ready()
        {
            self.force_paint = true;
            log::info!("egui gpu init completed; render thread ready");
        }
    }

    /// Whether a frame can actually be painted this tick: the inline
    /// renderer exists (macOS / Linux) or the render thread finished
    /// GPU init (Windows).
    #[cfg(not(target_os = "windows"))]
    fn painter_ready(&self) -> bool {
        self.renderer.is_some()
    }
    #[cfg(target_os = "windows")]
    fn painter_ready(&self) -> bool {
        self.render_thread
            .as_ref()
            .is_some_and(RenderThread::is_ready)
    }

    /// Reconfigure the wgpu surface to a physical pixel size. Inline
    /// (blocking) on macOS / Linux; queued latest-wins to the render
    /// thread on Windows, where DXGI `ResizeBuffers` can block in the
    /// driver until the GPU queue drains.
    #[cfg(not(target_os = "windows"))]
    fn painter_resize(&mut self, phys_w: u32, phys_h: u32) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.resize(phys_w, phys_h);
        }
    }
    #[cfg(target_os = "windows")]
    fn painter_resize(&mut self, phys_w: u32, phys_h: u32) {
        if let Some(rt) = &self.render_thread {
            rt.resize(phys_w, phys_h);
        }
    }

    /// Paint one frame of egui output: render inline (macOS / Linux)
    /// or submit to the render thread (Windows), which merges texture
    /// deltas if the previous frame was dropped unconsumed.
    // Owned args match the Windows variant, which moves them into the
    // render thread's `FramePacket`; this inline variant only borrows.
    #[allow(clippy::needless_pass_by_value)]
    #[cfg(not(target_os = "windows"))]
    fn painter_paint(
        &mut self,
        textures_delta: egui::TexturesDelta,
        primitives: Vec<egui::ClippedPrimitive>,
        pixels_per_point: f32,
    ) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.render(&textures_delta, &primitives, pixels_per_point);
        }
    }
    #[cfg(target_os = "windows")]
    fn painter_paint(
        &mut self,
        textures_delta: egui::TexturesDelta,
        primitives: Vec<egui::ClippedPrimitive>,
        pixels_per_point: f32,
    ) {
        if let Some(rt) = &self.render_thread {
            rt.submit(FramePacket {
                textures_delta,
                primitives,
                pixels_per_point,
            });
        }
    }

    /// How long the most recent swapchain acquire blocked - measured
    /// inline on macOS / Linux, reported back from the render thread
    /// on Windows (one paint delayed, which the pacer's decayed-max
    /// estimate absorbs).
    #[cfg(not(target_os = "windows"))]
    fn painter_acquire_wait(&self) -> std::time::Duration {
        self.renderer
            .as_ref()
            .map_or(std::time::Duration::ZERO, EguiRenderer::acquire_wait)
    }
    #[cfg(target_os = "windows")]
    fn painter_acquire_wait(&self) -> std::time::Duration {
        self.render_thread
            .as_ref()
            .map_or(std::time::Duration::ZERO, RenderThread::last_acquire_wait)
    }

    /// Apply a pending resize: `NSView` frame (baseview's
    /// `Window::resize`) first, then the wgpu surface. Reverse
    /// order would leave the surface oversized vs. the layer that
    /// hosts it for a frame and Metal could draw against an
    /// undersized drawable.
    fn apply_resize(
        &mut self,
        window: &mut Window,
        new_size: (u32, u32),
        scale: f64,
        surface_phys: Option<(u32, u32)>,
        resize_window: bool,
    ) {
        // On Linux, a host/WM-driven resize that already satisfies the
        // editor's bounds + aspect (`surface_phys` matched) is authoritative:
        // the X11 WM owns the interactive drag grab and enforces the same
        // constraints via size-increment hints. Counter-resizing the window
        // here fights that grab and, at fractional/×2 DPI, disagrees with the
        // WM by a pixel every frame - the resize jitter. Adopt the size (the
        // surface + `self.size` still update); only resize when the size came
        // from us (programmatic `set_size`) or the host handed us an
        // out-of-bounds box that needs correcting.
        if resize_window {
            window.resize(baseview::Size::new(
                f64::from(new_size.0),
                f64::from(new_size.1),
            ));
        }
        // Prefer the window's authoritative physical extent (host-driven
        // `Resized`); fall back to `to_physical_px(logical)` for
        // programmatic resizes and platforms that don't report it.
        let (phys_w, phys_h) = surface_phys.unwrap_or_else(|| {
            (
                truce_gui::to_physical_px(new_size.0, scale),
                truce_gui::to_physical_px(new_size.1, scale),
            )
        });
        self.painter_resize(phys_w, phys_h);
    }

    // `(u32, u32)` editor sizes widen to `f32` for egui's screen rect.
    // Editor sizes are bounded by display dimensions, well below 2^23.
    /// Run + paint one egui frame. Returns egui's requested
    /// `repaint_delay` for the root viewport (`None` if no renderer),
    /// which the caller folds into the idle gate: `ZERO` means animate
    /// next frame, a finite delay schedules one, and `Duration::MAX`
    /// means idle.
    #[allow(clippy::cast_precision_loss)]
    fn run_frame(&mut self) -> Option<std::time::Duration> {
        if !self.painter_ready() {
            return None;
        }

        // Pick up host-driven scale changes (CLAP `set_scale`, VST3
        // `IPlugViewContentScaleSupport`) that arrived via the editor's
        // `set_scale_factor` since the last frame. The `Resized` path
        // already applies its own scale changes inline, so this only
        // fires when scale moved without a corresponding window event.
        if let Some(cur_scale) = self.scale.take_change(&mut self.last_applied_scale) {
            let phys_w = truce_gui::to_physical_px(self.size.0, f64::from(cur_scale));
            let phys_h = truce_gui::to_physical_px(self.size.1, f64::from(cur_scale));
            self.painter_resize(phys_w, phys_h);
        }

        let ppp = self.last_applied_scale;

        // Lay out egui at the fitted (bounds- + aspect-clamped) editor size,
        // anchored top-left. Within `[min, max]` the fitted size equals the
        // window, so egui fills it (reflow); beyond max, or off the aspect
        // ratio, egui stays at the fitted size and the extra window area on
        // the right / bottom is the render pass's black clear (letterbox).
        // The surface always covers the whole window (see `apply_resize`).
        #[allow(clippy::cast_precision_loss)]
        let (lw, lh) = {
            let (fw, fh) = self.size;
            (fw as f32, fh as f32)
        };

        let mut raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(lw, lh),
            )),
            time: Some(self.start_time.elapsed().as_secs_f64()),
            modifiers: self.modifiers,
            events: std::mem::take(&mut self.pending_events),
            focused: true,
            ..Default::default()
        };
        raw_input
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(ppp);

        let ui_arc = &self.ui;
        let context = &self.context;
        let output = self.egui_ctx.run_ui(raw_input, |ui| {
            // Recover a poisoned `ui` mutex (into_inner) rather than skip the
            // frame forever: a panic in author `ui` code is caught by the
            // baseview `on_frame` firewall, and the editor must keep rendering
            // afterward - matching the built-in editor's `RefCell` recovery.
            ui_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ui(ui, context);
        });

        let repaint_delay = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay);

        let clipped_primitives = self
            .egui_ctx
            .tessellate(output.shapes, output.pixels_per_point);

        self.painter_paint(
            output.textures_delta,
            clipped_primitives,
            output.pixels_per_point,
        );

        repaint_delay
    }

    /// Poll params into the snapshot, returning whether any moved since
    /// the last poll. Cheap (atomic reads); drives automation detection
    /// for the idle gate.
    fn params_changed(&mut self) -> bool {
        let mut changed = false;
        for (slot, &id) in self.param_snapshot.iter_mut().zip(&self.param_ids) {
            let cur = self.context.get_param(id);
            if (cur - *slot).abs() > 1e-10 {
                *slot = cur;
                changed = true;
            }
        }
        changed
    }

    /// Whether this frame must be painted. Skipping otherwise keeps the
    /// host's GUI thread (which drives `on_frame`) free between real
    /// updates. Errs toward painting: any uncertainty repaints.
    fn should_paint(&mut self) -> bool {
        // `params_changed` must run every call to keep the snapshot
        // current, so evaluate it before the short-circuiting `||`.
        let params_moved = self.params_changed();
        // Bit-compare the live scale against the applied one without
        // mutating `last_applied_scale` (run_frame's `take_change` owns
        // that); a host scale change must repaint.
        #[allow(clippy::float_cmp)]
        let scale_moved = self.scale.get_f32() != self.last_applied_scale;
        self.force_paint
            || !self.pending_events.is_empty()
            || params_moved
            || scale_moved
            || self.egui_ctx.has_requested_repaint()
            || self
                .next_paint_at
                .is_some_and(|t| std::time::Instant::now() >= t)
    }
}

impl<P: Params + ?Sized + 'static> WindowHandler for EguiWindowHandler<P> {
    fn on_frame(&mut self, window: &mut Window) {
        // Catch panics at the FFI boundary: baseview drives this from an
        // `extern "system"` window proc (Windows) / AppKit callback
        // (macOS), so an unwinding panic - e.g. wgpu validation tripping
        // mid-resize - would cross a C frame and abort the host. Swallow
        // and log instead, mirroring the builtin GPU editor's handler.
        let frame_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Notice the render thread finishing GPU init (the editor
            // opens without waiting on it; blank until ready).
            #[cfg(target_os = "windows")]
            self.adopt_ready_renderer();
            // Issue a queued corrective resize (see `pending_correct`)
            // now that we're outside the host's resize dispatch.
            #[cfg(not(target_os = "linux"))]
            if let Some((rw, rh)) = self.pending_correct.take() {
                let _ = self.context.request_resize(rw, rh);
            }
            // Rebuild the renderer if the device was lost (flagged by the
            // device-lost callback or a swallowed render panic). Skip the rest
            // of this frame; the next tick renders against the fresh device.
            if self.device_lost.load(Ordering::Acquire) {
                self.recover_device(window);
                log::warn!("egui device-loss recovery: rebuilt");
                // The rebuilt surface starts blank; paint next frame
                // even if nothing else changed.
                self.force_paint = true;
                return;
            }
            // Skip the whole frame while the editor isn't presentable:
            // detached / occluded on macOS, host child window hidden /
            // minimized on Windows (no-op on Linux). On Windows this
            // body runs on the host's GUI thread, so skipping an
            // unpresentable frame keeps a blocking present from freezing
            // the host while its FX window is closed.
            {
                use raw_window_handle::HasRawWindowHandle;
                if truce_gui::platform::should_skip_frame(window.raw_window_handle()) {
                    return;
                }
            }
            // Re-anchor each frame so the child NSView's origin tracks
            // size changes against the host's plug-in pane - without it
            // the canvas drifts off-anchor as it grows, clipping the
            // layout's top off the visible area in CLAP hosts (REAPER).
            #[cfg(target_os = "macos")]
            {
                use raw_window_handle::HasRawWindowHandle;
                truce_gui::platform::reanchor_to_superview_top(window.raw_window_handle());
            }
            // Pick up host-driven `set_size` requests since the last frame.
            // baseview's macOS `Window::resize` doesn't synthesise a
            // `Resized` event, so the wgpu surface has to be reconfigured
            // here even though the OS-level resize happens via
            // `window.resize`. Linux/Win32 backends *do* fire `Resized`,
            // but reapplying the surface config is idempotent.
            //
            // Skip the draw on a resize frame so AppKit's deferred
            // relayout (scheduled by `view.setNeedsDisplay` inside
            // `Window::resize`) can settle before we paint. Without the
            // skip, the egui draw races against AppKit's layout pass and
            // can land an NSException through Reaper's main-thread
            // callback (Metal layer mid-resize). The next `on_frame` tick
            // picks up the freshly-sized surface.
            let pending = unpack_size(self.pending_size.load(Ordering::Relaxed));
            if pending != self.size && pending.0 > 0 && pending.1 > 0 {
                // Debounce a live drag: only pay the swapchain
                // reconfigure (a blocking driver wait, see
                // `resize_seen`) once the size has held still for a
                // tick, or every `RESIZE_DEBOUNCE_CAP` during a burst.
                // Until then skip the tick - content briefly freezes
                // at the old extent mid-drag, which beats stacking
                // driver waits on the host's GUI thread.
                let stable = pending == self.resize_seen;
                if !stable {
                    self.last_size_change = Some(std::time::Instant::now());
                }
                self.resize_seen = pending;
                let deadline_passed = self
                    .resize_burst_start
                    .is_some_and(|t| t.elapsed() >= RESIZE_DEBOUNCE_CAP);
                if self.resize_burst_start.is_none() {
                    self.resize_burst_start = Some(std::time::Instant::now());
                }
                if GUI_THREAD_BLOCKS_ON_GPU && !stable && !deadline_passed {
                    return;
                }
                self.resize_burst_start = None;
                // Skip the draw on a resize frame so AppKit's deferred
                // relayout (scheduled by `view.setNeedsDisplay` inside
                // `Window::resize`) settles before we paint, and
                // bracket the macOS-side work in an autoreleasepool so
                // AppKit autoreleased objects from `setFrameSize` drain
                // before the next call rather than accumulating into
                // the host's main-thread pool.
                let new_size = pending;
                let scale = self.scale.get();
                // A host/WM-driven resize is one whose fitted result equals
                // what the last `Resized` produced - then `last_resize_phys`
                // is the window's authoritative physical extent and the
                // surface must cover it (possibly larger than the content, so
                // the content centers with a margin). A programmatic
                // `set_size` writes a `pending` that doesn't match, so the
                // surface is sized from the logical value we're resizing to.
                let host_driven =
                    self.last_resize_phys != (0, 0) && new_size == self.last_resize_fitted;
                let surface_phys = host_driven.then_some(self.last_resize_phys);
                // On Linux, adopt a host/WM-driven size rather than
                // counter-resizing: the WM owns the interactive drag grab and
                // enforces bounds/aspect via size-increment hints, so
                // `window.resize` here fights that grab and jitters by a pixel
                // at ×2 / fractional DPI. macOS / Windows and programmatic
                // resizes still resize.
                let resize_window = cfg!(not(target_os = "linux")) || !host_driven;
                #[cfg(target_os = "macos")]
                objc::rc::autoreleasepool(|| {
                    self.apply_resize(window, new_size, scale, surface_phys, resize_window);
                });
                #[cfg(not(target_os = "macos"))]
                self.apply_resize(window, new_size, scale, surface_phys, resize_window);
                self.size = new_size;
                self.last_size_change = Some(std::time::Instant::now());
                // The freshly-sized surface must be painted next frame
                // regardless of the idle gate.
                self.force_paint = true;
                return;
            }
            // No divergent pending size: the host either never resized
            // or bounced back to the current size mid-burst - clear the
            // debounce marker so the next burst starts a fresh window.
            self.resize_burst_start = None;
            // Hold off painting until the size has been quiet for
            // `RESIZE_SETTLE` - a paint mid-resize blocks the host GUI
            // thread in the swapchain acquire (see the constant).
            // `force_paint` stays armed, so the settled size paints on
            // the first tick past the window. Windows paints straight
            // through (the render thread absorbs any stall).
            if GUI_THREAD_BLOCKS_ON_GPU
                && self
                    .last_size_change
                    .is_some_and(|t| t.elapsed() < RESIZE_SETTLE)
            {
                return;
            }
            // Compositor pacing veto - see `pacer`. Checked outside
            // `should_paint` because a repaint-requesting widget
            // (meter) re-arms `has_requested_repaint` every frame and
            // would bypass any schedule-based gate. Windows skips the
            // veto: the render thread's latest-wins mailbox drops
            // frames the compositor can't take, so pacing there only
            // adds latency (a resize-time acquire stall inflates the
            // pace estimate for seconds).
            if GUI_THREAD_BLOCKS_ON_GPU && self.pacer.should_hold() {
                return;
            }
            // Idle gate: skip the whole frame (no egui run, no present)
            // when nothing needs redrawing. This is what keeps the host
            // responsive - an idle editor stops doing per-tick work on
            // the host's GUI thread.
            if !self.should_paint() {
                return;
            }
            let repaint_delay = self.run_frame();
            self.force_paint = false;
            self.pacer.record_acquire(self.painter_acquire_wait());
            // Schedule the next forced paint from egui's reported delay:
            // `ZERO` -> paint next frame (animating), a finite delay ->
            // paint then, and anything very large (egui's idle
            // `Duration::MAX`) -> idle until input / automation. Cap
            // guards `Instant + Duration` against overflow.
            self.next_paint_at = match repaint_delay {
                Some(d) if d <= std::time::Duration::from_hours(1) => {
                    Some(std::time::Instant::now() + d)
                }
                _ => None,
            };
        }));
        if let Err(e) = frame_result {
            let msg = e
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            log::error!("egui on_frame panic swallowed: {msg}");
            // A render panic almost always means the device is dead (e.g.
            // egui-wgpu's staging-buffer alloc failing after a loss that
            // didn't fire the callback). Arm recovery for the next frame.
            self.device_lost.store(true, Ordering::Release);
        }
    }

    // `_window` is unused on macOS / Linux - only the Windows
    // ButtonPressed branch reads it (to SetFocus on the child HWND so
    // text widgets see WM_KEYDOWN). Underscore-prefix keeps that signal
    // intact; the allow lets the Windows branch use the binding without
    // renaming.
    #[allow(clippy::too_many_lines, clippy::used_underscore_binding)]
    fn on_event(&mut self, _window: &mut Window, event: Event) -> EventStatus {
        // Catch panics at the FFI boundary, like `on_frame` above: a panic
        // in egui input handling would otherwise unwind through baseview's
        // `extern "system"` window proc and abort the host. On panic we
        // report the event as `Ignored`.
        let event_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match event {
                Event::Mouse(mouse) => {
                    use baseview::MouseEvent::{
                        ButtonPressed, ButtonReleased, CursorEntered, CursorLeft, CursorMoved,
                        WheelScrolled,
                    };
                    // The explicit `CursorEntered => Ignored` arm signals the
                    // event was considered and intentionally ignored (vs.
                    // `CursorLeft` which we forward as `PointerGone`); the
                    // wildcard absorbs future baseview MouseEvent variants.
                    #[allow(clippy::match_same_arms)]
                    match mouse {
                        CursorMoved {
                            position,
                            modifiers,
                        } => {
                            self.modifiers = convert_kb_modifiers(modifiers);
                            // baseview reports cursor in f64 logical points;
                            // egui uses f32. Window dimensions never reach
                            // 2^23 - the narrowing is invisible.
                            #[allow(clippy::cast_possible_truncation)]
                            let pos = egui::pos2(position.x as f32, position.y as f32);
                            self.last_cursor_pos = pos;
                            self.pending_events.push(egui::Event::PointerMoved(pos));
                            EventStatus::Captured
                        }
                        ButtonPressed { button, modifiers } => {
                            // On Windows, a WS_CHILD plugin window doesn't receive
                            // WM_KEYDOWN/WM_CHAR until it has HWND focus. baseview
                            // doesn't SetFocus on mouse-down, so we do it here -
                            // otherwise text-edit widgets never see keystrokes
                            // (the DAW keeps eating them for transport etc.).
                            #[cfg(target_os = "windows")]
                            {
                                if !_window.has_focus() {
                                    _window.focus();
                                }
                            }
                            self.modifiers = convert_kb_modifiers(modifiers);
                            if let Some(btn) = convert_mouse_button(button) {
                                self.pending_events.push(egui::Event::PointerButton {
                                    pos: self.last_cursor_pos,
                                    button: btn,
                                    pressed: true,
                                    modifiers: self.modifiers,
                                });
                            }
                            EventStatus::Captured
                        }
                        ButtonReleased { button, modifiers } => {
                            self.modifiers = convert_kb_modifiers(modifiers);
                            if let Some(btn) = convert_mouse_button(button) {
                                self.pending_events.push(egui::Event::PointerButton {
                                    pos: self.last_cursor_pos,
                                    button: btn,
                                    pressed: false,
                                    modifiers: self.modifiers,
                                });
                            }
                            EventStatus::Captured
                        }
                        WheelScrolled { delta, modifiers } => {
                            self.modifiers = convert_kb_modifiers(modifiers);
                            let (dx, dy) = match delta {
                                baseview::ScrollDelta::Lines { x, y } => (x * 20.0, y * 20.0),
                                baseview::ScrollDelta::Pixels { x, y } => (x, y),
                            };
                            self.pending_events.push(egui::Event::MouseWheel {
                                unit: egui::MouseWheelUnit::Point,
                                delta: egui::vec2(dx, dy),
                                // baseview doesn't tell us touch / inertial phase;
                                // `Move` is egui's "unknown" recommendation.
                                phase: egui::TouchPhase::Move,
                                modifiers: self.modifiers,
                            });
                            EventStatus::Captured
                        }
                        CursorEntered => EventStatus::Ignored,
                        CursorLeft => {
                            self.pending_events.push(egui::Event::PointerGone);
                            EventStatus::Captured
                        }
                        _ => EventStatus::Ignored,
                    }
                }
                Event::Keyboard(kb) => {
                    use keyboard_types::KeyState;
                    self.modifiers = convert_kb_modifiers(kb.modifiers);

                    // Text input. Suppress Text events when Ctrl/Cmd is
                    // held - otherwise Ctrl+A/Ctrl+C/etc. would also insert
                    // the character into focused text fields, which egui's
                    // shortcut handler reads through `command_pressed()`.
                    let modifier_held = self.modifiers.command || self.modifiers.mac_cmd;
                    if kb.state == KeyState::Down
                        && !modifier_held
                        && let keyboard_types::Key::Character(ref ch) = kb.key
                    {
                        for c in ch.chars() {
                            if !c.is_control() {
                                self.pending_events.push(egui::Event::Text(c.to_string()));
                            }
                        }
                    }

                    // Key event
                    if let Some(key) = convert_key(&kb.key) {
                        self.pending_events.push(egui::Event::Key {
                            key,
                            physical_key: None,
                            pressed: kb.state == KeyState::Down,
                            repeat: kb.repeat,
                            modifiers: self.modifiers,
                        });
                    }

                    EventStatus::Captured
                }
                Event::Window(win) => {
                    if let baseview::WindowEvent::Resized(info) = win {
                        let pw = info.physical_size().width;
                        let ph = info.physical_size().height;
                        // Any change in the window's physical extent (re)arms
                        // the paint settle gate - see `RESIZE_SETTLE`. The
                        // pending-size tracking in `on_frame` can't see this
                        // churn for a fixed-size editor: the fitted logical
                        // size stays at the natural size while the host drags
                        // the window through arbitrary extents.
                        if (pw, ph) != self.last_resize_phys {
                            self.last_size_change = Some(std::time::Instant::now());
                        }
                        // Authoritative window extent for the wgpu surface -
                        // see `last_resize_phys`. `on_frame` reads it when it
                        // matches the pending logical size.
                        self.last_resize_phys = (pw, ph);
                        // Display scale never exceeds 4.0 in practice.
                        #[allow(clippy::cast_possible_truncation)]
                        let scale = info.scale() as f32;
                        truce_gui::platform::note_linux_scale_factor(info.scale());
                        // Store logical size - egui screen_rect uses logical
                        // points. Round so a physical 800px@2× reports as 400
                        // logical, not 399 (truncating cast). Window
                        // dimensions stay well below u32::MAX.
                        #[allow(
                            clippy::cast_possible_truncation,
                            clippy::cast_sign_loss,
                            clippy::cast_precision_loss
                        )]
                        let logical_in = (
                            (pw as f32 / scale).round() as u32,
                            (ph as f32 / scale).round() as u32,
                        );
                        // A host that resized the embed window directly
                        // never ran the format's constraint preflight - fit
                        // here and push the corrected size back.
                        let (logical_size, correct) = self.resize_corrector.fit(
                            logical_in.0,
                            logical_in.1,
                            self.min_size,
                            self.max_size,
                            self.aspect_ratio,
                        );
                        if let Some((rw, rh)) = correct {
                            // On Linux, hosts that bypass size negotiation
                            // (Bitwig) ignore this request and react by
                            // *growing* the embed window - a resize loop,
                            // worsened by our own corrective `window.resize`
                            // feeding a fresh in-bounds `Resized` that re-arms
                            // the guard. Clamp the content (and counter-resize
                            // our child) but never ask the host to resize its
                            // frame. mac/windows honor the request and
                            // negotiate via `checkSizeConstraint` anyway.
                            // Deferred to `on_frame`: issued inline, this
                            // re-enters the host's own resize dispatch
                            // (VST3 forbids `resizeView` inside `onSize`;
                            // Ableton hangs on it, e.g. title-bar
                            // double-click snapping the window back).
                            // Linux AND Windows: never ask the host to
                            // resize its frame - clamp the content and
                            // letterbox instead. Bitwig/X11 answers the
                            // request by growing the window (a loop), and
                            // REAPER on Windows re-asserts a maximized FX
                            // window every tick (double-click maximize),
                            // fighting the correction forever while each
                            // round trips a driver wait. Windows hosts
                            // shape interactive drags via
                            // `checkSizeConstraint` anyway. macOS keeps
                            // the push-back (hosts honor it, no fights).
                            #[cfg(target_os = "macos")]
                            {
                                self.pending_correct = Some((rw, rh));
                            }
                            #[cfg(not(target_os = "macos"))]
                            let _ = (rw, rh);
                        }
                        // Write through to the shared scale so `on_frame` /
                        // `run_frame` convert with the OS-reported DPI.
                        self.scale.set(info.scale());
                        // Defer the surface reconfigure to `on_frame` (via the
                        // same `pending_size` cell `set_size` uses) instead of
                        // calling `renderer.resize()` inline here. A fast
                        // host-driven drag fires `Resized` far quicker than
                        // vblank - on Windows the LV2 parent-HWND subclass turns
                        // REAPER's `WM_SIZE` storm into exactly that - and
                        // reconfiguring the wgpu swapchain on every event backs
                        // up the present queue until the GPU's timeout-detection
                        // (TDR) fires and hangs the host. `on_frame` coalesces
                        // the pending size to one reconfigure per frame.
                        self.last_resize_fitted = logical_size;
                        self.pending_size
                            .store(pack_size(logical_size), Ordering::Relaxed);
                    }
                    EventStatus::Ignored
                }
            }
        }));
        match event_result {
            Ok(status) => status,
            Err(e) => {
                let msg = e
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| e.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                log::error!("egui on_event panic swallowed: {msg}");
                EventStatus::Ignored
            }
        }
    }
}

// Event conversion helpers

fn convert_mouse_button(btn: baseview::MouseButton) -> Option<egui::PointerButton> {
    match btn {
        baseview::MouseButton::Left => Some(egui::PointerButton::Primary),
        baseview::MouseButton::Right => Some(egui::PointerButton::Secondary),
        baseview::MouseButton::Middle => Some(egui::PointerButton::Middle),
        // Side-mouse "back" / "forward" thumb buttons. baseview reports
        // them on every platform that distinguishes the buttons (X11
        // XInput2, Win32 WM_XBUTTON*, NSEvent buttonNumber 3/4); egui
        // surfaces them as `Extra1` / `Extra2`. Plugin authors that opt
        // in (e.g. for back/forward navigation in a custom editor) get
        // the events; ones that don't simply ignore the variant.
        baseview::MouseButton::Back => Some(egui::PointerButton::Extra1),
        baseview::MouseButton::Forward => Some(egui::PointerButton::Extra2),
        baseview::MouseButton::Other(_) => None,
    }
}

fn convert_kb_modifiers(mods: keyboard_types::Modifiers) -> egui::Modifiers {
    let alt = mods.contains(keyboard_types::Modifiers::ALT);
    let ctrl = mods.contains(keyboard_types::Modifiers::CONTROL);
    let shift = mods.contains(keyboard_types::Modifiers::SHIFT);
    let meta = mods.contains(keyboard_types::Modifiers::META);
    // `mac_cmd` - Mac-specific Cmd-key flag, fed by META on macOS only.
    // `command` - egui's cross-platform "primary modifier" alias:
    //   on macOS it tracks Cmd (= `mac_cmd`); elsewhere it tracks
    //   Ctrl. Mapping META→command on Linux/Windows (the original
    //   behavior) made egui treat Super as the shortcut modifier,
    //   breaking Ctrl+C/V/X/Z in plugin editors.
    //
    // Derive `command` from `mac_cmd` on macOS so the structural
    // redundancy (both fields end up with the same boolean on
    // macOS) flows from one source instead of computing each
    // independently from `meta`.
    let mac_cmd = cfg!(target_os = "macos") && meta;
    let command = if cfg!(target_os = "macos") {
        mac_cmd
    } else {
        ctrl
    };
    egui::Modifiers {
        alt,
        ctrl,
        shift,
        mac_cmd,
        command,
    }
}

fn convert_key(key: &keyboard_types::Key) -> Option<egui::Key> {
    use keyboard_types::Key::{
        ArrowDown, ArrowLeft, ArrowRight, ArrowUp, Backspace, Character, Delete, End, Enter,
        Escape, Home, PageDown, PageUp, Tab,
    };
    Some(match key {
        Character(s) => match s.as_str() {
            "a" | "A" => egui::Key::A,
            "b" | "B" => egui::Key::B,
            "c" | "C" => egui::Key::C,
            "d" | "D" => egui::Key::D,
            "e" | "E" => egui::Key::E,
            "f" | "F" => egui::Key::F,
            "g" | "G" => egui::Key::G,
            "h" | "H" => egui::Key::H,
            "i" | "I" => egui::Key::I,
            "j" | "J" => egui::Key::J,
            "k" | "K" => egui::Key::K,
            "l" | "L" => egui::Key::L,
            "m" | "M" => egui::Key::M,
            "n" | "N" => egui::Key::N,
            "o" | "O" => egui::Key::O,
            "p" | "P" => egui::Key::P,
            "q" | "Q" => egui::Key::Q,
            "r" | "R" => egui::Key::R,
            "s" | "S" => egui::Key::S,
            "t" | "T" => egui::Key::T,
            "u" | "U" => egui::Key::U,
            "v" | "V" => egui::Key::V,
            "w" | "W" => egui::Key::W,
            "x" | "X" => egui::Key::X,
            "y" | "Y" => egui::Key::Y,
            "z" | "Z" => egui::Key::Z,
            "0" => egui::Key::Num0,
            "1" => egui::Key::Num1,
            "2" => egui::Key::Num2,
            "3" => egui::Key::Num3,
            "4" => egui::Key::Num4,
            "5" => egui::Key::Num5,
            "6" => egui::Key::Num6,
            "7" => egui::Key::Num7,
            "8" => egui::Key::Num8,
            "9" => egui::Key::Num9,
            _ => return None,
        },
        Enter => egui::Key::Enter,
        Tab => egui::Key::Tab,
        Backspace => egui::Key::Backspace,
        Escape => egui::Key::Escape,
        Delete => egui::Key::Delete,
        ArrowLeft => egui::Key::ArrowLeft,
        ArrowRight => egui::Key::ArrowRight,
        ArrowUp => egui::Key::ArrowUp,
        ArrowDown => egui::Key::ArrowDown,
        Home => egui::Key::Home,
        End => egui::Key::End,
        PageUp => egui::Key::PageUp,
        PageDown => egui::Key::PageDown,
        _ => return None,
    })
}

// Editor trait implementation

impl<P: Params + 'static> Editor for EguiEditor<P> {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        // Re-type the dyn-erased context to `PluginContext<P>` using
        // the Arc<P> we stored at construction.
        let typed_ctx = context.with_params(self.params.clone());
        self.context = Some(typed_ctx.clone());
        let egui_ctx = egui::Context::default();
        let visuals = self.visuals.clone().unwrap_or_else(crate::theme::dark);
        egui_ctx.set_visuals(visuals.clone());
        let font = self.font;

        // Refresh the shared scale from the parent window - on macOS
        // the parent's NSWindow may live on a non-main display whose
        // `backingScaleFactor` differs from `NSScreen.mainScreen`'s.
        // On Linux the same call returns the cached baseview scale.
        // Any `set_scale_factor` the host issues *after* open will
        // override this on the next frame via the shared state.
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
            self.scale
                .set(crate::platform::query_backing_scale(&parent));
            WindowScalePolicy::SystemScaleFactor
        };
        let system_scale = self.scale.get();
        let (lw, lh) = self.size; // logical points

        // --- baseview + wgpu ---
        let ui = Arc::clone(&self.ui);
        ui.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .opened(&typed_ctx);
        let size = self.size;

        let options = WindowOpenOptions {
            title: String::from("truce-egui"),
            size: baseview::Size::new(f64::from(lw), f64::from(lh)),
            scale: scale_policy,
        };

        let parent_wrapper = ParentWindow(parent);
        let handler_ctx = typed_ctx.clone();
        // A non-resizable editor pins to its natural size: report it as
        // both the min and the max so the `ResizeCorrector` clamps any
        // host-driven resize back to it. The editor then renders at its
        // natural size (letterboxed in black if the host grew the window
        // past it) rather than reflowing/stretching to fill.
        let (min_size, max_size) = if self.can_resize {
            (self.min_size, self.max_size)
        } else {
            (self.size, self.size)
        };
        let aspect_ratio = self.aspect_ratio;
        // Seed the idle gate's param snapshot so the first automation
        // change is detected as a change rather than as "differs from
        // empty." IDs are fixed for the editor's lifetime.
        let param_ids: Vec<u32> = self.params.param_infos().iter().map(|i| i.id).collect();
        let param_snapshot: Vec<f64> = param_ids
            .iter()
            .map(|&id| typed_ctx.get_param(id))
            .collect();
        let scale_handle = self.scale.clone();
        // Clear the pending-size cell so a stale `set_size` from
        // before this `open()` doesn't immediately re-resize the
        // freshly built window. Storing 0 (not `pack_size(self.size)`)
        // because `on_frame` gates the resize branch on
        // `pending.0 > 0 && pending.1 > 0` - a 0 value is the "no
        // pending" sentinel, while storing the natural size here
        // would fight the host's autoresize: after a host-driven
        // grow, baseview's `setFrameSize:` override updates
        // `self.size` to the new parent bounds, but the cached
        // pending stays at the natural size, so the next `on_frame`
        // would call `window.resize(natural)` and shrink the child
        // back. With 0 the cell only carries genuine `set_size`
        // requests.
        self.pending_size.store(0, Ordering::Relaxed);
        let pending_size = self.pending_size.clone();
        // Shared device-loss flag: the renderer's device-lost callback raises
        // it, `on_frame` polls it and rebuilds. Cloned into the renderer; the
        // handler keeps a copy and re-arms a fresh one on each rebuild.
        let device_lost = Arc::new(AtomicBool::new(false));
        let device_lost_for_renderer = device_lost.clone();
        // `visuals` / `font` are kept on the handler so a device-loss rebuild
        // can recreate the `egui::Context` (which forces the font-atlas texture
        // to be re-uploaded to the fresh renderer).
        let handler_visuals = visuals.clone();

        let window = baseview::Window::open_parented(
            &parent_wrapper,
            options,
            move |window: &mut Window| {
                // Display scale never exceeds 4.0 in practice.
                #[allow(clippy::cast_possible_truncation)]
                let scale = system_scale as f32;
                let phys_w = truce_gui::to_physical_px(size.0, system_scale);
                let phys_h = truce_gui::to_physical_px(size.1, system_scale);
                #[cfg(not(target_os = "windows"))]
                let renderer = unsafe {
                    EguiRenderer::from_window(window, phys_w, phys_h, device_lost_for_renderer)
                };
                // Windows: GPU init and all blocking wgpu calls run on
                // the render thread; opening never waits on the driver.
                // Zero-size guard mirrors `from_window`'s (transient
                // zero-extent parents during host measurement steps).
                #[cfg(target_os = "windows")]
                let render_thread = crate::render_thread::hwnd_for(window)
                    .filter(|_| phys_w > 0 && phys_h > 0)
                    .and_then(|hwnd| {
                        RenderThread::spawn(hwnd, phys_w, phys_h, device_lost_for_renderer)
                    });

                if let Some(font_data) = font {
                    crate::font::apply_font(&egui_ctx, font_data);
                }

                // baseview's `on_frame` drives the frame loop, but it no
                // longer paints unconditionally every tick: the handler's
                // idle gate skips frames egui doesn't need. Widgets that
                // show live data (`level_meter`) call
                // `egui_ctx.request_repaint()` to keep the loop running;
                // input, automation, and animations are detected by the
                // gate directly.

                EguiWindowHandler::<P> {
                    ui,
                    context: handler_ctx,
                    egui_ctx,
                    #[cfg(not(target_os = "windows"))]
                    renderer,
                    #[cfg(target_os = "windows")]
                    render_thread,
                    pending_events: Vec::new(),
                    modifiers: egui::Modifiers::NONE,
                    start_time: std::time::Instant::now(),
                    size,
                    pending_size,
                    scale: scale_handle,
                    last_applied_scale: scale,
                    last_cursor_pos: egui::Pos2::ZERO,
                    device_lost,
                    font,
                    visuals: handler_visuals,
                    param_ids,
                    param_snapshot,
                    force_paint: true,
                    next_paint_at: None,
                    min_size,
                    max_size,
                    aspect_ratio,
                    resize_corrector: ResizeCorrector::default(),
                    #[cfg(not(target_os = "linux"))]
                    pending_correct: None,
                    resize_seen: (0, 0),
                    resize_burst_start: None,
                    last_size_change: None,
                    pacer: truce_gui::PaintPacer::default(),
                    last_resize_phys: (0, 0),
                    last_resize_fitted: (0, 0),
                }
            },
        );

        self.window = Some(window);
    }

    fn close(&mut self) {
        if let Some(mut window) = self.window.take() {
            window.close();
        }
    }

    fn idle(&mut self) {
        // baseview drives its own frame loop via on_frame().
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 {
            return false;
        }
        self.size = (width, height);
        // Hand the new logical size off to the live baseview handler.
        // It picks the change up at the top of `on_frame`, calls
        // `Window::resize`, and reconfigures the wgpu surface so the
        // next frame paints at the new size. If no editor is open the
        // store still primes the cell for the next `open()` call (which
        // re-syncs from `self.size` anyway, so the value is harmless).
        self.pending_size
            .store(pack_size((width, height)), Ordering::Relaxed);
        true
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

    fn set_scale_factor(&mut self, factor: f64) {
        // Write to the shared cell; the baseview handler picks up the
        // change on its next frame and resizes the wgpu surface +
        // renderer to match. No explicit notification needed -
        // baseview's frame loop polls.
        self.host_scale_set = true;
        self.scale.set(factor);
    }

    fn set_uses_system_scale(&mut self, yes: bool) {
        self.use_system_scale = yes;
    }

    fn state_changed(&mut self) {
        if let Some(ref ctx) = self.context {
            self.ui
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .state_changed(ctx);
        }
    }

    fn screenshot(
        &mut self,
        _params: Arc<dyn truce_params::Params>,
    ) -> Option<(Vec<u8>, u32, u32)> {
        let context = truce_core::editor::for_test_params(self.params.clone() as Arc<dyn Params>)
            .with_params(self.params.clone());
        // Match the live editor's content scale so the screenshot
        // exercises the same render path the user sees, not a fixed
        // 2× rasterization that hides scale-dependent layout bugs.
        // `EditorScale` falls back to `backing_scale()` when the host
        // never called `set_scale_factor`, so headless / pre-open
        // screenshots still get a sensible value (2.0 on Retina, 1.0
        // elsewhere). Tests that need deterministic output can
        // override via the `--scale` CLI flag in
        // `cargo truce screenshot` (which threads through to
        // `set_scale_factor` before this method runs).
        let pixels_per_point = self.scale.get_f32();
        let ui = Arc::clone(&self.ui);
        crate::screenshot::render_with_state::<P>(
            &context,
            self.size,
            pixels_per_point,
            self.font,
            self.visuals.clone(),
            move |root_ui, state| {
                ui.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .ui(root_ui, state);
            },
        )
    }
}

impl<P: Params + ?Sized> Drop for EguiEditor<P> {
    fn drop(&mut self) {
        // `baseview::WindowHandle` does not cancel the macOS frame timer
        // on drop, so a host that drops the editor without calling
        // `Editor::close` leaves the timer firing `on_frame`. Unlike the
        // cpu/iced raw-pointer handlers this can't use-after-free (the
        // handler holds owned `Arc`/`EditorScale` clones), but it keeps
        // rendering into a torn-down surface. Mirror `close`'s window
        // teardown here; idempotent via `self.window.take()`. (Inlined
        // rather than calling `Editor::close` because that impl requires
        // `P: Sized` while this `Drop` must match the struct's `?Sized`.)
        if let Some(mut window) = self.window.take() {
            window.close();
        }
    }
}
