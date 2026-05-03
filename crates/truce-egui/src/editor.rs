//! EguiEditor: implements truce_core::Editor using egui + baseview + wgpu.
//!
//! On `open()`, creates a baseview child window and a wgpu surface.
//! Each `on_frame()` tick, runs the egui frame, tessellates, and renders.

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions, WindowScalePolicy};

use truce_core::editor::{Editor, EditorContext, RawWindowHandle};
use truce_params::Params;

use crate::platform::{ParentWindow, query_backing_scale};
use crate::renderer::EguiRenderer;

/// Trait for stateful egui UI implementations.
///
/// Implement this for complex UIs that need internal state. For simple
/// closure-based UIs, use `EguiEditor::new()` instead.
pub trait EditorUi<P: Params + ?Sized>: Send {
    fn ui(&mut self, ctx: &egui::Context, state: &EditorContext<P>);

    /// Called once when the editor window opens. Use to create StateBindings.
    fn opened(&mut self, _state: &EditorContext<P>) {}

    /// Plugin state was restored (preset recall, undo, session load).
    /// Re-read any cached custom state. Parameter values update automatically.
    fn state_changed(&mut self, _state: &EditorContext<P>) {}
}

impl<P: Params + ?Sized, F: FnMut(&egui::Context, &EditorContext<P>) + Send> EditorUi<P> for F {
    fn ui(&mut self, ctx: &egui::Context, state: &EditorContext<P>) {
        self(ctx, state);
    }
}

/// No-op placeholder for `mem::replace` during builder chain.
struct NopUi<P: ?Sized>(PhantomData<fn(&P)>);
impl<P: Params + ?Sized> EditorUi<P> for NopUi<P> {
    fn ui(&mut self, _ctx: &egui::Context, _state: &EditorContext<P>) {}
}

/// Type alias to keep the `WithStateChanged` field signature within
/// clippy's complexity budget without losing the `Send` bound.
type StateChangedFn<P> = Box<dyn FnMut(&EditorContext<P>) + Send>;

/// Wraps an EditorUi with an additional state_changed callback.
struct WithStateChanged<P: Params + ?Sized> {
    inner: Box<dyn EditorUi<P>>,
    on_changed: StateChangedFn<P>,
}

impl<P: Params + ?Sized> EditorUi<P> for WithStateChanged<P> {
    fn ui(&mut self, ctx: &egui::Context, state: &EditorContext<P>) {
        self.inner.ui(ctx, state);
    }

    fn opened(&mut self, state: &EditorContext<P>) {
        self.inner.opened(state);
    }

    fn state_changed(&mut self, state: &EditorContext<P>) {
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
/// `state.gain.smoothed_next()`, `state.bypass.value()`. Stores its own
/// `Arc<P>` from construction; rebuilds the typed `EditorContext<P>`
/// every time the host opens the window via [`EditorContext::with_params`].
pub struct EguiEditor<P: Params + ?Sized> {
    params: Arc<P>,
    size: (u32, u32),
    /// Shared with the baseview WindowHandler so it survives open/close cycles.
    ui: Arc<Mutex<Box<dyn EditorUi<P>>>>,
    visuals: Option<egui::Visuals>,
    font: Option<&'static [u8]>,
    scale_factor: Option<f64>,
    /// Active baseview window handle — exists only while editor is open.
    window: Option<baseview::WindowHandle>,
    /// Typed editor context stored at open() for state_changed forwarding.
    context: Option<EditorContext<P>>,
}

// SAFETY: `baseview::WindowHandle` holds a raw native window pointer
// (HWND / NSView / X11 Window) and is not auto-`Send`. Hosts call
// `Editor::open` / `idle` / `close` from a single dedicated GUI thread
// — never concurrently and never from the audio thread — so the
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
        ui_fn: impl FnMut(&egui::Context, &EditorContext<P>) + Send + 'static,
    ) -> Self {
        Self {
            params,
            size,
            ui: Arc::new(Mutex::new(Box::new(ui_fn))),
            visuals: None,
            font: None,
            scale_factor: None,
            window: None,
            context: None,
        }
    }

    /// Create an egui editor with a trait-based UI (for stateful UIs).
    pub fn with_ui(params: Arc<P>, size: (u32, u32), ui: impl EditorUi<P> + 'static) -> Self {
        Self {
            params,
            size,
            ui: Arc::new(Mutex::new(Box::new(ui))),
            visuals: None,
            font: None,
            scale_factor: None,
            window: None,
            context: None,
        }
    }

    /// Add a callback for when plugin state is restored (preset recall, undo).
    ///
    /// Only needed with the closure API (`EguiEditor::new`). For the struct
    /// API (`EguiEditor::with_ui`), implement `EditorUi::state_changed` instead.
    ///
    /// ```ignore
    /// EguiEditor::new(params, (400, 300), |ctx, state| { /* ui */ })
    ///     .on_state_changed(|state| { /* re-read cached state */ })
    /// ```
    pub fn on_state_changed(mut self, f: impl FnMut(&EditorContext<P>) + Send + 'static) -> Self {
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
    pub fn with_visuals(mut self, visuals: egui::Visuals) -> Self {
        self.visuals = Some(visuals);
        self
    }

    /// Set a custom default font (TrueType data).
    ///
    /// ```ignore
    /// EguiEditor::new(params, (400, 300), my_ui)
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

struct EguiWindowHandler<P: Params + ?Sized> {
    ui: Arc<Mutex<Box<dyn EditorUi<P>>>>,
    context: EditorContext<P>,
    egui_ctx: egui::Context,
    renderer: Option<EguiRenderer>,
    pending_events: Vec<egui::Event>,
    modifiers: egui::Modifiers,
    start_time: std::time::Instant,
    size: (u32, u32),
    scale_factor: f32,
    last_cursor_pos: egui::Pos2,
}

impl<P: Params + ?Sized> EguiWindowHandler<P> {
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
        let context = &self.context;
        let output = self.egui_ctx.run(raw_input, |ctx| {
            if let Ok(mut ui_fn) = ui.lock() {
                ui_fn.ui(ctx, context);
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

impl<P: Params + ?Sized + 'static> WindowHandler for EguiWindowHandler<P> {
    fn on_frame(&mut self, _window: &mut Window) {
        self.run_frame();
    }

    fn on_event(&mut self, _window: &mut Window, event: Event) -> EventStatus {
        match event {
            Event::Mouse(mouse) => {
                use baseview::MouseEvent::*;
                match mouse {
                    CursorMoved {
                        position,
                        modifiers,
                    } => {
                        self.modifiers = convert_kb_modifiers(&modifiers);
                        let pos = egui::pos2(position.x as f32, position.y as f32);
                        self.last_cursor_pos = pos;
                        self.pending_events.push(egui::Event::PointerMoved(pos));
                        EventStatus::Captured
                    }
                    ButtonPressed { button, modifiers } => {
                        // On Windows, a WS_CHILD plugin window doesn't receive
                        // WM_KEYDOWN/WM_CHAR until it has HWND focus. baseview
                        // doesn't SetFocus on mouse-down, so we do it here —
                        // otherwise text-edit widgets never see keystrokes
                        // (the DAW keeps eating them for transport etc.).
                        #[cfg(target_os = "windows")]
                        {
                            if !_window.has_focus() {
                                _window.focus();
                            }
                        }
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

                // Text input. Suppress Text events when Ctrl/Cmd is
                // held — otherwise Ctrl+A/Ctrl+C/etc. would also insert
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
                    let scale = info.scale() as f32;
                    truce_gui::platform::note_linux_scale_factor(info.scale());
                    // Store logical size — egui screen_rect uses logical
                    // points. Round so a physical 800px@2× reports as 400
                    // logical, not 399 (truncating cast).
                    self.size = (
                        (pw as f32 / scale).round() as u32,
                        (ph as f32 / scale).round() as u32,
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

fn convert_kb_modifiers(mods: &keyboard_types::Modifiers) -> egui::Modifiers {
    let alt = mods.contains(keyboard_types::Modifiers::ALT);
    let ctrl = mods.contains(keyboard_types::Modifiers::CONTROL);
    let shift = mods.contains(keyboard_types::Modifiers::SHIFT);
    let meta = mods.contains(keyboard_types::Modifiers::META);
    egui::Modifiers {
        alt,
        ctrl,
        shift,
        // `mac_cmd` is Mac-specific, fed by Cmd (META on macOS).
        mac_cmd: cfg!(target_os = "macos") && meta,
        // `command` is the cross-platform "primary modifier": Cmd on
        // macOS, Ctrl elsewhere. Mapping META→command on Linux/Windows
        // (the previous behavior) made egui treat the Super key as the
        // shortcut modifier, breaking Ctrl+C/V/X/Z in plugin editors.
        command: if cfg!(target_os = "macos") {
            meta
        } else {
            ctrl
        },
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

impl<P: Params + 'static> Editor for EguiEditor<P> {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: RawWindowHandle, context: EditorContext) {
        // Re-type the dyn-erased context to `EditorContext<P>` using
        // the Arc<P> we stored at construction.
        let typed_ctx = context.with_params(self.params.clone());
        self.context = Some(typed_ctx.clone());
        let egui_ctx = egui::Context::default();
        let visuals = self.visuals.clone().unwrap_or_else(crate::theme::dark);
        egui_ctx.set_visuals(visuals.clone());
        let font = self.font;

        let system_scale = query_backing_scale(&parent);
        let (lw, lh) = self.size; // logical points

        // --- baseview + wgpu ---
        let ui = Arc::clone(&self.ui);
        if let Ok(mut ui_fn) = ui.lock() {
            ui_fn.opened(&typed_ctx);
        }
        let size = self.size;

        let options = WindowOpenOptions {
            title: String::from("truce-egui"),
            size: baseview::Size::new(lw as f64, lh as f64),
            scale: WindowScalePolicy::SystemScaleFactor,
        };

        let parent_wrapper = ParentWindow(parent);
        let handler_ctx = typed_ctx.clone();

        let window = baseview::Window::open_parented(
            &parent_wrapper,
            options,
            move |window: &mut Window| {
                let scale = system_scale as f32;
                let phys_w = (size.0 as f32 * scale) as u32;
                let phys_h = (size.1 as f32 * scale) as u32;
                let renderer = unsafe { EguiRenderer::from_window(window, phys_w, phys_h) };

                if let Some(font_data) = font {
                    crate::font::apply_font(&egui_ctx, font_data);
                }

                // Request continuous repainting (plugin GUIs need it for meters)
                egui_ctx.request_repaint();

                EguiWindowHandler::<P> {
                    ui,
                    context: handler_ctx,
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
        if let Some(mut window) = self.window.take() {
            window.close();
        }
    }

    fn idle(&mut self) {
        // baseview drives its own frame loop via on_frame().
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
        if let Some(ref ctx) = self.context
            && let Ok(mut ui) = self.ui.lock()
        {
            ui.state_changed(ctx);
        }
    }

    fn screenshot(
        &mut self,
        _params: Arc<dyn truce_params::Params>,
    ) -> Option<(Vec<u8>, u32, u32)> {
        let context = truce_core::editor::for_test_params(self.params.clone() as Arc<dyn Params>)
            .with_params(self.params.clone());
        // Pin to 2.0 so screenshots are reproducible across hosts and CI
        // regardless of any prior `set_scale_factor` from a live window.
        let pixels_per_point = 2.0_f32;
        let ui = Arc::clone(&self.ui);
        crate::screenshot::render_with_state::<P>(
            &context,
            self.size,
            pixels_per_point,
            self.font,
            self.visuals.clone(),
            move |ctx, state| {
                if let Ok(mut ui) = ui.lock() {
                    ui.ui(ctx, state);
                }
            },
        )
    }
}
