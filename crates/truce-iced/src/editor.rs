//! IcedEditor — implements `truce_core::Editor` using iced for rendering.
//!
//! Uses `iced_runtime::program::State` for manual iced runtime driving
//! and `iced_wgpu` for GPU-accelerated rendering, all embedded as a child
//! view of the DAW's parent window.

use std::ffi::c_void;
use std::fmt::Debug;
use std::ptr::NonNull;
use std::sync::Arc;

use iced::{Color, Event, Point, Size, Task};
use iced_wgpu::wgpu;
use truce_core::editor::{Editor, EditorContext};
use truce_gui::layout::GridLayout;
use truce_params::Params;

// Use iced_wgpu::Renderer directly (matches iced::Renderer when tiny-skia is disabled)
type IcedRenderer = iced_wgpu::Renderer;

use crate::auto_layout;
use crate::editor_handle::EditorHandle;
use crate::param_message::{Message, ParamMessage};
use crate::param_state::ParamState;
use crate::platform::{IcedPlatformView, IcedViewCallbacks};

// ---------------------------------------------------------------------------
// IcedPlugin trait — what plugin authors implement
// ---------------------------------------------------------------------------

/// Trait for plugin-specific iced UI logic.
///
/// Plugin authors implement this for full control over the iced view.
/// For zero-code UIs, use `IcedEditor::from_layout()` instead.
pub trait IcedPlugin<P: Params>: Sized + 'static {
    /// Plugin-specific message type.
    type Message: Debug + Clone + Send;

    /// Create the initial model.
    fn new(params: Arc<P>) -> Self;

    /// Handle a message (param change or plugin-specific).
    fn update(
        &mut self,
        message: Message<Self::Message>,
        params: &ParamState<P>,
        ctx: &EditorHandle,
    ) -> Task<Message<Self::Message>>;

    /// Build the view.
    fn view<'a>(
        &'a self,
        params: &'a ParamState<P>,
    ) -> iced::Element<'a, Message<Self::Message>>;

    /// Custom theme (default: truce dark).
    fn theme(&self) -> iced::Theme {
        crate::theme::truce_dark_theme()
    }

    /// Window title.
    fn title(&self) -> String {
        String::from("Plugin")
    }
}

// ---------------------------------------------------------------------------
// AutoPlugin — built-in plugin for GridLayout auto mode
// ---------------------------------------------------------------------------

/// Built-in IcedPlugin that generates a view from a GridLayout.
pub struct AutoPlugin {
    layout: GridLayout,
}

impl<P: Params> IcedPlugin<P> for AutoPlugin {
    type Message = (); // No custom messages in auto mode

    fn new(_params: Arc<P>) -> Self {
        panic!("AutoPlugin must be created via IcedEditor::from_layout");
    }

    fn update(
        &mut self,
        _message: Message<()>,
        _params: &ParamState<P>,
        _ctx: &EditorHandle,
    ) -> Task<Message<()>> {
        Task::none()
    }

    fn view<'a>(&'a self, params: &'a ParamState<P>) -> iced::Element<'a, Message<()>> {
        auto_layout::auto_view(&self.layout, params)
    }
}

// ---------------------------------------------------------------------------
// IcedProgram — adapts IcedPlugin to iced_runtime::Program
// ---------------------------------------------------------------------------

pub(crate) struct IcedProgram<P: Params, M: IcedPlugin<P>> {
    pub(crate) plugin: M,
    pub(crate) param_state: ParamState<P>,
    pub(crate) editor_handle: EditorHandle,
    pub(crate) meter_ids: Vec<u32>,
}

impl<P: Params, M: IcedPlugin<P>> IcedProgram<P, M> {
    fn apply_param_message(&self, msg: &ParamMessage) {
        match msg {
            ParamMessage::BeginEdit(id) => self.editor_handle.begin_edit(*id),
            ParamMessage::SetNormalized(id, val) => self.editor_handle.set_param(*id, *val),
            ParamMessage::EndEdit(id) => self.editor_handle.end_edit(*id),
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
        // Handle param messages — forward to host
        if let Message::Param(ref param_msg) = message {
            self.apply_param_message(param_msg);
        }

        match message {
            Message::Tick => {
                // Sync params and meters from atomics
                self.param_state.sync(self.editor_handle.context());
                self.param_state
                    .sync_meters(self.editor_handle.context(), &self.meter_ids);
                Task::none()
            }
            other => self
                .plugin
                .update(other, &self.param_state, &self.editor_handle),
        }
    }

    fn view(&self) -> iced::Element<'_, Self::Message> {
        self.plugin.view(&self.param_state)
    }
}

// ---------------------------------------------------------------------------
// IcedEditor — main entry point, implements truce_core::Editor
// ---------------------------------------------------------------------------

/// Iced-based plugin editor.
///
/// Type parameters:
/// - `P` — the plugin's `Params` type
/// - `M` — the plugin's `IcedPlugin` implementation
pub struct IcedEditor<P, M>
where
    P: Params + 'static,
    M: IcedPlugin<P>,
{
    params: Arc<P>,
    size: (u32, u32),
    scale_factor: f64,
    runtime: Option<IcedRuntime<P, M>>,
    layout: Option<GridLayout>,
    meter_ids: Vec<u32>,
}

unsafe impl<P: Params, M: IcedPlugin<P>> Send for IcedEditor<P, M> {}

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

        Self {
            params,
            size,
            scale_factor: truce_gui::backing_scale(),
            runtime: None,
            layout: Some(layout),
            meter_ids,
        }
    }
}

impl<P: Params + 'static, M: IcedPlugin<P>> IcedEditor<P, M> {
    /// Create an editor with a custom `IcedPlugin` implementation.
    pub fn new(params: Arc<P>, size: (u32, u32)) -> Self {
        Self {
            params,
            size,
            scale_factor: truce_gui::backing_scale(),
            runtime: None,
            layout: None,
            meter_ids: Vec::new(),
        }
    }

    /// Set meter IDs to poll each tick.
    pub fn with_meter_ids(mut self, ids: Vec<u32>) -> Self {
        self.meter_ids = ids;
        self
    }
}

// ---------------------------------------------------------------------------
// IcedRuntime — active iced state (exists only while editor is open)
// ---------------------------------------------------------------------------

struct IcedRuntime<P: Params, M: IcedPlugin<P>> {
    /// Rendering pipeline — initialized lazily on first render after setup.
    render: Option<RenderState<P, M>>,
    /// Platform view (RAII — drop destroys the NSView). Set after view creation.
    _view: Option<IcedPlatformView>,
    /// CAMetalLayer pointer received from setup callback.
    metal_layer: Option<NonNull<c_void>>,
    /// Current cursor position in logical coordinates.
    cursor_position: Point,
    /// Whether the left mouse button is pressed.
    mouse_left_pressed: bool,
    /// Pending iced events queued by mouse callbacks.
    pending_events: Vec<Event>,
    /// Plugin creation info (consumed during render init).
    program: Option<IcedProgram<P, M>>,
    /// Editor size for viewport.
    size: (u32, u32),
    /// Scale factor.
    scale_factor: f64,
}

/// Holds the full wgpu + iced rendering pipeline.
///
/// Uses manual wgpu setup (surface from CAMetalLayer) combined with
/// `iced_wgpu::Engine` and `iced_wgpu::Renderer` for iced rendering.
/// This bypasses iced's compositor to use the proven Metal layer approach.
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
    /// Initialize the wgpu + iced rendering pipeline from the Metal layer.
    fn init_render(&mut self) -> bool {
        let metal_layer = match self.metal_layer {
            Some(v) => v.as_ptr(),
            None => return false,
        };

        let program = match self.program.take() {
            Some(p) => p,
            None => return false,
        };

        let (w, h) = self.size;
        let scale = self.scale_factor;

        // Create wgpu infrastructure from the CAMetalLayer
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });

        let surface = match unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
                metal_layer,
            ))
        } {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[truce-iced] Failed to create wgpu surface: {e}");
                self.program = Some(program);
                return false;
            }
        };

        let adapter = match pollster::block_on(instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            },
        )) {
            Some(a) => a,
            None => {
                eprintln!("[truce-iced] No suitable GPU adapter found");
                self.program = Some(program);
                return false;
            }
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
                eprintln!("[truce-iced] Failed to create wgpu device: {e}");
                self.program = Some(program);
                return false;
            }
        };

        let surface_caps = surface.get_capabilities(&adapter);
        if surface_caps.formats.is_empty() {
            eprintln!("[truce-iced] No surface formats available");
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
            width: w,
            height: h,
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
            None,
        );

        let mut renderer = iced_wgpu::Renderer::new(
            &device,
            &engine,
            iced::Font::DEFAULT,
            iced::Pixels(14.0),
        );

        let viewport =
            iced_graphics::Viewport::with_physical_size(Size::new(w, h), scale);
        let mut debug = iced_runtime::Debug::new();
        let theme = program.plugin.theme();

        let state = iced_runtime::program::State::new(
            program,
            viewport.logical_size(),
            &mut renderer,
            &mut debug,
        );

        let bg = crate::theme::truce_dark_theme()
            .palette()
            .background;

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

        eprintln!("[truce-iced] GPU active (wgpu/Metal, {w}x{h})");
        true
    }

    /// Drive one frame: update iced state + present to surface.
    fn tick(&mut self) {
        if self.render.is_none() && self.metal_layer.is_some() {
            if !self.init_render() {
                return;
            }
        }

        let render = match self.render.as_mut() {
            Some(r) => r,
            None => return,
        };

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
            Err(wgpu::SurfaceError::Timeout) | Err(wgpu::SurfaceError::Outdated) => {
                render
                    .surface
                    .configure(&render.device, &render.surface_config);
                return;
            }
            Err(e) => {
                eprintln!("[truce-iced] Surface error: {e}");
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

        render.renderer.present(
            &mut render.engine,
            &render.device,
            &render.queue,
            &mut encoder,
            Some(render.bg_color),
            render.surface_config.format,
            &view,
            &render.viewport,
            &render.debug.overlay(),
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

// ---------------------------------------------------------------------------
// C callbacks from the platform view
// ---------------------------------------------------------------------------

unsafe extern "C" fn cb_setup<P: Params + 'static, M: IcedPlugin<P>>(
    ctx: *mut c_void,
    metal_layer: *mut c_void,
) {
    let editor = &mut *(ctx as *mut IcedEditor<P, M>);
    if let Some(ref mut runtime) = editor.runtime {
        runtime.metal_layer = NonNull::new(metal_layer);
        if runtime.metal_layer.is_some() {
            eprintln!("[truce-iced] View setup complete, CAMetalLayer received");
        }
    }
}

unsafe extern "C" fn cb_render<P: Params + 'static, M: IcedPlugin<P>>(ctx: *mut c_void) {
    let editor = &mut *(ctx as *mut IcedEditor<P, M>);
    if let Some(ref mut runtime) = editor.runtime {
        runtime.tick();
    }
}

unsafe extern "C" fn cb_mouse_down<P: Params + 'static, M: IcedPlugin<P>>(
    ctx: *mut c_void,
    x: f32,
    y: f32,
) {
    let editor = &mut *(ctx as *mut IcedEditor<P, M>);
    if let Some(ref mut runtime) = editor.runtime {
        runtime.queue_cursor_move(x, y);
        runtime.mouse_left_pressed = true;
        runtime
            .pending_events
            .push(Event::Mouse(iced::mouse::Event::ButtonPressed(
                iced::mouse::Button::Left,
            )));
    }
}

unsafe extern "C" fn cb_mouse_dragged<P: Params + 'static, M: IcedPlugin<P>>(
    ctx: *mut c_void,
    x: f32,
    y: f32,
) {
    let editor = &mut *(ctx as *mut IcedEditor<P, M>);
    if let Some(ref mut runtime) = editor.runtime {
        runtime.queue_cursor_move(x, y);
    }
}

unsafe extern "C" fn cb_mouse_up<P: Params + 'static, M: IcedPlugin<P>>(
    ctx: *mut c_void,
    x: f32,
    y: f32,
) {
    let editor = &mut *(ctx as *mut IcedEditor<P, M>);
    if let Some(ref mut runtime) = editor.runtime {
        runtime.queue_cursor_move(x, y);
        runtime.mouse_left_pressed = false;
        runtime
            .pending_events
            .push(Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )));
    }
}

unsafe extern "C" fn cb_scroll<P: Params + 'static, M: IcedPlugin<P>>(
    ctx: *mut c_void,
    x: f32,
    y: f32,
    delta_y: f32,
) {
    let editor = &mut *(ctx as *mut IcedEditor<P, M>);
    if let Some(ref mut runtime) = editor.runtime {
        runtime.queue_cursor_move(x, y);
        runtime
            .pending_events
            .push(Event::Mouse(iced::mouse::Event::WheelScrolled {
                delta: iced::mouse::ScrollDelta::Lines {
                    x: 0.0,
                    y: delta_y,
                },
            }));
    }
}

unsafe extern "C" fn cb_double_click<P: Params + 'static, M: IcedPlugin<P>>(
    ctx: *mut c_void,
    x: f32,
    y: f32,
) {
    let editor = &mut *(ctx as *mut IcedEditor<P, M>);
    if let Some(ref mut runtime) = editor.runtime {
        // Double-click = two rapid button presses. Iced's widgets handle
        // double-click detection internally, so just send press+release.
        runtime.queue_cursor_move(x, y);
        runtime
            .pending_events
            .push(Event::Mouse(iced::mouse::Event::ButtonPressed(
                iced::mouse::Button::Left,
            )));
        runtime
            .pending_events
            .push(Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )));
    }
}

unsafe extern "C" fn cb_mouse_moved<P: Params + 'static, M: IcedPlugin<P>>(
    ctx: *mut c_void,
    x: f32,
    y: f32,
) -> u8 {
    let editor = &mut *(ctx as *mut IcedEditor<P, M>);
    if let Some(ref mut runtime) = editor.runtime {
        if x < 0.0 || y < 0.0 {
            // Mouse exited
            runtime
                .pending_events
                .push(Event::Mouse(iced::mouse::Event::CursorLeft));
        } else {
            runtime.queue_cursor_move(x, y);
        }
        // Return 1 if cursor indicates a clickable widget
        if let Some(ref render) = runtime.render {
            let interaction = render.state.mouse_interaction();
            return match interaction {
                iced::mouse::Interaction::Pointer
                | iced::mouse::Interaction::Grab
                | iced::mouse::Interaction::Grabbing => 1,
                _ => 0,
            };
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Editor trait implementation
// ---------------------------------------------------------------------------

impl<P: Params + 'static, M: IcedPlugin<P>> Editor for IcedEditor<P, M> {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: truce_core::editor::RawWindowHandle, context: EditorContext) {
        let (w, h) = self.size;

        let parent_ptr = match parent {
            truce_core::editor::RawWindowHandle::AppKit(ptr) => ptr,
            #[allow(unused)]
            _ => std::ptr::null_mut(),
        };

        if parent_ptr.is_null() {
            return;
        }

        // Create the plugin model
        let plugin = if let Some(ref layout) = self.layout {
            let auto = AutoPlugin {
                layout: layout.clone(),
            };
            // SAFETY: AutoPlugin and M are the same type when layout is Some
            // (enforced by from_layout constructor which sets M = AutoPlugin).
            unsafe { std::ptr::read(&auto as *const AutoPlugin as *const M) }
        } else {
            M::new(self.params.clone())
        };

        let param_state = ParamState::new(self.params.clone());
        let editor_handle = EditorHandle::new(context);
        let program = IcedProgram {
            plugin,
            param_state,
            editor_handle,
            meter_ids: self.meter_ids.clone(),
        };

        // Set runtime BEFORE creating the platform view, because
        // cb_setup fires synchronously during view creation and needs
        // to store the Metal layer pointer on the runtime.
        self.runtime = Some(IcedRuntime {
            render: None,
            _view: None,
            metal_layer: None,
            cursor_position: Point::ORIGIN,
            mouse_left_pressed: false,
            pending_events: Vec::new(),
            program: Some(program),
            size: (w, h),
            scale_factor: self.scale_factor.max(1.0),
        });

        let self_ptr = self as *mut IcedEditor<P, M> as *mut c_void;

        let callbacks = IcedViewCallbacks {
            setup: Some(cb_setup::<P, M>),
            render: Some(cb_render::<P, M>),
            mouse_down: Some(cb_mouse_down::<P, M>),
            mouse_dragged: Some(cb_mouse_dragged::<P, M>),
            mouse_up: Some(cb_mouse_up::<P, M>),
            scroll: Some(cb_scroll::<P, M>),
            double_click: Some(cb_double_click::<P, M>),
            mouse_moved: Some(cb_mouse_moved::<P, M>),
        };

        let view = unsafe { IcedPlatformView::new(parent_ptr, w, h, self_ptr, &callbacks) };

        if let Some(view) = view {
            // Store the view handle (RAII — keeps NSView alive)
            if let Some(ref mut runtime) = self.runtime {
                runtime._view = Some(view);
            }
            eprintln!("[truce-iced] Editor opened ({w}x{h})");
        } else {
            // View creation failed — clean up the runtime
            self.runtime = None;
        }
    }

    fn close(&mut self) {
        self.runtime = None;
        eprintln!("[truce-iced] Editor closed");
    }

    fn idle(&mut self) {
        // Timer-driven rendering in cb_render handles everything.
    }

    fn can_resize(&self) -> bool {
        true
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        self.size = (width, height);
        if let Some(ref mut runtime) = self.runtime {
            runtime.size = (width, height);
            if let Some(ref mut render) = runtime.render {
                let scale = self.scale_factor;
                render.viewport = iced_graphics::Viewport::with_physical_size(
                    Size::new(width, height), scale,
                );
                render.surface_config.width = width;
                render.surface_config.height = height;
                render.surface.configure(&render.device, &render.surface_config);
            }
        }
        true
    }

    fn set_scale_factor(&mut self, factor: f64) {
        self.scale_factor = factor;
        if let Some(ref mut runtime) = self.runtime {
            runtime.scale_factor = factor.max(1.0);
            // Reconfigure viewport and surface if rendering is active
            if let Some(ref mut render) = runtime.render {
                let (w, h) = runtime.size;
                render.viewport =
                    iced_graphics::Viewport::with_physical_size(Size::new(w, h), factor);
                render.surface.configure(&render.device, &render.surface_config);
            }
        }
    }
}
