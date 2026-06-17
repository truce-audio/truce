//! `EguiEditor`: implements `truce_core::Editor` using egui + baseview + wgpu.
//!
//! On `open()`, creates a baseview child window and a wgpu surface.
//! Each `on_frame()` tick, runs the egui frame, tessellates, and renders.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions, WindowScalePolicy};

use truce_core::editor::{Editor, PluginContext, RawWindowHandle};
use truce_params::Params;

use crate::platform::ParentWindow;
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

struct EguiWindowHandler<P: Params + ?Sized> {
    ui: Arc<Mutex<Box<dyn EditorUi<P>>>>,
    context: PluginContext<P>,
    egui_ctx: egui::Context,
    renderer: Option<EguiRenderer>,
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
}

impl<P: Params + ?Sized> EguiWindowHandler<P> {
    /// Apply a pending resize: `NSView` frame (baseview's
    /// `Window::resize`) first, then the wgpu surface. Reverse
    /// order would leave the surface oversized vs. the layer that
    /// hosts it for a frame and Metal could draw against an
    /// undersized drawable.
    fn apply_resize(
        window: &mut Window,
        renderer: Option<&mut EguiRenderer>,
        new_size: (u32, u32),
        scale: f64,
    ) {
        window.resize(baseview::Size::new(
            f64::from(new_size.0),
            f64::from(new_size.1),
        ));
        if let Some(renderer) = renderer {
            let phys_w = truce_gui::to_physical_px(new_size.0, scale);
            let phys_h = truce_gui::to_physical_px(new_size.1, scale);
            renderer.resize(phys_w, phys_h);
        }
    }

    // `(u32, u32)` editor sizes widen to `f32` for egui's screen rect.
    // Editor sizes are bounded by display dimensions, well below 2^23.
    #[allow(clippy::cast_precision_loss)]
    fn run_frame(&mut self) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };

        // Pick up host-driven scale changes (CLAP `set_scale`, VST3
        // `IPlugViewContentScaleSupport`) that arrived via the editor's
        // `set_scale_factor` since the last frame. The `Resized` path
        // already applies its own scale changes inline, so this only
        // fires when scale moved without a corresponding window event.
        if let Some(cur_scale) = self.scale.take_change(&mut self.last_applied_scale) {
            let phys_w = truce_gui::to_physical_px(self.size.0, f64::from(cur_scale));
            let phys_h = truce_gui::to_physical_px(self.size.1, f64::from(cur_scale));
            renderer.resize(phys_w, phys_h);
        }

        let ppp = self.last_applied_scale;
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

        let ui_arc = &self.ui;
        let context = &self.context;
        let output = self.egui_ctx.run_ui(raw_input, |ui| {
            if let Ok(mut ui_fn) = ui_arc.lock() {
                ui_fn.ui(ui, context);
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
    fn on_frame(&mut self, window: &mut Window) {
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
            // Skip the draw on a resize frame so AppKit's deferred
            // relayout (scheduled by `view.setNeedsDisplay` inside
            // `Window::resize`) settles before we paint, and
            // bracket the macOS-side work in an autoreleasepool so
            // AppKit autoreleased objects from `setFrameSize` drain
            // before the next call rather than accumulating into
            // the host's main-thread pool.
            let new_size = pending;
            let scale = self.scale.get();
            #[cfg(target_os = "macos")]
            {
                let renderer = self.renderer.as_mut();
                objc::rc::autoreleasepool(|| {
                    Self::apply_resize(window, renderer, new_size, scale);
                });
            }
            #[cfg(not(target_os = "macos"))]
            Self::apply_resize(window, self.renderer.as_mut(), new_size, scale);
            self.size = new_size;
            return;
        }
        self.run_frame();
    }

    // `_window` is unused on macOS / Linux - only the Windows
    // ButtonPressed branch reads it (to SetFocus on the child HWND so
    // text widgets see WM_KEYDOWN). Underscore-prefix keeps that signal
    // intact; the allow lets the Windows branch use the binding without
    // renaming.
    #[allow(clippy::too_many_lines, clippy::used_underscore_binding)]
    fn on_event(&mut self, _window: &mut Window, event: Event) -> EventStatus {
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
                    let logical_size = (
                        (pw as f32 / scale).round() as u32,
                        (ph as f32 / scale).round() as u32,
                    );
                    self.size = logical_size;
                    // Write through to the shared scale so the editor's
                    // next `set_scale_factor` and any sibling reader
                    // see the OS-reported value, and update
                    // last_applied so run_frame's diff-check stays a
                    // no-op (we already resized inline below).
                    self.scale.set(info.scale());
                    self.last_applied_scale = scale;
                    if let Some(renderer) = self.renderer.as_mut() {
                        renderer.resize(pw, ph);
                    }
                }
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
        self.scale
            .set(crate::platform::query_backing_scale(&parent));
        let system_scale = self.scale.get();
        let (lw, lh) = self.size; // logical points

        // --- baseview + wgpu ---
        let ui = Arc::clone(&self.ui);
        if let Ok(mut ui_fn) = ui.lock() {
            ui_fn.opened(&typed_ctx);
        }
        let size = self.size;

        let options = WindowOpenOptions {
            title: String::from("truce-egui"),
            size: baseview::Size::new(f64::from(lw), f64::from(lh)),
            scale: WindowScalePolicy::SystemScaleFactor,
        };

        let parent_wrapper = ParentWindow(parent);
        let handler_ctx = typed_ctx.clone();
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

        let window = baseview::Window::open_parented(
            &parent_wrapper,
            options,
            move |window: &mut Window| {
                // Display scale never exceeds 4.0 in practice.
                #[allow(clippy::cast_possible_truncation)]
                let scale = system_scale as f32;
                let phys_w = truce_gui::to_physical_px(size.0, system_scale);
                let phys_h = truce_gui::to_physical_px(size.1, system_scale);
                let renderer = unsafe { EguiRenderer::from_window(window, phys_w, phys_h) };

                if let Some(font_data) = font {
                    crate::font::apply_font(&egui_ctx, font_data);
                }

                // Continuous repainting is driven by baseview's
                // `on_frame` calling `run_frame` every vblank, not by
                // egui's own scheduler. `egui_ctx.request_repaint()`
                // schedules a single frame, which baseview would
                // immediately paint anyway - the call had no effect.

                EguiWindowHandler::<P> {
                    ui,
                    context: handler_ctx,
                    egui_ctx,
                    renderer,
                    pending_events: Vec::new(),
                    modifiers: egui::Modifiers::NONE,
                    start_time: std::time::Instant::now(),
                    size,
                    pending_size,
                    scale: scale_handle,
                    last_applied_scale: scale,
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
        self.scale.set(factor);
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
                if let Ok(mut ui) = ui.lock() {
                    ui.ui(root_ui, state);
                }
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
