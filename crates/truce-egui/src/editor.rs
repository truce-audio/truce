//! EguiEditor: implements truce_core::Editor using egui + baseview + wgpu.
//!
//! On `open()`, creates a baseview child window and a wgpu surface.
//! Each `on_frame()` tick, runs the egui frame, tessellates, and renders.

use std::sync::{Arc, Mutex};

use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions, WindowScalePolicy};

use truce_core::editor::{Editor, EditorContext, RawWindowHandle};

use crate::param_state::ParamState;
use crate::platform::{ParentWindow, query_backing_scale};
use crate::renderer::EguiRenderer;

/// Trait for stateful egui UI implementations.
///
/// Implement this for complex UIs that need internal state. For simple
/// closure-based UIs, use `EguiEditor::new()` instead.
pub trait EditorUi: Send {
    fn ui(&mut self, ctx: &egui::Context, state: &ParamState);

    /// Plugin state was restored (preset recall, undo, session load).
    /// Re-read any cached custom state. Parameter values update automatically.
    fn state_changed(&mut self, _state: &ParamState) {}
}

impl<F: FnMut(&egui::Context, &ParamState) + Send> EditorUi for F {
    fn ui(&mut self, ctx: &egui::Context, state: &ParamState) {
        self(ctx, state);
    }
}

/// egui-based editor implementing truce's `Editor` trait.
///
/// Owns the egui context, wgpu renderer, and baseview window. On each
/// `on_frame()` tick, runs the egui frame, executes the user's UI function,
/// tessellates, and presents via egui-wgpu.
pub struct EguiEditor {
    size: (u32, u32),
    /// Shared with the baseview WindowHandler so it survives open/close cycles.
    ui: Arc<Mutex<Box<dyn EditorUi>>>,
    visuals: Option<egui::Visuals>,
    font: Option<&'static [u8]>,
    scale_factor: Option<f64>,
    /// Active baseview window handle — exists only while editor is open.
    window: Option<baseview::WindowHandle>,
    /// True when the native NSView + wgpu AAX path is active.
    uses_aax_native: bool,
    #[cfg(target_os = "macos")]
    aax_state: Option<EguiAaxState>,
    /// EditorContext stored at open() for state_changed forwarding.
    context: Option<truce_core::editor::EditorContext>,
}

// WindowHandle contains raw pointers; only accessed from host UI thread.
unsafe impl Send for EguiEditor {}

impl EguiEditor {
    /// Create an egui editor with a closure-based UI.
    ///
    /// `size` is the initial window size in pixels (physical).
    pub fn new(
        size: (u32, u32),
        ui_fn: impl FnMut(&egui::Context, &ParamState) + Send + 'static,
    ) -> Self {
        Self {
            size,
            ui: Arc::new(Mutex::new(Box::new(ui_fn))),
            visuals: None,
            font: None,
            scale_factor: None,
            window: None,
            uses_aax_native: false,
            #[cfg(target_os = "macos")]
            aax_state: None,
            context: None,
        }
    }

    /// Create an egui editor with a trait-based UI (for stateful UIs).
    pub fn with_ui(size: (u32, u32), ui: impl EditorUi + 'static) -> Self {
        Self {
            size,
            ui: Arc::new(Mutex::new(Box::new(ui))),
            visuals: None,
            font: None,
            scale_factor: None,
            window: None,
            uses_aax_native: false,
            #[cfg(target_os = "macos")]
            aax_state: None,
            context: None,
        }
    }

    /// Set custom visuals (theme). Use `truce_egui::theme::dark()` for
    /// the default dark theme matching truce-gui.
    pub fn with_visuals(mut self, visuals: egui::Visuals) -> Self {
        self.visuals = Some(visuals);
        self
    }

    /// Set a custom default font (TrueType data).
    ///
    /// ```ignore
    /// EguiEditor::new((400, 300), my_ui)
    ///     .with_font(truce_gui::font::JETBRAINS_MONO)
    /// ```
    pub fn with_font(mut self, font_data: &'static [u8]) -> Self {
        self.font = Some(font_data);
        self
    }
}

// ---------------------------------------------------------------------------
// Baseview WindowHandler — owns the egui frame loop + wgpu renderer
// ---------------------------------------------------------------------------

struct EguiWindowHandler {
    ui: Arc<Mutex<Box<dyn EditorUi>>>,
    param_state: ParamState,
    egui_ctx: egui::Context,
    renderer: Option<EguiRenderer>,
    pending_events: Vec<egui::Event>,
    modifiers: egui::Modifiers,
    start_time: std::time::Instant,
    size: (u32, u32),
    scale_factor: f32,
    last_cursor_pos: egui::Pos2,
}

impl EguiWindowHandler {
    fn run_frame(&mut self) {
        let renderer = match self.renderer.as_mut() {
            Some(r) => r,
            None => return,
        };

        let ppp = self.scale_factor;
        let (lw, lh) = self.size; // logical points

        let mut raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(lw as f32, lh as f32),
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

        let ui = &self.ui;
        let param_state = &self.param_state;
        let output = self.egui_ctx.run(raw_input, |ctx| {
            if let Ok(mut ui_fn) = ui.lock() {
                ui_fn.ui(ctx, param_state);
            }
        });

        let clipped_primitives = self
            .egui_ctx
            .tessellate(output.shapes, output.pixels_per_point);

        renderer.render(
            &output.textures_delta,
            &clipped_primitives,
            output.pixels_per_point,
        );
    }
}

impl WindowHandler for EguiWindowHandler {
    fn on_frame(&mut self, _window: &mut Window) {
        self.run_frame();
    }

    fn on_event(&mut self, _window: &mut Window, event: Event) -> EventStatus {
        match event {
            Event::Mouse(mouse) => {
                use baseview::MouseEvent::*;
                match mouse {
                    CursorMoved { position, modifiers } => {
                        self.modifiers = convert_kb_modifiers(&modifiers);
                        let pos = egui::pos2(position.x as f32, position.y as f32);
                        self.last_cursor_pos = pos;
                        self.pending_events
                            .push(egui::Event::PointerMoved(pos));
                        EventStatus::Captured
                    }
                    ButtonPressed { button, modifiers } => {
                        self.modifiers = convert_kb_modifiers(&modifiers);
                        if let Some(btn) = convert_mouse_button(&button) {
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
                        self.modifiers = convert_kb_modifiers(&modifiers);
                        if let Some(btn) = convert_mouse_button(&button) {
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
                        self.modifiers = convert_kb_modifiers(&modifiers);
                        let (dx, dy) = match delta {
                            baseview::ScrollDelta::Lines { x, y } => (x * 20.0, y * 20.0),
                            baseview::ScrollDelta::Pixels { x, y } => (x, y),
                        };
                        self.pending_events.push(egui::Event::MouseWheel {
                            unit: egui::MouseWheelUnit::Point,
                            delta: egui::vec2(dx, dy),
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
                self.modifiers = convert_kb_modifiers(&kb.modifiers);

                // Text input
                if kb.state == KeyState::Down {
                    if let keyboard_types::Key::Character(ref ch) = kb.key {
                        for c in ch.chars() {
                            if !c.is_control() {
                                self.pending_events
                                    .push(egui::Event::Text(c.to_string()));
                            }
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
                    let scale = info.scale() as f32;
                    // Store logical size — egui screen_rect uses logical points
                    self.size = (
                        (pw as f32 / scale) as u32,
                        (ph as f32 / scale) as u32,
                    );
                    self.scale_factor = scale;
                    if let Some(renderer) = self.renderer.as_mut() {
                        renderer.resize(pw, ph);
                    }
                }
                EventStatus::Ignored
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Event conversion helpers
// ---------------------------------------------------------------------------

fn convert_mouse_button(btn: &baseview::MouseButton) -> Option<egui::PointerButton> {
    match btn {
        baseview::MouseButton::Left => Some(egui::PointerButton::Primary),
        baseview::MouseButton::Right => Some(egui::PointerButton::Secondary),
        baseview::MouseButton::Middle => Some(egui::PointerButton::Middle),
        _ => None,
    }
}

fn convert_kb_modifiers(mods: &keyboard_types::Modifiers) -> egui::Modifiers {
    egui::Modifiers {
        alt: mods.contains(keyboard_types::Modifiers::ALT),
        ctrl: mods.contains(keyboard_types::Modifiers::CONTROL),
        shift: mods.contains(keyboard_types::Modifiers::SHIFT),
        mac_cmd: mods.contains(keyboard_types::Modifiers::META),
        command: mods.contains(keyboard_types::Modifiers::META),
    }
}

fn convert_key(key: &keyboard_types::Key) -> Option<egui::Key> {
    use keyboard_types::Key::*;
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

// ---------------------------------------------------------------------------
// AAX native view path (macOS only)
// ---------------------------------------------------------------------------

/// Shared input state between the native view callbacks and idle().
/// Box'd separately so its address is stable across moves of EguiAaxState.
#[cfg(target_os = "macos")]
struct EguiAaxInput {
    pending_events: Vec<egui::Event>,
    last_cursor_pos: egui::Pos2,
}

#[cfg(target_os = "macos")]
struct EguiAaxState {
    native_view: truce_gui::native_view::NativeView,
    renderer: EguiRenderer,
    egui_ctx: egui::Context,
    param_state: ParamState,
    input: Box<EguiAaxInput>,
    start_time: std::time::Instant,
    scale_factor: f32,
    display_link: *mut std::ffi::c_void,
}

// SAFETY: Only accessed from the GUI thread.
#[cfg(target_os = "macos")]
unsafe impl Send for EguiAaxState {}

// ---------------------------------------------------------------------------
// CVDisplayLink for high-frequency egui rendering in AAX
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVDisplayLinkCreateWithActiveCGDisplays(link_out: *mut *mut std::ffi::c_void) -> i32;
    fn CVDisplayLinkSetOutputCallback(
        link: *mut std::ffi::c_void,
        callback: extern "C" fn(
            *mut std::ffi::c_void, *const std::ffi::c_void, *const std::ffi::c_void,
            u64, *mut u64, *mut std::ffi::c_void,
        ) -> i32,
        user_info: *mut std::ffi::c_void,
    ) -> i32;
    fn CVDisplayLinkStart(link: *mut std::ffi::c_void) -> i32;
    fn CVDisplayLinkStop(link: *mut std::ffi::c_void) -> i32;
    fn CVDisplayLinkRelease(link: *mut std::ffi::c_void);
}

#[cfg(target_os = "macos")]
extern "C" {
    static _dispatch_main_q: std::ffi::c_void;
    fn dispatch_async_f(
        queue: *const std::ffi::c_void,
        context: *mut std::ffi::c_void,
        work: extern "C" fn(*mut std::ffi::c_void),
    );
}

/// CVDisplayLink callback — fires on background thread at VBlank rate.
/// Dispatches a render to the main thread via GCD.
#[cfg(target_os = "macos")]
extern "C" fn egui_display_link_callback(
    _link: *mut std::ffi::c_void,
    _now: *const std::ffi::c_void,
    _output: *const std::ffi::c_void,
    _flags: u64,
    _flags_out: *mut u64,
    ctx: *mut std::ffi::c_void,
) -> i32 {
    unsafe {
        dispatch_async_f(
            &_dispatch_main_q as *const std::ffi::c_void,
            ctx,
            egui_render_on_main,
        );
    }
    0 // kCVReturnSuccess
}

/// Dispatched to main thread — renders one egui frame.
#[cfg(target_os = "macos")]
extern "C" fn egui_render_on_main(ctx: *mut std::ffi::c_void) {
    if ctx.is_null() {
        return;
    }
    unsafe {
        extern "C" {
            fn objc_autoreleasePoolPush() -> *mut std::ffi::c_void;
            fn objc_autoreleasePoolPop(pool: *mut std::ffi::c_void);
        }

        let editor = &mut *(ctx as *mut EguiEditor);

        // Guard: only render if AAX state is still alive
        let (aax_state, ui, size) = match editor.aax_state.as_mut() {
            Some(state) => (state, &editor.ui, editor.size),
            None => return,
        };

        let pool = objc_autoreleasePoolPush();

        let (lw, lh) = size;
        let ppp = aax_state.scale_factor;

        let mut raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(lw as f32, lh as f32),
            )),
            time: Some(aax_state.start_time.elapsed().as_secs_f64()),
            modifiers: egui::Modifiers::NONE,
            events: std::mem::take(&mut aax_state.input.pending_events),
            focused: true,
            ..Default::default()
        };
        raw_input
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(ppp);

        let output = aax_state.egui_ctx.run(raw_input, |ctx| {
            if let Ok(mut ui_fn) = ui.lock() {
                ui_fn.ui(ctx, &aax_state.param_state);
            }
        });

        let clipped_primitives = aax_state
            .egui_ctx
            .tessellate(output.shapes, output.pixels_per_point);

        aax_state.renderer.render(
            &output.textures_delta,
            &clipped_primitives,
            output.pixels_per_point,
        );

        objc_autoreleasePoolPop(pool);
    }
}

#[cfg(target_os = "macos")]
struct EguiAaxCallbackCtx {
    input: *mut EguiAaxInput,
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn egui_aax_mouse_moved(ctx: *mut std::ffi::c_void, x: f32, y: f32) {
    let ctx = &*(ctx as *mut EguiAaxCallbackCtx);
    let input = &mut *ctx.input;
    let pos = egui::pos2(x, y);
    input.last_cursor_pos = pos;
    input.pending_events.push(egui::Event::PointerMoved(pos));
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn egui_aax_mouse_dragged(ctx: *mut std::ffi::c_void, x: f32, y: f32) {
    egui_aax_mouse_moved(ctx, x, y);
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn egui_aax_mouse_down(ctx: *mut std::ffi::c_void, x: f32, y: f32) {
    let ctx = &*(ctx as *mut EguiAaxCallbackCtx);
    let input = &mut *ctx.input;
    let pos = egui::pos2(x, y);
    input.last_cursor_pos = pos;
    input.pending_events.push(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn egui_aax_mouse_up(ctx: *mut std::ffi::c_void, x: f32, y: f32) {
    let ctx = &*(ctx as *mut EguiAaxCallbackCtx);
    let input = &mut *ctx.input;
    let pos = egui::pos2(x, y);
    input.last_cursor_pos = pos;
    input.pending_events.push(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn egui_aax_scroll(
    ctx: *mut std::ffi::c_void, _x: f32, _y: f32, dy: f32,
) {
    let ctx = &*(ctx as *mut EguiAaxCallbackCtx);
    let input = &mut *ctx.input;
    input.pending_events.push(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, dy),
        modifiers: egui::Modifiers::NONE,
    });
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn egui_aax_mouse_exited(ctx: *mut std::ffi::c_void) {
    let ctx = &*(ctx as *mut EguiAaxCallbackCtx);
    let input = &mut *ctx.input;
    input.pending_events.push(egui::Event::PointerGone);
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn egui_aax_drop_ctx(ctx: *mut std::ffi::c_void) {
    let _ = Box::from_raw(ctx as *mut EguiAaxCallbackCtx);
}

/// Create an NSView with a CAMetalLayer and return (NativeView, metal_layer_ptr).
#[cfg(target_os = "macos")]
unsafe fn create_native_view_with_metal(
    parent_ptr: *mut std::ffi::c_void,
    width: f64,
    height: f64,
    callbacks: truce_gui::native_view::NativeViewCallbacks,
) -> (truce_gui::native_view::NativeView, *mut std::ffi::c_void) {
    use cocoa::base::id;
    use objc::{class, msg_send, sel, sel_impl};

    let native_view = truce_gui::native_view::open(parent_ptr, width, height, callbacks);
    let ns_view = native_view.ns_view_ptr() as id;

    // Create a CAMetalLayer and assign it to the view
    let metal_layer: id = msg_send![class!(CAMetalLayer), layer];
    let device: id = {
        extern "C" { fn MTLCreateSystemDefaultDevice() -> id; }
        MTLCreateSystemDefaultDevice()
    };
    let _: () = msg_send![metal_layer, setDevice: device];
    let _: () = msg_send![metal_layer, setPixelFormat: 80u64]; // MTLPixelFormatBGRA8Unorm
    let _: () = msg_send![metal_layer, setFramebufferOnly: true];

    // Get the backing scale factor
    let window: id = msg_send![ns_view, window];
    let scale: f64 = if !window.is_null() {
        msg_send![window, backingScaleFactor]
    } else {
        let screen: id = msg_send![class!(NSScreen), mainScreen];
        msg_send![screen, backingScaleFactor]
    };
    let _: () = msg_send![metal_layer, setContentsScale: scale];

    // Layer-hosting mode: set the custom layer FIRST, then enable wantsLayer.
    // This is the opposite of layer-backed mode (CgBlit).
    let _: () = msg_send![ns_view, setLayer: metal_layer];
    let _: () = msg_send![ns_view, setWantsLayer: cocoa::base::YES];

    let layer_ptr = metal_layer as *mut std::ffi::c_void;
    (native_view, layer_ptr)
}

// ---------------------------------------------------------------------------
// Editor trait implementation
// ---------------------------------------------------------------------------

impl Editor for EguiEditor {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: RawWindowHandle, context: EditorContext) {
        self.context = Some(context.clone());
        let egui_ctx = egui::Context::default();
        let visuals = self.visuals.clone().unwrap_or_else(crate::theme::dark);
        egui_ctx.set_visuals(visuals.clone());
        let font = self.font;

        let system_scale = query_backing_scale(&parent);
        let (lw, lh) = self.size; // logical points

        // --- AAX path: native NSView + Metal layer + wgpu (no baseview) ---
        #[cfg(target_os = "macos")]
        if truce_gui::editor::should_use_cg_blit() {
            self.uses_aax_native = true;

            let parent_ptr = match parent {
                RawWindowHandle::AppKit(ptr) => ptr,
                _ => std::ptr::null_mut(),
            };
            if parent_ptr.is_null() {
                return;
            }

            let scale = system_scale as f32;
            let phys_w = (lw as f32 * scale) as u32;
            let phys_h = (lh as f32 * scale) as u32;

            let param_state = ParamState::new(context);

            // Box the input separately so its address is stable for callbacks
            let mut input = Box::new(EguiAaxInput {
                pending_events: Vec::new(),
                last_cursor_pos: egui::Pos2::ZERO,
            });

            let cb_ctx = Box::new(EguiAaxCallbackCtx {
                input: &mut *input as *mut EguiAaxInput,
            });
            let cb_ctx_ptr = Box::into_raw(cb_ctx) as *mut std::ffi::c_void;

            let callbacks = truce_gui::native_view::NativeViewCallbacks {
                ctx: cb_ctx_ptr,
                on_mouse_moved: egui_aax_mouse_moved,
                on_mouse_dragged: egui_aax_mouse_dragged,
                on_mouse_down: egui_aax_mouse_down,
                on_mouse_up: egui_aax_mouse_up,
                on_scroll: egui_aax_scroll,
                on_mouse_exited: egui_aax_mouse_exited,
                drop_ctx: egui_aax_drop_ctx,
            };

            let (native_view, metal_layer) = unsafe {
                create_native_view_with_metal(
                    parent_ptr,
                    lw as f64,
                    lh as f64,
                    callbacks,
                )
            };

            let renderer = unsafe {
                EguiRenderer::from_metal_layer(metal_layer, phys_w, phys_h)
            };

            if let Some(renderer) = renderer {
                if let Some(font_data) = font {
                    crate::font::apply_font(&egui_ctx, font_data);
                }
                egui_ctx.request_repaint();

                // Start CVDisplayLink for 60Hz rendering
                let mut display_link: *mut std::ffi::c_void = std::ptr::null_mut();
                unsafe { CVDisplayLinkCreateWithActiveCGDisplays(&mut display_link) };

                self.aax_state = Some(EguiAaxState {
                    native_view,
                    renderer,
                    egui_ctx,
                    param_state,
                    input,
                    start_time: std::time::Instant::now(),
                    scale_factor: scale,
                    display_link,
                });

                if !display_link.is_null() {
                    unsafe {
                        let self_ptr = self as *mut EguiEditor as *mut std::ffi::c_void;
                        CVDisplayLinkSetOutputCallback(
                            display_link,
                            egui_display_link_callback,
                            self_ptr,
                        );
                        CVDisplayLinkStart(display_link);
                    }
                }
            }
            return;
        }

        // --- Normal path: baseview + wgpu ---
        let ui = Arc::clone(&self.ui);
        let param_state = ParamState::new(context);
        let size = self.size;

        let options = WindowOpenOptions {
            title: String::from("truce-egui"),
            size: baseview::Size::new(lw as f64, lh as f64),
            scale: WindowScalePolicy::SystemScaleFactor,
        };

        let parent_wrapper = ParentWindow(parent);

        let window = baseview::Window::open_parented(
            &parent_wrapper,
            options,
            move |window: &mut Window| {
                let scale = system_scale as f32;
                let phys_w = (size.0 as f32 * scale) as u32;
                let phys_h = (size.1 as f32 * scale) as u32;
                let renderer =
                    unsafe { EguiRenderer::from_window(window, phys_w, phys_h) };

                if let Some(font_data) = font {
                    crate::font::apply_font(&egui_ctx, font_data);
                }

                // Request continuous repainting (plugin GUIs need it for meters)
                egui_ctx.request_repaint();

                EguiWindowHandler {
                    ui,
                    param_state,
                    egui_ctx,
                    renderer,
                    pending_events: Vec::new(),
                    modifiers: egui::Modifiers::NONE,
                    start_time: std::time::Instant::now(),
                    size,
                    scale_factor: scale,
                    last_cursor_pos: egui::Pos2::ZERO,
                }
            },
        );

        self.window = Some(window);
    }

    fn close(&mut self) {
        #[cfg(target_os = "macos")]
        if self.uses_aax_native {
            // Stop CVDisplayLink FIRST — synchronous, blocks until
            // any in-flight callback completes.
            if let Some(ref state) = self.aax_state {
                unsafe {
                    if !state.display_link.is_null() {
                        CVDisplayLinkStop(state.display_link);
                        CVDisplayLinkRelease(state.display_link);
                    }
                }
            }
            self.aax_state = None;
            self.uses_aax_native = false;
            return;
        }
        if let Some(mut window) = self.window.take() {
            window.close();
        }
    }

    fn idle(&mut self) {
        #[cfg(target_os = "macos")]
        if self.uses_aax_native {
            // CVDisplayLink drives rendering at VBlank rate (~60Hz).
            // idle() is still called by the host at ~30Hz — skip if
            // display link is active, fall back to idle-driven render
            // if display link creation failed.
            if let Some(ref state) = self.aax_state {
                if !state.display_link.is_null() {
                    return;
                }
            }
            egui_render_on_main(self as *mut EguiEditor as *mut std::ffi::c_void);
            return;
        }
        // Baseview drives its own frame loop via on_frame().
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        self.size = (width, height);
        true
    }

    fn can_resize(&self) -> bool {
        false
    }

    fn set_scale_factor(&mut self, factor: f64) {
        self.scale_factor = Some(factor);
    }

    fn state_changed(&mut self) {
        if let Some(ref ctx) = self.context {
            let ps = ParamState::new(ctx.clone());
            if let Ok(mut ui) = self.ui.lock() {
                ui.state_changed(&ps);
            }
        }
    }
}
