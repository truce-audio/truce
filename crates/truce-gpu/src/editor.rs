//! GPU editor — wraps `BuiltinEditor` rendering with wgpu + baseview.
//!
//! Creates a baseview child window with a wgpu surface. Each frame,
//! delegates widget rendering to `BuiltinEditor::render_to()` through
//! the GPU backend, then presents.

use std::sync::{Arc, Mutex};

macro_rules! hot_debug {
    ($($arg:tt)*) => {
        #[cfg(feature = "hot-debug")]
        eprintln!($($arg)*);
    };
}

use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions, WindowScalePolicy};

use truce_core::editor::{Editor, EditorContext, RawWindowHandle};
use truce_gui::editor::BuiltinEditor;
use truce_gui::render::RenderBackend;
use truce_params::Params;

use crate::backend::WgpuBackend;
use crate::platform::ParentWindow;

/// GPU-accelerated editor.
///
/// On `open()`, creates a baseview child window with a wgpu surface.
/// Falls back to the inner `BuiltinEditor` (CPU path) if GPU init fails.
pub struct GpuEditor<P: Params> {
    inner: Arc<Mutex<BuiltinEditor<P>>>,
    size: (u32, u32),
    window: Option<baseview::WindowHandle>,
}

unsafe impl<P: Params> Send for GpuEditor<P> {}

impl<P: Params + 'static> GpuEditor<P> {
    pub fn new(inner: BuiltinEditor<P>) -> Self {
        let size = inner.size();
        Self {
            inner: Arc::new(Mutex::new(inner)),
            size,
            window: None,
        }
    }

    /// Create from a pre-existing shared reference.
    /// Used by `HotEditor` to share the inner `BuiltinEditor` so it can
    /// swap the layout on hot-reload while GPU rendering continues.
    pub fn new_shared(inner: Arc<Mutex<BuiltinEditor<P>>>) -> Self {
        let size = inner.lock().unwrap().size();
        Self { inner, size, window: None }
    }
}

// ---------------------------------------------------------------------------
// Baseview WindowHandler
// ---------------------------------------------------------------------------

struct GpuWindowHandler<P: Params> {
    inner: Arc<Mutex<BuiltinEditor<P>>>,
    gpu: Option<WgpuBackend>,
    /// Last cursor position in logical points.
    last_pos: (f32, f32),
    left_pressed: bool,
    last_click_time: std::time::Instant,
    last_click_pos: (f32, f32),
    /// Current logical size — used to detect hot-reload size changes.
    current_size: (u32, u32),
    /// Host callback to request a window resize.
    request_resize: Arc<dyn Fn(u32, u32) -> bool + Send + Sync>,
}

impl<P: Params + 'static> WindowHandler for GpuWindowHandler<P> {
    fn on_frame(&mut self, _window: &mut Window) {
        if let Some(ref mut gpu) = self.gpu {
            if let Ok(mut inner) = self.inner.lock() {
                #[cfg(feature = "hot-debug")]
                if !inner.has_context() {
                    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        eprintln!("[truce-gpu] WARNING: on_frame called but inner has no context");
                    }
                }

                // Check if the inner editor's size changed (e.g. after hot reload).
                let new_size = inner.size();
                if new_size != self.current_size {
                    hot_debug!(
                        "[truce-gpu] size changed: {}x{} -> {}x{}",
                        self.current_size.0, self.current_size.1,
                        new_size.0, new_size.1,
                    );
                    gpu.resize(new_size.0, new_size.1);
                    (self.request_resize)(new_size.0, new_size.1);
                    self.current_size = new_size;
                }

                inner.render_to(gpu);
            }
            gpu.present();
        }
    }

    fn on_event(&mut self, _window: &mut Window, event: Event) -> EventStatus {
        match event {
            Event::Mouse(mouse) => {
                use baseview::MouseEvent::*;
                let mut inner = match self.inner.lock() {
                    Ok(inner) => inner,
                    Err(_) => return EventStatus::Ignored,
                };

                match mouse {
                    CursorMoved { position, .. } => {
                        let pos = (position.x as f32, position.y as f32);
                        self.last_pos = pos;
                        if self.left_pressed {
                            inner.on_mouse_dragged(pos.0, pos.1);
                        } else {
                            inner.on_mouse_moved(pos.0, pos.1);
                        }
                        EventStatus::Captured
                    }
                    ButtonPressed { button: baseview::MouseButton::Left, .. } => {
                        let (px, py) = self.last_pos;
                        let now = std::time::Instant::now();

                        // Double-click detection
                        let dt = now.duration_since(self.last_click_time).as_millis();
                        let dx = (px - self.last_click_pos.0).abs();
                        let dy = (py - self.last_click_pos.1).abs();
                        if dt < 400 && dx < 5.0 && dy < 5.0 {
                            inner.on_double_click(px, py);
                        } else {
                            inner.on_mouse_down(px, py);
                        }

                        self.last_click_time = now;
                        self.last_click_pos = (px, py);
                        self.left_pressed = true;
                        EventStatus::Captured
                    }
                    ButtonReleased { button: baseview::MouseButton::Left, .. } => {
                        let (px, py) = self.last_pos;
                        self.left_pressed = false;
                        inner.on_mouse_up(px, py);
                        EventStatus::Captured
                    }
                    WheelScrolled { delta, .. } => {
                        let (px, py) = self.last_pos;
                        let dy = match delta {
                            baseview::ScrollDelta::Lines { y, .. } => y * 20.0,
                            baseview::ScrollDelta::Pixels { y, .. } => y,
                        };
                        inner.on_scroll(px, py, dy);
                        EventStatus::Captured
                    }
                    CursorLeft => {
                        inner.on_mouse_moved(-1.0, -1.0);
                        EventStatus::Captured
                    }
                    _ => EventStatus::Ignored,
                }
            }
            Event::Window(win) => {
                if let baseview::WindowEvent::Resized(_info) = win {
                    // TODO: resize wgpu surface + BuiltinEditor
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

impl<P: Params + 'static> Editor for GpuEditor<P> {
    fn size(&self) -> (u32, u32) {
        // Read live size from the inner editor so hot-reload changes
        // are reflected when the host queries our size.
        self.inner.lock().map(|g| g.size()).unwrap_or(self.size)
    }

    fn open(&mut self, parent: RawWindowHandle, context: EditorContext) {
        let system_scale = truce_gui::backing_scale();
        let (lw, lh) = self.size; // logical points

        hot_debug!("[truce-gpu] open() called, size={}x{}", lw, lh);

        let request_resize = Arc::clone(&context.request_resize);

        // Set up the inner editor's context for param access
        if let Ok(mut inner) = self.inner.lock() {
            inner.set_context(context);
            hot_debug!("[truce-gpu] context set on inner editor");
        } else {
            hot_debug!("[truce-gpu] ERROR: failed to lock inner for set_context");
        }

        let inner = Arc::clone(&self.inner);
        let size = self.size;

        let options = WindowOpenOptions {
            title: String::from("truce-gpu"),
            size: baseview::Size::new(lw as f64, lh as f64),
            scale: WindowScalePolicy::SystemScaleFactor,
        };

        let parent_wrapper = ParentWindow(parent);

        let window = baseview::Window::open_parented(
            &parent_wrapper,
            options,
            move |window: &mut Window| {
                let scale = system_scale as f32;
                let gpu = unsafe {
                    WgpuBackend::from_window(window, size.0, size.1, scale)
                };

                if gpu.is_some() {
                    eprintln!("[truce-gpu] GPU backend active (wgpu/baseview, scale={scale})");
                } else {
                    eprintln!("[truce-gpu] GPU init failed — plugin window will be blank. \
                              Build with --no-default-features to use CPU rendering.");
                }

                GpuWindowHandler {
                    inner,
                    gpu,
                    last_pos: (0.0, 0.0),
                    left_pressed: false,
                    last_click_time: std::time::Instant::now()
                        - std::time::Duration::from_secs(10),
                    last_click_pos: (-100.0, -100.0),
                    current_size: size,
                    request_resize,
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
}
