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
        Self {
            inner,
            size,
            window: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Baseview WindowHandler
// ---------------------------------------------------------------------------

struct GpuWindowHandler<P: Params> {
    inner: Arc<Mutex<BuiltinEditor<P>>>,
    gpu: Option<WgpuBackend>,
    /// Canonical baseview → `InputEvent` translator. Handles cursor
    /// tracking, double-click synthesis, and line→pixel scroll
    /// conversion once for everyone.
    translator: truce_gui::interaction::BaseviewTranslator,
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
                    static WARNED: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        eprintln!("[truce-gpu] WARNING: on_frame called but inner has no context");
                    }
                }

                // Check if the inner editor's size changed (e.g. after hot reload).
                let new_size = inner.size();
                if new_size != self.current_size {
                    hot_debug!(
                        "[truce-gpu] size changed: {}x{} -> {}x{}",
                        self.current_size.0,
                        self.current_size.1,
                        new_size.0,
                        new_size.1,
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
            Event::Mouse(_) => {
                let Some(input) = self.translator.translate(&event) else {
                    return EventStatus::Ignored;
                };
                if let Ok(mut inner) = self.inner.lock() {
                    inner.dispatch_events(&[input]);
                }
                EventStatus::Captured
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
                let gpu = unsafe { WgpuBackend::from_window(window, size.0, size.1, scale) };

                if gpu.is_some() {
                    eprintln!("[truce-gpu] GPU backend active (wgpu/baseview, scale={scale})");
                } else {
                    eprintln!(
                        "[truce-gpu] GPU init failed — plugin window will be blank. \
                              Build with --no-default-features to use CPU rendering."
                    );
                }

                GpuWindowHandler {
                    inner,
                    gpu,
                    translator: truce_gui::interaction::BaseviewTranslator::new(),
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
        // baseview drives its own frame loop via on_frame().
    }

    fn state_changed(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.state_changed();
        }
    }

    fn screenshot(
        &mut self,
        _params: Arc<dyn truce_params::Params>,
    ) -> Option<(Vec<u8>, u32, u32)> {
        // Headless render of the inner BuiltinEditor at 2× scale.
        // Drives the same code path as production (`render_to` →
        // wgpu RenderBackend), just with a `WgpuBackend::headless`
        // target instead of a window-bound one. Used by
        // `truce_test::assert_screenshot::<P>()`.
        //
        // The inner BuiltinEditor was already built against the
        // plugin's `Arc<P>` (which is defaults for a fresh plugin),
        // so the `params` arg is unused.
        let mut inner = self.inner.lock().ok()?;
        let (lw, lh) = inner.size();
        let scale = 2.0_f32;
        let mut backend = WgpuBackend::headless(lw, lh, scale)?;
        inner.render_to(&mut backend);
        let pixels = backend.read_pixels();
        let phys_w = (lw as f32 * scale) as u32;
        let phys_h = (lh as f32 * scale) as u32;
        Some((pixels, phys_w, phys_h))
    }
}
