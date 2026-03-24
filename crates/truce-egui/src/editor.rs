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
    scale_factor: Option<f64>,
    /// Active baseview window handle — exists only while editor is open.
    window: Option<baseview::WindowHandle>,
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
            scale_factor: None,
            window: None,
        }
    }

    /// Create an egui editor with a trait-based UI (for stateful UIs).
    pub fn with_ui(size: (u32, u32), ui: impl EditorUi + 'static) -> Self {
        Self {
            size,
            ui: Arc::new(Mutex::new(Box::new(ui))),
            visuals: None,
            scale_factor: None,
            window: None,
        }
    }

    /// Set custom visuals (theme). Use `truce_egui::theme::dark()` for
    /// the default dark theme matching truce-gui.
    pub fn with_visuals(mut self, visuals: egui::Visuals) -> Self {
        self.visuals = Some(visuals);
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
                        self.pending_events
                            .push(egui::Event::PointerMoved(pos));
                        EventStatus::Captured
                    }
                    ButtonPressed { button, modifiers } => {
                        self.modifiers = convert_kb_modifiers(&modifiers);
                        if let Some(btn) = convert_mouse_button(&button) {
                            self.pending_events.push(egui::Event::PointerButton {
                                pos: egui::Pos2::ZERO, // egui uses last known pos
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
                                pos: egui::Pos2::ZERO,
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
// Editor trait implementation
// ---------------------------------------------------------------------------

impl Editor for EguiEditor {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: RawWindowHandle, context: EditorContext) {
        let egui_ctx = egui::Context::default();
        let visuals = self.visuals.clone().unwrap_or_else(crate::theme::dark);
        egui_ctx.set_visuals(visuals);

        let system_scale = query_backing_scale(&parent);
        let (lw, lh) = self.size; // logical points

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
}
