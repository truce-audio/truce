//! GPU editor — wraps `BuiltinEditor` rendering with wgpu + baseview.
//!
//! Creates a baseview child window with a wgpu surface. Each frame,
//! delegates widget rendering to `BuiltinEditor::render_to()` through
//! the GPU backend, then presents.

use std::sync::{Arc, Mutex};

use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions, WindowScalePolicy};

use truce_core::editor::{Editor, EditorBridge, PluginContext, RawWindowHandle};
use truce_gui::EditorScale;
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
    /// Live content-scale factor, shared with the baseview handler via
    /// [`truce_gui::EditorScale`]. `set_scale_factor` (host) writes
    /// here; the handler reads it each frame and updates the
    /// `WgpuBackend` scale + reconfigures the surface when the value
    /// diverges from `last_applied_scale`. Single source of truth
    /// shared with egui / iced / slint backends.
    scale: EditorScale,
    window: Option<baseview::WindowHandle>,
}

// SAFETY: `baseview::WindowHandle` holds a raw native window pointer
// (HWND / NSView / X11 Window) and is therefore not auto-`Send`. Hosts
// call `Editor::open` / `idle` / `close` from a single dedicated GUI
// thread, never concurrently and never from the audio thread, so the
// handle is only ever touched on the thread that created it. The
// `Editor` trait requires `Send` so the editor can live behind a
// trait object — this impl asserts that the *type* doesn't escape its
// thread in practice. All other fields (`Arc<Mutex<...>>`, `(u32,
// u32)`) are already `Send`.
unsafe impl<P: Params> Send for GpuEditor<P> {}

impl<P: Params + 'static> GpuEditor<P> {
    #[must_use] 
    pub fn new(inner: BuiltinEditor<P>) -> Self {
        let size = inner.size();
        Self {
            inner: Arc::new(Mutex::new(inner)),
            size,
            scale: EditorScale::new(truce_gui::backing_scale()),
            window: None,
        }
    }

    /// Create from a pre-existing shared reference.
    /// Used by `HotEditor` to share the inner `BuiltinEditor` so it can
    /// swap the layout on hot-reload while GPU rendering continues.
    ///
    /// # Panics
    ///
    /// Panics if the inner mutex is poisoned (a previous holder
    /// panicked). In normal operation `BuiltinEditor` never panics
    /// while holding the lock.
    pub fn new_shared(inner: Arc<Mutex<BuiltinEditor<P>>>) -> Self {
        let size = inner.lock().unwrap().size();
        Self {
            inner,
            size,
            scale: EditorScale::new(truce_gui::backing_scale()),
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
    /// Bridge handle, retained so we can drive `request_resize` from
    /// the render loop when hot-reload changes the editor's size.
    bridge: Arc<dyn EditorBridge>,
    /// Shared with the parent `GpuEditor`; written by `set_scale_factor`
    /// (host). `on_frame` compares against `last_applied_scale` and
    /// reconfigures the wgpu surface + MSAA target via
    /// `WgpuBackend::set_scale` + `resize` when they diverge.
    scale: EditorScale,
    last_applied_scale: f32,
}

impl<P: Params + 'static> WindowHandler for GpuWindowHandler<P> {
    fn on_frame(&mut self, _window: &mut Window) {
        if let Some(ref mut gpu) = self.gpu {
            // Pick up host-driven scale changes (CLAP `set_scale`, VST3
            // `IPlugViewContentScaleSupport`) that landed in the shared
            // cell since the last frame. There is no Resized fallback
            // here because the GPU path disallows host-driven resize
            // (see on_event), so the shared cell is the only writer.
            if let Some(cur_scale) = self.scale.take_change(&mut self.last_applied_scale) {
                gpu.set_scale(cur_scale);
                gpu.resize(self.current_size.0, self.current_size.1);
            }

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
                    self.bridge.request_resize(new_size.0, new_size.1);
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
                if let baseview::WindowEvent::Resized(info) = win {
                    // Resize is intentionally disallowed: `Editor::can_resize`
                    // and `Editor::set_size` use the trait defaults
                    // (`false` / `false`), so hosts shouldn't drive a
                    // resize. We pass the OS-reported scale through
                    // `note_linux_scale_factor` to keep the cross-
                    // backend Linux DPI cache populated, but
                    // deliberately do not reconfigure the wgpu surface
                    // or the inner `BuiltinEditor` — a user who drags
                    // the host window across a DPI boundary accepts the
                    // stretched/cropped output. Matches the
                    // `truce-gui::BuiltinEditor` CPU path so the two
                    // paths behave identically.
                    truce_gui::platform::note_linux_scale_factor(info.scale());
                }
                EventStatus::Ignored
            }
            Event::Keyboard(_) => EventStatus::Ignored,
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
        self.inner.lock().map_or(self.size, |g| g.size())
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        // Refresh the shared scale from the parent window — on macOS
        // this is the live `[NSWindow backingScaleFactor]`, on Windows
        // the per-monitor DPI from the parent HWND. Any
        // `set_scale_factor` the host issues *after* open will
        // overwrite this through the same shared cell.
        self.scale
            .set(crate::platform::query_backing_scale(&parent));
        let system_scale = self.scale.get();
        let (lw, lh) = self.size; // logical points

        hot_debug!("[truce-gpu] open() called, size={}x{}", lw, lh);

        let bridge = Arc::clone(context.bridge());

        // Set up the inner editor's context for param access
        if let Ok(mut inner) = self.inner.lock() {
            inner.set_context(context);
            hot_debug!("[truce-gpu] context set on inner editor");
        } else {
            hot_debug!("[truce-gpu] ERROR: failed to lock inner for set_context");
        }

        let inner = Arc::clone(&self.inner);
        let size = self.size;
        let scale_handle = self.scale.clone();

        let options = WindowOpenOptions {
            title: String::from("truce-gpu"),
            size: baseview::Size::new(f64::from(lw), f64::from(lh)),
            scale: WindowScalePolicy::SystemScaleFactor,
        };

        let parent_wrapper = ParentWindow(parent);

        let window = baseview::Window::open_parented(
            &parent_wrapper,
            options,
            move |window: &mut Window| {
                // Display scale never exceeds 4.0 in practice.
                #[allow(clippy::cast_possible_truncation)]
                let scale = system_scale as f32;
                let gpu = unsafe { WgpuBackend::from_window(window, size.0, size.1, scale) };

                GpuWindowHandler {
                    inner,
                    gpu,
                    translator: truce_gui::interaction::BaseviewTranslator::new(),
                    current_size: size,
                    bridge,
                    scale: scale_handle,
                    last_applied_scale: scale,
                }
            },
        );

        self.window = Some(window);
    }

    fn set_scale_factor(&mut self, factor: f64) {
        // Write to the shared cell; the baseview handler picks up the
        // change on its next frame and reconfigures the wgpu surface +
        // MSAA target via `WgpuBackend::set_scale` + `resize`. Replaces
        // the default no-op (host scale was previously dropped on the
        // floor for the GPU path).
        self.scale.set(factor);
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
        // Headless render of the inner BuiltinEditor at the live
        // content scale. Drives the same code path as production
        // (`render_to` → wgpu RenderBackend), just with a
        // `WgpuBackend::headless` target instead of a window-bound
        // one. Used by `truce_test::assert_screenshot::<P>()`.
        //
        // The inner BuiltinEditor was already built against the
        // plugin's `Arc<P>` (which is defaults for a fresh plugin),
        // so the `params` arg is unused.
        //
        // `EditorScale` falls back to `backing_scale()` for pre-open
        // / headless calls — 2.0 on Retina, 1.0 elsewhere — so the
        // historical "fixed 2×" behavior is preserved on the macOS
        // hosts where reference PNGs were originally baked.
        let mut inner = self.inner.lock().ok()?;
        let (lw, lh) = inner.size();
        let scale = self.scale.get_f32();
        let mut backend = WgpuBackend::headless(lw, lh, scale)?;
        inner.render_to(&mut backend);
        let pixels = backend.read_pixels();
        // Round (rather than truncate) so non-integer DPI scales produce
        // the same physical resolution the WgpuBackend internally
        // computed when sizing the headless target.
        // Window dimensions stay below u32::MAX after scaling.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let (phys_w, phys_h) = (
            (lw as f32 * scale).round() as u32,
            (lh as f32 * scale).round() as u32,
        );
        Some((pixels, phys_w, phys_h))
    }
}
