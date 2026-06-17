//! `IcedEditor` - implements `truce_core::Editor` using iced for rendering.
//!
//! Drives iced's `UserInterface` directly each frame against a wgpu
//! surface provided by baseview. Used to lean on
//! `iced_runtime::program::State` for this; that surface was removed
//! in iced 0.14, so this module now manages the build / update / draw
//! / cache cycle inline.

use std::fmt::Debug;
use std::sync::Arc;

use iced::{Color, Event, Point, Size, Task};
use iced_wgpu::wgpu;
use truce_core::editor::{Editor, PluginContext};
use truce_gui::EditorScale;
use truce_gui::layout::GridLayout;
use truce_params::Params;

use crate::auto_layout;
use crate::param_cache::ParamCache;
use crate::param_message::{Message, ParamMessage};

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

    /// Build the view.
    fn view<'a>(&'a self, params: &'a ParamCache<P>) -> iced::Element<'a, Message<Self::Message>>;

    /// Custom theme (default: truce dark).
    fn theme(&self) -> iced::Theme {
        crate::theme::truce_dark_theme()
    }

    /// Window title.
    fn title(&self) -> String {
        String::from("Plugin")
    }

    /// Plugin state was restored (preset recall, undo, session load).
    /// Re-read any cached custom state. Parameter values update automatically.
    fn state_changed(&mut self) {}
}

// AutoPlugin - built-in plugin for GridLayout auto mode

/// Built-in `IcedPlugin` that generates a view from a `GridLayout`.
pub struct AutoPlugin {
    layout: GridLayout,
}

impl<P: Params> IcedPlugin<P> for AutoPlugin {
    type Message = (); // No custom messages in auto mode

    fn new(_params: Arc<P>) -> Self {
        panic!("AutoPlugin must be created via IcedEditor::from_layout");
    }

    fn view<'a>(&'a self, params: &'a ParamCache<P>) -> iced::Element<'a, Message<()>> {
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
                self.param_cache.sync(&self.context);
                self.param_cache.sync_meters(&self.context, &self.meter_ids);
            }
            other => {
                let _: Task<Message<M::Message>> =
                    self.plugin.update(other, &self.param_cache, &self.context);
            }
        }
    }

    pub(crate) fn view(&self) -> iced::Element<'_, Message<M::Message>> {
        self.plugin.view(&self.param_cache)
    }
}

// IcedEditor - main entry point, implements truce_core::Editor

/// Iced-based plugin editor.
///
/// Type parameters:
/// - `P` - the plugin's `Params` type
/// - `M` - the plugin's `IcedPlugin` implementation
pub struct IcedEditor<P, M>
where
    P: Params + 'static,
    M: IcedPlugin<P>,
{
    params: Arc<P>,
    size: (u32, u32),
    /// Live content-scale factor, shared with the runtime via
    /// [`truce_gui::EditorScale`]. Both `set_scale_factor` (host) and
    /// the baseview `Resized` handler write here; the runtime's
    /// `tick()` reads it and reconfigures the surface/viewport when it
    /// diverges from `last_applied_scale`.
    scale: EditorScale,
    font: Option<&'static [u8]>,
    runtime: Option<IcedRuntime<P, M>>,
    /// Constructor closure for the plugin model. Each constructor
    /// stores a closure that produces an `M` of the correct concrete
    /// type:
    /// - `from_layout` captures the `GridLayout` and returns
    ///   `AutoPlugin { layout: layout.clone() }` (the `impl` block
    ///   fixes `M = AutoPlugin`).
    /// - `new` defers to `M::new(params)`.
    ///
    /// `Fn` (not `FnOnce`) so `open()` and `screenshot()` can each
    /// produce a fresh `M`. Hosts that destroy and recreate the editor
    /// (CLAP `gui_destroy` / `gui_create`) call `open()` more than once;
    /// `screenshot()` builds a separate offscreen iced program. The
    /// closure also carries the construction invariant for `AutoPlugin`,
    /// whose `IcedPlugin::new` is `panic!("must be created via
    /// from_layout")` - going through `M::new` instead would panic on
    /// the screenshot path.
    make_plugin: Box<dyn Fn(Arc<P>) -> M + Send + Sync>,
    meter_ids: Vec<u32>,
    baseview_window: Option<baseview::WindowHandle>,
    /// Pending logical size shared with the baseview handler.
    /// Packed as `(width << 32) | height`; `0` is the "no resize
    /// pending" sentinel. `Editor::set_size` writes here so the
    /// handler's `on_frame` can call `baseview::Window::resize`
    /// (which sets the NSView/HWND/X11 frame on the underlying
    /// platform window) and reconfigure the wgpu surface in one
    /// place. Without this handoff the wgpu surface gets the new
    /// size but the platform window stays at its original
    /// dimensions, so the editor renders into a viewport but the
    /// host only paints the un-resized rectangle (visible on
    /// standalone as an editor that fills the original area only
    /// while the outer window grew around it).
    pending_size: Arc<std::sync::atomic::AtomicU64>,
    /// Resize-capability flag exposed via `Editor::can_resize`.
    /// Defaults to `false`; iced plugins that have been designed
    /// with a flexible widget tree opt in with `.resizable(true)`.
    /// The default keeps every existing fixed-size plugin pinned
    /// to its built dimensions instead of silently following an
    /// autoresize-driven parent `NSView` grow.
    can_resize: bool,
    /// Whether the standalone host may maximize the window, exposed
    /// via `Editor::can_maximize`. Defaults to `false`; only consulted
    /// for resizable editors. Opt in with `.maximizable(true)` for
    /// editors that render correctly at any size.
    can_maximize: bool,
    /// Constraints exposed through the `Editor` trait so format
    /// wrappers can hand the host honest bounds.
    min_size: (u32, u32),
    max_size: (u32, u32),
    aspect_ratio: Option<(u32, u32)>,
    prefers_pow2: bool,
}

// SAFETY: `baseview::WindowHandle` holds a raw native window pointer
// (HWND / NSView / X11 Window) and is not auto-`Send`. Hosts call
// `Editor::open` / `idle` / `close` from a single dedicated GUI thread
// - never concurrently and never from the audio thread - so the
// handle is only ever touched on the thread that created it. The
// `Editor` trait requires `Send` so the editor can live behind a
// trait object; this impl asserts that the type doesn't escape its
// thread in practice. The `make_plugin` boxed closure is already
// `Send`-bounded; runtime / meter_ids / size are trivially `Send`.
unsafe impl<P: Params, M: IcedPlugin<P>> Send for IcedEditor<P, M> {}

impl<P: Params + 'static, M: IcedPlugin<P> + 'static> Drop for IcedEditor<P, M> {
    /// Defensive cleanup for hosts that drop the editor without first
    /// calling `Editor::close`. Pro Tools AAX has been seen to do this
    /// on plugin removal under certain conditions; live-coding hosts
    /// and unit tests can also short-circuit the lifecycle. On Linux
    /// `baseview::WindowHandle` has no `Drop`, so without an explicit
    /// `close` the render thread would keep running against a freed
    /// `*mut IcedEditor` and later panic inside wgpu as surfaces tear
    /// down. `close()` is idempotent - `baseview_window.take()`
    /// no-ops on the second call - so calling it here on top of a
    /// well-behaved host's earlier `close()` is safe.
    fn drop(&mut self) {
        Editor::close(self);
    }
}

impl<P: Params + 'static> IcedEditor<P, AutoPlugin> {
    /// Create an editor that auto-generates the UI from a `GridLayout`.
    pub fn from_layout(params: Arc<P>, layout: GridLayout) -> Self {
        let size = (layout.width, layout.height);
        let meter_ids: Vec<u32> = layout
            .widgets
            .iter()
            .filter_map(|w| w.meter_ids.as_ref())
            .flatten()
            .copied()
            .collect();

        let make_plugin: Box<dyn Fn(Arc<P>) -> AutoPlugin + Send + Sync> =
            Box::new(move |_params| AutoPlugin {
                layout: layout.clone(),
            });

        Self {
            params,
            size,
            scale: EditorScale::new(truce_gui::backing_scale()),
            font: None,
            runtime: None,
            make_plugin,
            meter_ids,
            baseview_window: None,
            pending_size: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            can_resize: false,
            can_maximize: false,
            min_size: (1, 1),
            max_size: (u32::MAX, u32::MAX),
            aspect_ratio: None,
            prefers_pow2: false,
        }
    }
}

impl<P: Params + 'static, M: IcedPlugin<P> + 'static> IcedEditor<P, M> {
    /// Create an editor with a custom `IcedPlugin` implementation.
    pub fn new(params: Arc<P>, size: (u32, u32)) -> Self {
        Self {
            params,
            size,
            scale: EditorScale::new(truce_gui::backing_scale()),
            font: None,
            runtime: None,
            make_plugin: Box::new(|p| M::new(p)),
            meter_ids: Vec::new(),
            baseview_window: None,
            pending_size: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            can_resize: false,
            can_maximize: false,
            min_size: (1, 1),
            max_size: (u32::MAX, u32::MAX),
            aspect_ratio: None,
            prefers_pow2: false,
        }
    }

    /// Set a custom default font. The family name is read from the
    /// TTF `name` table - matches the `with_font(bytes)` shape used
    /// by `truce-egui::EguiEditor` and `truce-vizia::ViziaEditor`.
    ///
    /// ```ignore
    /// IcedEditor::new(params, (250, 330))
    ///     .with_font(truce_font::JETBRAINS_MONO)
    /// ```
    #[must_use]
    pub fn with_font(mut self, data: &'static [u8]) -> Self {
        self.font = Some(data);
        self
    }

    /// Set meter IDs to poll each tick.
    #[must_use]
    pub fn with_meter_ids(mut self, ids: Vec<impl Into<u32>>) -> Self {
        self.meter_ids = ids.into_iter().map(std::convert::Into::into).collect();
        self
    }

    /// Opt out of host-driven resizing. iced editors default to
    /// resizable because the widget tree reflows for free; pass
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
    /// standalone host consults this, and only when `resizable(true)`.
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

    /// Maximum logical-point dimensions the editor accepts.
    #[must_use]
    pub fn max_size(mut self, max: (u32, u32)) -> Self {
        self.max_size = max;
        self
    }

    /// Lock the aspect ratio as `(numerator, denominator)`. Pass
    /// `None` (the default) for free resizing.
    #[must_use]
    pub fn aspect_ratio(mut self, ratio: Option<(u32, u32)>) -> Self {
        self.aspect_ratio = ratio;
        self
    }

    /// Hint that the renderer prefers power-of-two surface sizes.
    #[must_use]
    pub fn prefers_pow2(mut self, prefers: bool) -> Self {
        self.prefers_pow2 = prefers;
        self
    }
}

// IcedRuntime - active iced state (exists only while editor is open)

struct IcedRuntime<P: Params, M: IcedPlugin<P>> {
    /// Rendering pipeline - initialized lazily when the baseview window
    /// finishes building and a wgpu surface is available.
    render: Option<RenderState<P, M>>,
    /// Current cursor position in logical coordinates.
    cursor_position: Point,
    /// Pending iced events queued by mouse callbacks.
    pending_events: Vec<Event>,
    /// Plugin creation info (consumed during render init).
    program: Option<IcedProgram<P, M>>,
    /// Editor size for viewport.
    size: (u32, u32),
    /// Live scale factor (clone of the editor's). Source of truth for
    /// every render path; written by `Editor::set_scale_factor` and
    /// the baseview `Resized` handler, observed each `tick()`.
    scale: EditorScale,
    /// Last scale value the surface/viewport were configured for. When
    /// `scale.get()` diverges from this, `tick()` reconfigures and
    /// updates this snapshot.
    last_applied_scale: f64,
    /// Custom font's TrueType bytes. Family name is recovered by
    /// `crate::font::apply_font` from the TTF `name` table.
    font: Option<&'static [u8]>,
}

/// Holds the full wgpu + iced rendering pipeline.
///
/// Replaces what `iced_runtime::program::State` used to encapsulate
/// in our 0.13 setup: we own the plugin model + the `UserInterface`
/// cache that lets iced reuse layout work between frames, and drive
/// the build / update / draw / extract-cache cycle by hand each
/// `tick()`.
struct RenderState<P: Params + 'static, M: IcedPlugin<P>> {
    /// Cloned wgpu handle for surface (re)configuration. The "primary"
    /// device + queue handles live inside `renderer`'s `Engine`.
    device: wgpu::Device,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    renderer: iced_wgpu::Renderer,
    program: IcedProgram<P, M>,
    /// `iced_runtime::UserInterface` cache between frames. Holds widget
    /// internal state (focus, scroll positions, ...) so we don't lose
    /// it between layout passes. `None` only mid-`tick()` between
    /// build and extract.
    ui_cache: Option<iced_runtime::user_interface::Cache>,
    /// Most recent mouse interaction reported by the UI's draw pass.
    /// Polled by the baseview handler to update the OS cursor.
    interaction: iced::mouse::Interaction,
    viewport: iced_graphics::Viewport,
    theme: iced::Theme,
    bg_color: Color,
}

impl<P: Params + 'static, M: IcedPlugin<P>> IcedRuntime<P, M> {
    /// Initialize the wgpu + iced rendering pipeline from a pre-created surface.
    //
    // `instance` and `surface` are threaded into the iced renderer; the
    // owned-arg shape avoids a clone at the call site.
    #[allow(clippy::needless_pass_by_value)]
    fn init_render(&mut self, instance: wgpu::Instance, surface: wgpu::Surface<'static>) -> bool {
        let Some(program) = self.program.take() else {
            return false;
        };

        let (lw, lh) = self.size;
        // Read from the shared cell (clone of the editor's scale). Re-
        // querying `truce_gui::backing_scale()` would drop a host-
        // supplied value and on Linux the process-wide cache may not
        // have been populated yet, so the first frame would render at
        // 1.0 even on a HiDPI display.
        let render_scale = self.scale.get();
        self.last_applied_scale = render_scale;
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
                    self.program = Some(program);
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
                    self.program = Some(program);
                    return false;
                }
            };

        let surface_caps = surface.get_capabilities(&adapter);
        if surface_caps.formats.is_empty() {
            log::warn!("no surface formats available");
            self.program = Some(program);
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

        // wgpu::Device / Queue are cheaply Clone-able (internally Arc'd);
        // hand the canonical pair to `Engine::new` and keep clones for
        // post-init surface reconfiguration.
        let surface_device = device.clone();
        let engine = iced_wgpu::Engine::new(
            &adapter,
            device,
            queue,
            surface_format,
            Some(iced_graphics::Antialiasing::MSAAx4),
            iced_graphics::Shell::headless(),
        );

        let default_font = if let Some(data) = self.font {
            crate::font::apply_font(data)
        } else {
            iced::Font::DEFAULT
        };
        let renderer = iced_wgpu::Renderer::new(engine, default_font, iced::Pixels(14.0));

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
            interaction: iced::mouse::Interaction::default(),
            viewport,
            theme,
            bg_color: bg,
        });

        log::info!("gpu active (wgpu, {w}x{h})");
        true
    }

    /// Drive one frame: update iced state + present to surface.
    fn tick(&mut self) {
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
        if cur_scale.to_bits() != self.last_applied_scale.to_bits() {
            let (lw, lh) = self.size;
            let pw = truce_gui::to_physical_px(lw, cur_scale);
            let ph = truce_gui::to_physical_px(lh, cur_scale);
            render.surface_config.width = pw;
            render.surface_config.height = ph;
            render
                .surface
                .configure(&render.device, &render.surface_config);
            #[allow(clippy::cast_possible_truncation)] // display DPI; bounded
            let scale_f32 = cur_scale as f32;
            render.viewport =
                iced_graphics::Viewport::with_physical_size(Size::new(pw, ph), scale_f32);
            self.last_applied_scale = cur_scale;
        }

        // Process the per-frame "sync params and meters" tick + any
        // events queued by baseview before we touch iced. Tick first so
        // the view rebuilt below sees fresh shadow values; events after
        // are folded into the same UserInterface update pass.
        render.program.dispatch(Message::Tick);

        let cursor = iced::mouse::Cursor::Available(self.cursor_position);
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

        let pending_events = std::mem::take(&mut self.pending_events);
        let (ui_state, _statuses) = user_interface.update(
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
            mouse_interaction, ..
        } = ui_state
        {
            render.interaction = mouse_interaction;
        }

        user_interface.draw(&mut render.renderer, &render.theme, &style, cursor);

        render.ui_cache = Some(user_interface.into_cache());

        // Now we can mutate the program again - drain any messages the
        // event handlers produced.
        for message in messages {
            render.program.dispatch(message);
        }

        // Present: get surface texture, render, submit. iced 0.14's
        // `Renderer::present` builds its own encoder + submits to the
        // queue internally, so we no longer manage either by hand.
        let frame = match render.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Timeout | wgpu::SurfaceError::Outdated) => {
                render
                    .surface
                    .configure(&render.device, &render.surface_config);
                return;
            }
            Err(e) => {
                log::warn!("surface error: {e}");
                return;
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

        frame.present();
    }

    /// Queue a cursor move event. Coordinates are in logical points.
    fn queue_cursor_move(&mut self, x: f32, y: f32) {
        self.cursor_position = Point::new(x, y);
        self.pending_events
            .push(Event::Mouse(iced::mouse::Event::CursorMoved {
                position: self.cursor_position,
            }));
    }
}

// Baseview window handler (all platforms)

struct IcedBaseviewHandler<P: Params + 'static, M: IcedPlugin<P>> {
    editor: *mut IcedEditor<P, M>,
    last_cursor: Option<baseview::MouseCursor>,
}

// SAFETY: The raw `*mut IcedEditor<P, M>` is only dereferenced from
// the baseview event thread (which `WindowHandler` is bound to). The
// editor's host-side close path joins this thread before dropping the
// editor, so the pointer is always valid while `WindowHandler`
// methods run. baseview requires `Send` for its handler types so that
// the handler can be moved onto the dedicated event thread on
// construction; once moved, it never crosses threads again.
unsafe impl<P: Params, M: IcedPlugin<P>> Send for IcedBaseviewHandler<P, M> {}

impl<P: Params + 'static, M: IcedPlugin<P>> Drop for IcedBaseviewHandler<P, M> {
    fn drop(&mut self) {
        // Drop wgpu/iced render state on the baseview event thread, while
        // any underlying display connection (e.g. X11 Display via XcbConnection)
        // is still alive. If we let the host-thread close() path drop
        // `runtime.render` instead, NVIDIA's Vulkan surface-destruction code
        // tries to use a freed Display and segfaults inside _XSend.
        //
        // Safety: close() always calls window.close() which joins this
        // thread before returning. While this drop runs, the host thread
        // is blocked in join(), so `self.editor` is still valid.
        let editor = unsafe { &mut *self.editor };
        if let Some(ref mut runtime) = editor.runtime {
            runtime.render = None;
        }
    }
}

// The explicit `Idle | None => Default` arm documents iced's known
// no-cursor states; the trailing `_ => Default` keeps forward-compat
// against future iced enum variants. Both intentionally share the
// value.
#[allow(clippy::match_same_arms)]
fn iced_interaction_to_cursor(interaction: iced::mouse::Interaction) -> baseview::MouseCursor {
    use iced::mouse::Interaction;
    match interaction {
        Interaction::Idle | Interaction::None => baseview::MouseCursor::Default,
        Interaction::Pointer | Interaction::Grab => baseview::MouseCursor::Hand,
        Interaction::Grabbing => baseview::MouseCursor::HandGrabbing,
        Interaction::Text => baseview::MouseCursor::Text,
        Interaction::Crosshair => baseview::MouseCursor::Crosshair,
        Interaction::NotAllowed => baseview::MouseCursor::NotAllowed,
        Interaction::ResizingHorizontally => baseview::MouseCursor::EwResize,
        Interaction::ResizingVertically => baseview::MouseCursor::NsResize,
        _ => baseview::MouseCursor::Default,
    }
}

impl<P: Params + 'static, M: IcedPlugin<P>> baseview::WindowHandler for IcedBaseviewHandler<P, M> {
    fn on_frame(&mut self, window: &mut baseview::Window) {
        // Re-anchor each frame so the child NSView's origin tracks
        // size changes against the host's plug-in pane - without it
        // the canvas drifts off-anchor as it grows, clipping the
        // layout's top off the visible area in CLAP hosts (REAPER).
        #[cfg(target_os = "macos")]
        {
            use raw_window_handle::HasRawWindowHandle;
            // Skip the whole frame while detached or occluded - a
            // non-visible window can't present, so rendered drawables
            // pile up unbounded until it returns to front.
            if truce_gui::platform::should_skip_frame(window.raw_window_handle()) {
                return;
            }
            truce_gui::platform::reanchor_to_superview_top(window.raw_window_handle());
        }
        let editor = unsafe { &mut *self.editor };
        // Pick up host-driven `set_size` requests since the last
        // frame. Without this the wgpu surface would be at the new
        // size but the platform window stays at the original
        // dimensions, so the editor visibly fills only the old
        // rect inside a larger host frame.
        let packed = editor
            .pending_size
            .swap(0, std::sync::atomic::Ordering::Acquire);
        if packed != 0 {
            #[allow(clippy::cast_possible_truncation)]
            let new_w = (packed >> 32) as u32;
            #[allow(clippy::cast_possible_truncation)]
            let new_h = (packed & 0xFFFF_FFFF) as u32;
            if new_w > 0 && new_h > 0 {
                window.resize(baseview::Size::new(f64::from(new_w), f64::from(new_h)));
                if let Some(ref mut runtime) = editor.runtime {
                    runtime.size = (new_w, new_h);
                    if let Some(ref mut render) = runtime.render {
                        let scale = editor.scale.get();
                        let pw = truce_gui::to_physical_px(new_w, scale);
                        let ph = truce_gui::to_physical_px(new_h, scale);
                        #[allow(clippy::cast_possible_truncation)]
                        let scale_f32 = scale as f32;
                        render.viewport = iced_graphics::Viewport::with_physical_size(
                            Size::new(pw, ph),
                            scale_f32,
                        );
                        render.surface_config.width = pw;
                        render.surface_config.height = ph;
                        render
                            .surface
                            .configure(&render.device, &render.surface_config);
                    }
                }
            }
        }
        if let Some(ref mut runtime) = editor.runtime {
            runtime.tick();
            if let Some(ref render) = runtime.render {
                let cursor = iced_interaction_to_cursor(render.interaction);
                if self.last_cursor != Some(cursor) {
                    self.last_cursor = Some(cursor);
                    window.set_mouse_cursor(cursor);
                }
            }
        }
    }

    fn on_event(
        &mut self,
        #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
        window: &mut baseview::Window,
        event: baseview::Event,
    ) -> baseview::EventStatus {
        let editor = unsafe { &mut *self.editor };
        let Some(runtime) = editor.runtime.as_mut() else {
            return baseview::EventStatus::Ignored;
        };

        match event {
            baseview::Event::Mouse(mouse) => {
                match mouse {
                    baseview::MouseEvent::CursorMoved { position, .. } => {
                        // baseview reports logical points; iced widgets
                        // hit-test in logical units against
                        // `viewport.logical_size()`, so forward as-is.
                        // Window dimensions stay well below 2^23 - the
                        // f64 → f32 narrowing is invisible.
                        #[allow(clippy::cast_possible_truncation)]
                        let pos = (position.x as f32, position.y as f32);
                        runtime.queue_cursor_move(pos.0, pos.1);
                    }
                    baseview::MouseEvent::CursorLeft => {
                        runtime
                            .pending_events
                            .push(Event::Mouse(iced::mouse::Event::CursorLeft));
                    }
                    baseview::MouseEvent::ButtonPressed {
                        button: baseview::MouseButton::Left,
                        ..
                    } => {
                        // WS_CHILD plugin windows don't receive WM_KEYDOWN
                        // until focused; baseview doesn't SetFocus on click,
                        // so we do it here. Without this, text-edit widgets
                        // never see keystrokes on Windows.
                        #[cfg(target_os = "windows")]
                        {
                            if !window.has_focus() {
                                window.focus();
                            }
                        }
                        runtime.pending_events.push(Event::Mouse(
                            iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
                        ));
                    }
                    baseview::MouseEvent::ButtonReleased {
                        button: baseview::MouseButton::Left,
                        ..
                    } => {
                        runtime.pending_events.push(Event::Mouse(
                            iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
                        ));
                    }
                    baseview::MouseEvent::WheelScrolled { delta, .. } => {
                        let dy = match delta {
                            baseview::ScrollDelta::Lines { y, .. } => y * 30.0,
                            baseview::ScrollDelta::Pixels { y, .. } => y,
                        };
                        runtime.pending_events.push(Event::Mouse(
                            iced::mouse::Event::WheelScrolled {
                                delta: iced::mouse::ScrollDelta::Pixels { x: 0.0, y: dy },
                            },
                        ));
                    }
                    _ => return baseview::EventStatus::Ignored,
                }
                baseview::EventStatus::Captured
            }
            baseview::Event::Window(baseview::WindowEvent::Resized(info)) => {
                crate::platform::note_linux_scale_factor(info.scale());
                // Mirror the OS-reported scale into the shared cell
                // (so a follow-up `set_scale_factor` from the host
                // reads a fresh baseline) and bump `last_applied_scale`
                // so `tick()`'s diff-check stays a no-op - we apply
                // the reconfigure inline below.
                runtime.scale.set(info.scale());
                runtime.last_applied_scale = info.scale();
                if let Some(ref mut render) = runtime.render {
                    let pw = info.physical_size().width;
                    let ph = info.physical_size().height;
                    render.surface_config.width = pw.max(1);
                    render.surface_config.height = ph.max(1);
                    render
                        .surface
                        .configure(&render.device, &render.surface_config);
                    #[allow(clippy::cast_possible_truncation)] // display DPI; bounded
                    let scale_f32 = info.scale() as f32;
                    render.viewport =
                        iced_graphics::Viewport::with_physical_size(Size::new(pw, ph), scale_f32);
                }
                baseview::EventStatus::Captured
            }
            _ => baseview::EventStatus::Ignored,
        }
    }
}

// Editor trait implementation

impl<P: Params + 'static, M: IcedPlugin<P>> Editor for IcedEditor<P, M> {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: truce_core::editor::RawWindowHandle, context: PluginContext) {
        let (w, h) = self.size;
        // Drop any stale `set_size` that fired before this
        // `open()` so the handler doesn't immediately re-resize
        // the freshly-built window to a previous request.
        self.pending_size
            .store(0, std::sync::atomic::Ordering::Relaxed);

        // Create the plugin model. The closure is `Fn`, not `FnOnce`,
        // so destroy/recreate cycles (CLAP `gui_destroy` / `gui_create`,
        // some VST3 hosts that close+reopen the editor) reuse it.
        let plugin = (self.make_plugin)(self.params.clone());

        let mut param_cache = ParamCache::new(self.params.clone());
        if let Some(data) = self.font {
            // `apply_font` is idempotent on the iced font-system side
            // (load_font is fine to call twice with the same bytes);
            // the redundant load here is cheap and lets the canvas
            // widgets reuse the correct family without threading the
            // already-derived `iced::Font` from the runtime path.
            param_cache.set_font(crate::font::apply_font(data));
        }
        let typed_ctx = context.with_params(self.params.clone());
        let program = IcedProgram {
            plugin,
            param_cache,
            context: typed_ctx,
            meter_ids: self.meter_ids.clone(),
        };

        self.runtime = Some(IcedRuntime {
            render: None,
            cursor_position: Point::ORIGIN,
            pending_events: Vec::new(),
            program: Some(program),
            size: (w, h),
            scale: self.scale.clone(),
            // init_render writes the real value; this placeholder
            // never reaches a render call.
            last_applied_scale: 0.0,
            font: self.font,
        });

        let parent_wrapper = crate::platform::ParentWindow(parent);
        let options = baseview::WindowOpenOptions {
            title: String::from("truce-iced"),
            size: baseview::Size::new(f64::from(w), f64::from(h)),
            scale: baseview::WindowScalePolicy::SystemScaleFactor,
        };

        let editor_addr = std::ptr::from_mut::<IcedEditor<P, M>>(self) as usize;

        let window = baseview::Window::open_parented(
            &parent_wrapper,
            options,
            move |window: &mut baseview::Window| {
                let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
                    backends: wgpu::Backends::PRIMARY,
                    ..Default::default()
                });

                let surface = unsafe { crate::platform::create_wgpu_surface(&instance, window) };

                if let Some(surface) = surface {
                    let editor = unsafe { &mut *(editor_addr as *mut IcedEditor<P, M>) };
                    if let Some(ref mut runtime) = editor.runtime {
                        runtime.init_render(instance, surface);
                    }
                }

                IcedBaseviewHandler::<P, M> {
                    editor: editor_addr as *mut IcedEditor<P, M>,
                    last_cursor: None,
                }
            },
        );

        self.baseview_window = Some(window);
        log::info!("editor opened via baseview ({w}x{h})");
    }

    fn close(&mut self) {
        // baseview's Linux WindowHandle has no Drop impl - we must call
        // close() explicitly to request shutdown and join the render
        // thread. Without this, the thread keeps running against a
        // dangling self pointer after the host drops this editor, which
        // later panics inside wgpu as surfaces get torn down.
        if let Some(mut window) = self.baseview_window.take() {
            window.close();
        }
        self.runtime = None;
        log::info!("editor closed");
    }

    fn idle(&mut self) {
        // baseview drives its own frame loop via on_frame().
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

    fn screenshot(
        &mut self,
        _params: Arc<dyn truce_params::Params>,
    ) -> Option<(Vec<u8>, u32, u32)> {
        // Build the plugin via the editor's own constructor closure.
        // Calling `M::new` directly would panic for `AutoPlugin` -
        // `from_layout` captures the `GridLayout` in the closure and
        // the `IcedPlugin::new` impl on `AutoPlugin` is `panic!("must
        // be created via from_layout")`.
        let plugin = (self.make_plugin)(Arc::clone(&self.params));
        // Match the live editor's content scale so the screenshot
        // exercises the same render path the user sees. `EditorScale`
        // falls back to `backing_scale()` for pre-open / headless
        // calls.
        let scale = self.scale.get();
        crate::screenshot::render_to_pixels::<P, M>(
            Arc::clone(&self.params),
            plugin,
            self.size,
            scale,
            self.font,
        )
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 {
            return false;
        }
        self.size = (width, height);
        // Hand the new logical size to the live baseview handler;
        // its `on_frame` reads the cell and runs the unified
        // `baseview::Window::resize` + iced viewport + wgpu
        // surface reconfigure sequence in one place. The handler
        // also exists when the editor isn't open, but the cell
        // gets reset to `0` in `open()` to drop any stale write.
        self.pending_size.store(
            (u64::from(width) << 32) | u64::from(height),
            std::sync::atomic::Ordering::Release,
        );
        true
    }

    fn set_scale_factor(&mut self, factor: f64) {
        // Write to the shared cell; the runtime's `tick()` picks up the
        // change on its next frame and reconfigures the surface and
        // viewport.
        self.scale.set(factor);
    }
}
