//! GPU editor - wraps `BuiltinEditor` rendering with wgpu + baseview.
//!
//! Creates a baseview child window with a wgpu surface. Each frame,
//! delegates widget rendering to `BuiltinEditor::render_to()` through
//! the wgpu backend, then presents. Lives in `truce-gui` so the
//! framework's user-facing renderer entry-point is a single crate;
//! the wgpu primitives (`WgpuBackend`) stay an implementation detail
//! in `truce-gpu`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use baseview::{Event, EventStatus, Window, WindowHandler, WindowOpenOptions, WindowScalePolicy};

use truce_core::editor::{Editor, PluginContext, RawWindowHandle};
use truce_gpu::WgpuBackend;
use truce_gui_types::render::RenderBackend;
use truce_params::Params;

use crate::EditorScale;
use crate::editor::BuiltinEditor;
use crate::platform::ParentWindow;

/// GPU-accelerated editor.
///
/// On `open()`, creates a baseview child window with a wgpu surface.
/// If wgpu adapter / surface acquisition fails, `from_window` returns
/// `None` and `on_frame` becomes a no-op for that session.
pub struct GpuEditor<P: Params> {
    inner: Arc<Mutex<BuiltinEditor<P>>>,
    size: (u32, u32),
    /// Live content-scale factor (a [`EditorScale`]).
    /// `set_scale_factor` (host) writes here; the baseview handler
    /// reads it each frame and updates the `WgpuBackend` scale +
    /// reconfigures the surface when the value diverges from
    /// `last_applied_scale`.
    scale: EditorScale,
    window: Option<baseview::WindowHandle>,
}

// SAFETY: `baseview::WindowHandle` holds a raw native window pointer
// (HWND / NSView / X11 Window) and is therefore not auto-`Send`. Hosts
// call `Editor::open` / `idle` / `close` from a single dedicated GUI
// thread, never concurrently and never from the audio thread, so the
// handle is only ever touched on the thread that created it. The
// `Editor` trait requires `Send` so the editor can live behind a
// trait object - this impl asserts that the *type* doesn't escape its
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
            scale: EditorScale::new(crate::backing_scale()),
            window: None,
        }
    }

    /// Create from a pre-existing shared reference. Reserved for
    /// future hot-reload paths that want to swap the inner
    /// `BuiltinEditor` while GPU rendering continues.
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
            scale: EditorScale::new(crate::backing_scale()),
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
    translator: crate::interaction::BaseviewTranslator,
    /// Current logical size - used to detect host/user-driven resizes
    /// so the GPU surface + child window follow.
    current_size: (u32, u32),
    /// Shared with the parent `GpuEditor`; written by `set_scale_factor`
    /// (host). `on_frame` compares against `last_applied_scale` and
    /// reconfigures the wgpu surface + MSAA target via
    /// `WgpuBackend::set_scale` + `resize` when they diverge.
    scale: EditorScale,
    last_applied_scale: f32,
}

impl<P: Params + 'static> WindowHandler for GpuWindowHandler<P> {
    fn on_frame(&mut self, window: &mut Window) {
        if let Some(ref mut gpu) = self.gpu {
            // Pick up scale changes that landed in the shared cell
            // since the last frame - either from a host callback (CLAP
            // `set_scale`, VST3 `IPlugViewContentScaleSupport`) or from
            // the OS-driven `Resized` path (see on_event). Logical w×h
            // is fixed (resize is disallowed per `Editor::can_resize`'s
            // `false` default); only the logical→physical ratio moves.
            if let Some(cur_scale) = self.scale.take_change(&mut self.last_applied_scale) {
                gpu.set_scale(cur_scale);
                gpu.resize(self.current_size.0, self.current_size.1);
            }

            if let Ok(mut inner) = self.inner.lock() {
                if !inner.has_context() {
                    static WARNED: AtomicBool = AtomicBool::new(false);
                    if !WARNED.swap(true, Ordering::Relaxed) {
                        log::warn!("[truce-gpu] on_frame called but inner has no context");
                    }
                }

                // Pick up a size change from the inner editor (a
                // host/user-driven `set_size`: the standalone's outer
                // `Resized` or this window's own `Resized` below). Resize
                // the GPU surface and this child window so the content
                // follows - mirrors `BuiltinWindowHandler`.
                //
                // Deliberately NO `bridge.request_resize` here: this runs
                // on baseview's render thread, and in a plugin host
                // echoing a resize request back while the host is mid-
                // resize creates a feedback loop (host resize -> set_size
                // -> here -> request_resize -> host resize -> ...) that
                // hangs the host. The host/WM already owns the outer
                // frame size during a drag; we only follow it.
                let new_size = inner.size();
                if new_size != self.current_size {
                    gpu.resize(new_size.0, new_size.1);
                    window.resize(baseview::Size::new(
                        f64::from(new_size.0),
                        f64::from(new_size.1),
                    ));
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
                    // The OS-reported *scale* is authoritative: on Windows
                    // the parent HWND queried at `open()` time can report a
                    // different DPI than the child surface baseview actually
                    // creates, and on every platform dragging across a
                    // monitor boundary needs to land on the new DPI. Write
                    // through to the shared cell so `on_frame`'s
                    // `take_change` path calls `set_scale` + `resize`.
                    self.scale.set(info.scale());
                    crate::platform::note_linux_scale_factor(info.scale());

                    // When the editor opts into resize, route the new
                    // logical bounds into the inner editor's `set_size` so
                    // the grid reflows. In a plugin host this `Resized` is
                    // the only resize signal (the host drives our child
                    // view directly); the standalone also calls `set_size`
                    // via the outer window, but this is harmless there -
                    // the size already matches and `set_size` is a no-op.
                    // `on_frame` then resizes the GPU surface + window.
                    if let Ok(mut inner) = self.inner.lock()
                        && inner.can_resize()
                    {
                        let phys = info.physical_size();
                        let scale = info.scale();
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let (lw, lh) = if scale > 0.0 {
                            (
                                (f64::from(phys.width) / scale).round() as u32,
                                (f64::from(phys.height) / scale).round() as u32,
                            )
                        } else {
                            (phys.width, phys.height)
                        };
                        if (lw, lh) != inner.size() && lw > 0 && lh > 0 {
                            inner.set_size(lw, lh);
                        }
                    }
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

    // Resize delegates to the inner `BuiltinEditor`, which owns the
    // grid reflow + cell-snap logic. Without these the GPU editor would
    // fall back to the trait defaults (`can_resize() == false`) and
    // hosts / the standalone would pin the window. The actual surface +
    // child-window resize happens in `GpuWindowHandler::on_frame` once
    // `set_size` changes the inner editor's reported size.
    fn can_resize(&self) -> bool {
        self.inner.lock().is_ok_and(|g| g.can_resize())
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        self.inner
            .lock()
            .is_ok_and(|mut g| g.set_size(width, height))
    }

    fn min_size(&self) -> (u32, u32) {
        self.inner.lock().map_or((1, 1), |g| g.min_size())
    }

    fn max_size(&self) -> (u32, u32) {
        self.inner
            .lock()
            .map_or((u32::MAX, u32::MAX), |g| g.max_size())
    }

    fn size_increment(&self) -> Option<(u32, u32)> {
        self.inner.lock().ok().and_then(|g| g.size_increment())
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        // Refresh the shared scale from the parent window - on macOS
        // this is the live `[NSWindow backingScaleFactor]`, on Windows
        // the per-monitor DPI from the parent HWND. Any
        // `set_scale_factor` the host issues *after* open will
        // overwrite this through the same shared cell.
        self.scale
            .set(crate::platform::query_backing_scale(&parent));
        let system_scale = self.scale.get();
        let (lw, lh) = self.size; // logical points

        // Set up the inner editor's context for param access.
        if let Ok(mut inner) = self.inner.lock() {
            inner.set_context(context);
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
                    translator: crate::interaction::BaseviewTranslator::default(),
                    current_size: size,
                    scale: scale_handle,
                    last_applied_scale: scale,
                }
            },
        );

        self.window = Some(window);
    }

    fn set_scale_factor(&mut self, factor: f64) {
        // Write to the shared cell; the baseview handler picks up the
        // change on its next frame and reconfigures the wgpu surface
        // + MSAA target via `WgpuBackend::set_scale` + `resize`. The
        // trait's default no-op would silently swallow host scale
        // changes for the GPU path.
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
        // `EditorScale` falls back to `backing_scale()` for pre-open
        // / headless calls - 2.0 on Retina, 1.0 elsewhere - so the
        // historical "fixed 2×" behaviour is preserved on the macOS
        // hosts where reference PNGs were originally baked.
        let mut inner = self.inner.lock().ok()?;
        let (lw, lh) = inner.size();
        let scale = self.scale.get_f32();
        let mut backend = WgpuBackend::headless(lw, lh, scale)?;
        inner.render_to(&mut backend);
        let pixels = backend.read_pixels();
        // Round (rather than truncate) so non-integer DPI scales
        // produce the same physical resolution the WgpuBackend
        // internally computed when sizing the headless target. Window
        // dimensions stay below u32::MAX after scaling.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let (phys_w, phys_h) = (
            (lw as f32 * scale).round() as u32,
            (lh as f32 * scale).round() as u32,
        );
        Some((pixels, phys_w, phys_h))
    }
}

impl<P: Params + 'static> Drop for GpuEditor<P> {
    fn drop(&mut self) {
        // Dropping the baseview `WindowHandle` does not cancel the macOS
        // frame timer, so if the host drops us without a prior
        // `Editor::close` the timer keeps firing `on_frame`. The handler
        // holds owned `Arc` clones rather than a raw editor pointer, so
        // this leaks / renders into a dead surface rather than crashing
        // outright - but it is the same defect the cpu path crashes on.
        // Tear the window down here too; idempotent via `Option::take`.
        Editor::close(self);
    }
}
