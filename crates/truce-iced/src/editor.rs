//! `IcedEditor` - implements `truce_core::Editor` using iced for rendering.
//!
//! Uses `iced_runtime::program::State` for manual iced runtime driving
//! and `iced_wgpu` for GPU-accelerated rendering, embedded as a child
//! of the host's parent window via baseview.

use std::fmt::Debug;
use std::sync::Arc;

use iced::{Color, Event, Point, Size, Task};
use iced_wgpu::wgpu;
use truce_core::editor::{Editor, PluginContext};
use truce_gui::EditorScale;
use truce_gui::layout::GridLayout;
use truce_params::Params;

// Use iced_wgpu::Renderer directly (matches iced::Renderer when tiny-skia is disabled)
type IcedRenderer = iced_wgpu::Renderer;

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

// IcedProgram - adapts IcedPlugin to iced_runtime::Program

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
}

impl<P: Params + 'static, M: IcedPlugin<P>> iced_runtime::Program for IcedProgram<P, M> {
    type Renderer = IcedRenderer;
    type Theme = iced::Theme;
    type Message = Message<M::Message>;

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        // Handle param messages - forward to host
        if let Message::Param(ref param_msg) = message {
            self.apply_param_message(param_msg);
        }

        match message {
            Message::Tick => {
                // Sync params and meters from atomics
                self.param_cache.sync(&self.context);
                self.param_cache.sync_meters(&self.context, &self.meter_ids);
                Task::none()
            }
            other => self.plugin.update(other, &self.param_cache, &self.context),
        }
    }

    fn view(&self) -> iced::Element<'_, Self::Message> {
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
    font: Option<(&'static str, &'static [u8])>,
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
        }
    }

    /// Set a custom default font (family name + TrueType data).
    ///
    /// ```ignore
    /// IcedEditor::new(params, (250, 330))
    ///     .with_font("JetBrains Mono", truce_gui::font::JETBRAINS_MONO)
    /// ```
    #[must_use]
    pub fn with_font(mut self, family: &'static str, data: &'static [u8]) -> Self {
        self.font = Some((family, data));
        self
    }

    /// Set meter IDs to poll each tick.
    #[must_use]
    pub fn with_meter_ids(mut self, ids: Vec<impl Into<u32>>) -> Self {
        self.meter_ids = ids.into_iter().map(std::convert::Into::into).collect();
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
    /// Custom font (family name, TrueType data).
    font: Option<(&'static str, &'static [u8])>,
}

/// Holds the full wgpu + iced rendering pipeline.
struct RenderState<P: Params, M: IcedPlugin<P>> {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    engine: iced_wgpu::Engine,
    renderer: iced_wgpu::Renderer,
    state: iced_runtime::program::State<IcedProgram<P, M>>,
    viewport: iced_graphics::Viewport,
    debug: iced_runtime::Debug,
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

        let Some(adapter) =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            }))
        else {
            log::warn!("no suitable GPU adapter found");
            self.program = Some(program);
            return false;
        };

        let (device, queue) = match pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("truce-iced"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
            },
            None,
        )) {
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

        let engine = iced_wgpu::Engine::new(
            &adapter,
            &device,
            &queue,
            surface_format,
            Some(iced_graphics::Antialiasing::MSAAx4),
        );

        let default_font = if let Some((family, data)) = self.font {
            crate::font::apply_font(family, data)
        } else {
            iced::Font::DEFAULT
        };
        let mut renderer =
            iced_wgpu::Renderer::new(&device, &engine, default_font, iced::Pixels(14.0));

        let viewport = iced_graphics::Viewport::with_physical_size(Size::new(w, h), render_scale);
        let mut debug = iced_runtime::Debug::new();
        let theme = program.plugin.theme();

        let state = iced_runtime::program::State::new(
            program,
            viewport.logical_size(),
            &mut renderer,
            &mut debug,
        );

        let bg = crate::theme::truce_dark_theme().palette().background;

        self.render = Some(RenderState {
            device,
            queue,
            surface,
            surface_config,
            engine,
            renderer,
            state,
            viewport,
            debug,
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
            render.viewport =
                iced_graphics::Viewport::with_physical_size(Size::new(pw, ph), cur_scale);
            self.last_applied_scale = cur_scale;
        }

        // Queue Tick message to sync params/meters
        render.state.queue_message(Message::Tick);

        // Queue any pending mouse events
        for event in self.pending_events.drain(..) {
            render.state.queue_event(event);
        }

        let cursor = iced::mouse::Cursor::Available(self.cursor_position);

        let style = iced_runtime::core::renderer::Style {
            text_color: Color::from_rgb(0.90, 0.90, 0.92),
        };

        let _ = render.state.update(
            render.viewport.logical_size(),
            cursor,
            &mut render.renderer,
            &render.theme,
            &style,
            &mut iced_runtime::core::clipboard::Null,
            &mut render.debug,
        );

        // Present: get surface texture, render, submit
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

        let mut encoder = render
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("truce-iced-encoder"),
            });

        // `Debug::overlay()` allocates a fresh `Vec<String>` from iced's
        // internal frame metrics every call; that's wasted work in
        // release where the overlay is invisible anyway.
        let overlay: Vec<String> = if cfg!(debug_assertions) {
            render.debug.overlay()
        } else {
            Vec::new()
        };
        render.renderer.present(
            &mut render.engine,
            &render.device,
            &render.queue,
            &mut encoder,
            Some(render.bg_color),
            render.surface_config.format,
            &view,
            &render.viewport,
            &overlay,
        );

        render.engine.submit(&render.queue, encoder);
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
        Interaction::Working => baseview::MouseCursor::Working,
        Interaction::NotAllowed => baseview::MouseCursor::NotAllowed,
        Interaction::ResizingHorizontally => baseview::MouseCursor::EwResize,
        Interaction::ResizingVertically => baseview::MouseCursor::NsResize,
        _ => baseview::MouseCursor::Default,
    }
}

impl<P: Params + 'static, M: IcedPlugin<P>> baseview::WindowHandler for IcedBaseviewHandler<P, M> {
    fn on_frame(&mut self, window: &mut baseview::Window) {
        let editor = unsafe { &mut *self.editor };
        if let Some(ref mut runtime) = editor.runtime {
            runtime.tick();
            if let Some(ref render) = runtime.render {
                let cursor = iced_interaction_to_cursor(render.state.mouse_interaction());
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
                    render.viewport = iced_graphics::Viewport::with_physical_size(
                        Size::new(pw, ph),
                        info.scale(),
                    );
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

        // Create the plugin model. The closure is `Fn`, not `FnOnce`,
        // so destroy/recreate cycles (CLAP `gui_destroy` / `gui_create`,
        // some VST3 hosts that close+reopen the editor) reuse it.
        let plugin = (self.make_plugin)(self.params.clone());

        let mut param_cache = ParamCache::new(self.params.clone());
        if let Some((family, _)) = self.font {
            param_cache.set_font(iced::Font {
                family: iced::font::Family::Name(family),
                ..iced::Font::DEFAULT
            });
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
                let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
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
        true
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
        self.size = (width, height);
        if let Some(ref mut runtime) = self.runtime {
            runtime.size = (width, height);
            if let Some(ref mut render) = runtime.render {
                let scale = self.scale.get();
                let pw = truce_gui::to_physical_px(width, scale);
                let ph = truce_gui::to_physical_px(height, scale);
                render.viewport =
                    iced_graphics::Viewport::with_physical_size(Size::new(pw, ph), scale);
                render.surface_config.width = pw;
                render.surface_config.height = ph;
                render
                    .surface
                    .configure(&render.device, &render.surface_config);
            }
        }
        true
    }

    fn set_scale_factor(&mut self, factor: f64) {
        // Write to the shared cell; the runtime's `tick()` picks up the
        // change on its next frame and reconfigures the surface and
        // viewport.
        self.scale.set(factor);
    }
}
