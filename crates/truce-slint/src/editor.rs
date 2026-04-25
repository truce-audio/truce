//! SlintEditor: implements truce_core::Editor using Slint + baseview + wgpu.
//!
//! On `open()`, creates a baseview child window with a wgpu surface.
//! Each frame, renders the Slint UI to a pixel buffer via SoftwareRenderer,
//! uploads it to a wgpu texture, and blits to the surface.
//!
//! Runs the same code path on every macOS host, AAX included — the
//! old setContents-on-NSView AAX workaround was stripped 2026-04-23
//! once patched baseview made the standard wgpu path viable inside
//! Pro Tools' DFW container.

use std::iter;
use std::rc::Rc;
use std::sync::Arc;

use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions, WindowScalePolicy};
use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor};
use slint::platform::{PointerEventButton, WindowAdapter, WindowEvent};
use slint::{LogicalPosition, PhysicalSize};

use truce_core::editor::{Editor, EditorContext, RawWindowHandle};

use crate::blit::BlitPipeline;
use crate::param_state::ParamState;
use crate::platform::{self, ParentWindow};

/// Slint-based editor implementing truce's `Editor` trait.
///
/// The developer provides a setup closure that:
/// 1. Creates the Slint component
/// 2. Wires Slint callbacks to `ParamState` for UI→host parameter changes
/// 3. Returns a per-frame sync closure for host→UI parameter updates
///
/// # Example
///
/// ```ignore
/// SlintEditor::new((400, 300), |state: ParamState| {
///     let ui = MyPluginUi::new().unwrap();
///     let s = state.clone();
///     ui.on_gain_changed(move |v| s.set_immediate(0, v as f64));
///     Box::new(move |state: &ParamState| {
///         ui.set_gain(state.get(0) as f32);
///     })
/// })
/// ```
/// Per-frame sync closure: takes the current `ParamState` and updates the
/// Slint component's properties. Returned by the editor's `setup` callback.
pub type SyncFn = Box<dyn Fn(&ParamState)>;

/// Editor `setup` callback: called every time the host re-opens the editor,
/// creates the Slint component, and returns a `SyncFn` that the editor calls
/// each frame to push live param values into the component.
pub type SetupFn = Arc<dyn Fn(ParamState) -> SyncFn + Send + Sync>;

pub struct SlintEditor {
    size: (u32, u32),
    /// Called on each open() to create the Slint component and param bindings.
    /// Must be `Fn` (not `FnOnce`) because the host may close and re-open
    /// the editor window multiple times.
    setup: SetupFn,
    window: Option<baseview::WindowHandle>,
}

unsafe impl Send for SlintEditor {}

impl SlintEditor {
    /// Create a Slint editor.
    ///
    /// `size` is the window size in logical points. `setup` is called
    /// on the UI thread each time the editor opens. It must create a
    /// fresh Slint component and return a per-frame sync closure.
    pub fn new(
        size: (u32, u32),
        setup: impl Fn(ParamState) -> Box<dyn Fn(&ParamState)> + Send + Sync + 'static,
    ) -> Self {
        Self {
            size,
            setup: Arc::new(setup),
            window: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Baseview WindowHandler
// ---------------------------------------------------------------------------

struct SlintWindowHandler {
    slint_window: Rc<MinimalSoftwareWindow>,
    sync_fn: Box<dyn Fn(&ParamState)>,
    state: ParamState,
    blit: Option<BlitPipeline>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    px_buf: Vec<PremultipliedRgbaColor>,
    rgba_buf: Vec<u8>,
    width: u32,
    height: u32,
    scale: f32,
    /// Last known cursor position in logical points.
    last_pos: LogicalPosition,
}

impl WindowHandler for SlintWindowHandler {
    fn on_frame(&mut self, _window: &mut Window) {
        // 1. Drive Slint timers/animations
        slint::platform::update_timers_and_animations();

        // 2. Sync host params → Slint properties
        (self.sync_fn)(&self.state);

        // 3. Force redraw — params/meters change externally every frame
        self.slint_window.request_redraw();

        // 4. Render Slint to pixel buffer
        let phys_w = (self.width as f32 * self.scale) as u32;
        let phys_h = (self.height as f32 * self.scale) as u32;
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

        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => return,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        blit.render(&mut encoder, &view);
        self.queue.submit(iter::once(encoder.finish()));
        frame.present();
    }

    fn on_event(&mut self, _window: &mut Window, event: Event) -> EventStatus {
        match event {
            Event::Mouse(mouse) => {
                use baseview::MouseEvent::*;
                match mouse {
                    CursorMoved { position, .. } => {
                        self.last_pos = LogicalPosition::new(position.x as f32, position.y as f32);
                        self.slint_window
                            .window()
                            .dispatch_event(WindowEvent::PointerMoved {
                                position: self.last_pos,
                            });
                        EventStatus::Captured
                    }
                    ButtonPressed {
                        button: baseview::MouseButton::Left,
                        ..
                    } => {
                        // Child plugin window needs focus to receive WM_KEYDOWN
                        // on Windows. See truce-egui editor.rs for rationale.
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
                                button: PointerEventButton::Left,
                            });
                        EventStatus::Captured
                    }
                    ButtonReleased {
                        button: baseview::MouseButton::Left,
                        ..
                    } => {
                        self.slint_window
                            .window()
                            .dispatch_event(WindowEvent::PointerReleased {
                                position: self.last_pos,
                                button: PointerEventButton::Left,
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
                    self.width = (phys_w as f64 / scale) as u32;
                    self.height = (phys_h as f64 / scale) as u32;
                    self.scale = scale as f32;

                    self.slint_window
                        .window()
                        .dispatch_event(WindowEvent::ScaleFactorChanged {
                            scale_factor: scale as f32,
                        });
                    self.slint_window
                        .set_size(slint::WindowSize::Physical(PhysicalSize::new(
                            phys_w, phys_h,
                        )));

                    self.surface_config.width = phys_w;
                    self.surface_config.height = phys_h;
                    self.surface.configure(&self.device, &self.surface_config);

                    if let Some(ref mut blit) = self.blit {
                        blit.resize(&self.device, phys_w, phys_h);
                    }
                }
                EventStatus::Ignored
            }
            _ => EventStatus::Ignored,
        }
    }
}

// ---------------------------------------------------------------------------
// Editor trait
// ---------------------------------------------------------------------------

impl Editor for SlintEditor {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: RawWindowHandle, context: EditorContext) {
        platform::ensure_platform();

        let (lw, lh) = self.size;
        let scale = platform::query_backing_scale(&parent);
        let state = ParamState::new(context);
        let setup = Arc::clone(&self.setup);

        // --- baseview + wgpu ---
        let options = WindowOpenOptions {
            title: String::from("truce-slint"),
            size: baseview::Size::new(lw as f64, lh as f64),
            scale: WindowScalePolicy::SystemScaleFactor,
        };

        let parent_wrapper = ParentWindow(parent);

        let window = baseview::Window::open_parented(
            &parent_wrapper,
            options,
            move |window: &mut Window| {
                // baseview spawns this closure on a new thread; Slint's
                // set_platform is per-thread, so re-register here.
                platform::ensure_platform();

                // Create wgpu surface
                let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
                    backends: wgpu::Backends::PRIMARY,
                    ..Default::default()
                });

                let surface = unsafe { platform::create_wgpu_surface(&instance, window) }
                    .expect("failed to create wgpu surface");

                let adapter =
                    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                        power_preference: wgpu::PowerPreference::LowPower,
                        compatible_surface: Some(&surface),
                        force_fallback_adapter: false,
                    }))
                    .expect("no suitable GPU adapter");

                let (device, queue) = pollster::block_on(adapter.request_device(
                    &wgpu::DeviceDescriptor {
                        label: Some("truce-slint"),
                        required_features: wgpu::Features::empty(),
                        required_limits: wgpu::Limits::downlevel_defaults(),
                        memory_hints: wgpu::MemoryHints::Performance,
                    },
                    None,
                ))
                .expect("failed to create wgpu device");

                let caps = surface.get_capabilities(&adapter);
                let format = caps
                    .formats
                    .iter()
                    .find(|f| f.is_srgb())
                    .copied()
                    .unwrap_or(caps.formats[0]);

                let phys_w = (lw as f64 * scale) as u32;
                let phys_h = (lh as f64 * scale) as u32;

                let surface_config = wgpu::SurfaceConfiguration {
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    format,
                    width: phys_w,
                    height: phys_h,
                    present_mode: wgpu::PresentMode::AutoVsync,
                    desired_maximum_frame_latency: 2,
                    alpha_mode: wgpu::CompositeAlphaMode::Auto,
                    view_formats: vec![],
                };
                surface.configure(&device, &surface_config);

                // Create the MinimalSoftwareWindow and register it in the
                // thread-local so the next Slint component creation (inside
                // the setup closure) attaches to it.
                let slint_window = platform::create_slint_window();
                slint_window.set_size(slint::WindowSize::Physical(PhysicalSize::new(
                    phys_w, phys_h,
                )));
                slint_window
                    .window()
                    .dispatch_event(WindowEvent::ScaleFactorChanged {
                        scale_factor: scale as f32,
                    });

                // Developer creates the Slint component here — it attaches
                // to slint_window via create_window_adapter().
                let sync_fn = setup(state.clone());

                SlintWindowHandler {
                    slint_window,
                    sync_fn,
                    state,
                    blit: None,
                    device,
                    queue,
                    surface,
                    surface_config,
                    px_buf: Vec::new(),
                    rgba_buf: Vec::new(),
                    width: lw,
                    height: lh,
                    scale: scale as f32,
                    last_pos: LogicalPosition::default(),
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
        // baseview drives its own frame loop.
    }

    fn screenshot(
        &mut self,
        params: Arc<dyn truce_params::Params>,
    ) -> Option<(Vec<u8>, u32, u32)> {
        let state = ParamState::from_params(params);
        let setup = Arc::clone(&self.setup);
        Some(crate::screenshot::render_with_state(
            &state,
            self.size,
            2.0,
            move |s| setup(s.clone()),
        ))
    }
}
