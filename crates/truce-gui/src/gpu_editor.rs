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

use truce_core::editor::{Editor, PluginContext, RawWindowHandle, fit_size};
use truce_gpu::WgpuBackend;
use truce_gui_types::render::RenderBackend;
use truce_params::Params;

use crate::EditorScale;
use crate::editor::BuiltinEditor;
use crate::platform::ParentWindow;

/// GPU-accelerated editor.
///
/// On `open()`, creates a baseview child window and a surface pump
/// (see `truce_gpu::pump`): GPU init runs there - off the host's GUI
/// thread on Windows, where a stalled driver used to freeze the DAW -
/// and the backend is adopted on a later frame. If init fails,
/// `on_frame` stays a no-op for that session (blank editor, live
/// host).
pub struct GpuEditor<P: Params> {
    inner: Arc<Mutex<BuiltinEditor<P>>>,
    size: (u32, u32),
    /// Live content-scale factor (a [`EditorScale`]).
    /// `set_scale_factor` (host) writes here; the baseview handler
    /// reads it each frame and updates the `WgpuBackend` scale +
    /// reconfigures the surface when the value diverges from
    /// `last_applied_scale`.
    scale: EditorScale,
    /// Standalone hosts set this (via `set_uses_system_scale`) so the
    /// editor honors the desktop `Xft.dpi` scale on Linux; plugins leave
    /// it false and drive scale from the host instead. See
    /// [`crate::platform::editor_window_scale`]. No effect off Linux.
    use_system_scale: bool,
    /// Whether the host announced a content scale via `set_scale_factor`.
    /// On Linux this gates whether an embedded editor trusts `scale`
    /// (host-announced) or defaults to 1.0.
    host_scale_set: bool,
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
            use_system_scale: false,
            host_scale_set: false,
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
            use_system_scale: false,
            host_scale_set: false,
            window: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Baseview WindowHandler
// ---------------------------------------------------------------------------

struct GpuWindowHandler<P: Params> {
    inner: Arc<Mutex<BuiltinEditor<P>>>,
    /// Owns the wgpu surface + every blocking swapchain call; the
    /// backend below is its init product, adopted in `on_frame`.
    pump: Option<truce_gpu::pump::SurfacePump<WgpuBackend>>,
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
    /// Whether the window's scale is host-driven (baseview
    /// `WindowScalePolicy::ScaleFactor`, i.e. an embedded plug-in) rather
    /// than OS-detected (`SystemScaleFactor`, i.e. the standalone). When
    /// true, the host's `set_scale_factor` is authoritative and baseview's
    /// echoed `info.scale()` must NOT overwrite it - instead the `Resized`
    /// handler pushes the host scale into baseview (via
    /// `Window::set_scale_factor`) when the two diverge, so a late
    /// `IPlugViewContentScaleSupport` report (REAPER on Linux) is applied
    /// without the editor and baseview fighting over the scale.
    host_driven_scale: bool,
    /// Paces paints to the compositor's measured consumption rate so
    /// the per-tick render/present can't park the host's GUI thread in
    /// the swapchain acquire - see [`crate::PaintPacer`].
    pacer: crate::platform::PaintPacer,
}

impl<P: Params + 'static> GpuWindowHandler<P> {
    fn on_frame_inner(&mut self, window: &mut Window) {
        // Adopt the pump's backend once GPU init lands (first frame on
        // macOS / Linux where init ran inline; whenever the pump
        // thread finishes on Windows - the editor is blank until then
        // and the host stays responsive throughout).
        if self.gpu.is_none()
            && let Some(pump) = &mut self.pump
            && let Some(mut backend) = pump.take_init()
        {
            backend.set_pump(pump.client());
            self.gpu = Some(backend);
        }
        // Skip the whole frame while the editor isn't presentable:
        // detached / occluded on macOS, host child window hidden /
        // minimized on Windows (no-op on Linux). On Windows this runs
        // on the host's GUI thread, so skipping an unpresentable frame
        // keeps a blocking present from freezing the host.
        {
            use raw_window_handle::HasRawWindowHandle;
            if crate::platform::should_skip_frame(window.raw_window_handle()) {
                return;
            }
        }
        #[cfg(target_os = "macos")]
        {
            use raw_window_handle::HasRawWindowHandle;
            crate::platform::reanchor_to_superview_top(window.raw_window_handle());
        }
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
                // Push the corrected scale into baseview so its window /
                // mouse-coordinate mapping tracks the host. Without this, a
                // host that reports its content scale only after the view is
                // attached (REAPER on Linux, via
                // `IPlugViewContentScaleSupport`) leaves baseview pinned to the
                // creation-time scale and the two fight, flickering
                // 1x-in-a-2x-frame until it settles. No-op on Windows/macOS.
                window.set_scale_factor(f64::from(cur_scale));
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

                // Compositor pacing veto - scale/resize above still
                // apply during a hold; only the render + present skip.
                // Windows skips the veto: the pump pre-acquires frames
                // off-thread and `try_take_frame` returning `None`
                // already paces paints, so holding here only adds
                // latency.
                if cfg!(not(target_os = "windows")) && self.pacer.should_hold() {
                    return;
                }
                inner.render_to(gpu);
            }
            gpu.present();
            self.pacer.record_acquire(gpu.acquire_wait());
        }
    }
}

impl<P: Params + 'static> WindowHandler for GpuWindowHandler<P> {
    fn on_frame(&mut self, window: &mut Window) {
        // Catch panics at the FFI boundary. baseview drives this via
        // an `extern "C-unwind"` AppKit override; without the catch
        // a Rust panic (e.g. wgpu validation on an overflow) is
        // re-thrown as an ObjC exception and `NSApplication run`
        // terminates the host.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.on_frame_inner(window);
        }));
        if let Err(e) = result {
            let msg = if let Some(s) = e.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = e.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            log::error!("GpuWindowHandler::on_frame panic swallowed: {msg}");
        }
    }

    fn on_event(&mut self, window: &mut Window, event: Event) -> EventStatus {
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
                    //
                    // In host-driven (plug-in) mode the host's reported scale
                    // is authoritative; baseview's echoed `info.scale()` is the
                    // value we pinned at creation. If the host has since
                    // reported a different scale (a late
                    // `IPlugViewContentScaleSupport` call from REAPER on
                    // Linux), baseview is stale and this event's physical size
                    // is at the wrong scale. Push the host scale into baseview
                    // and drop the event; baseview re-emits a `Resized` at the
                    // corrected scale (X11 only - a no-op elsewhere, where this
                    // branch also never triggers because scale is OS-driven).
                    let bv_scale = info.scale();
                    let host_scale = self.scale.get_f32();
                    #[allow(clippy::cast_possible_truncation)]
                    if self.host_driven_scale && (host_scale - bv_scale as f32).abs() > 1.0e-3 {
                        window.set_scale_factor(f64::from(host_scale));
                        return EventStatus::Ignored;
                    }
                    // Mirror baseview's scale into the shared cell ONLY in
                    // system-scale (standalone) mode, where `info.scale()` is
                    // the authoritative OS-detected value. In host-driven mode
                    // the host owns the cell and we confirmed above that
                    // baseview agrees, so writing here would clobber a
                    // concurrent host update (the race that stranded the editor
                    // at 1x).
                    if !self.host_driven_scale {
                        self.scale.set(info.scale());
                    }
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
                        if lw > 0 && lh > 0 {
                            // Fit-only: this handler deliberately never
                            // echoes `request_resize` (see `on_frame`), so
                            // an out-of-bounds host size renders clamped.
                            let (fw, fh) =
                                fit_size(lw, lh, inner.min_size(), inner.max_size(), None);
                            if (fw, fh) != inner.size() {
                                inner.set_size(fw, fh);
                            }
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

    fn can_maximize(&self) -> bool {
        // `false` (the trait default) when the inner lock is poisoned,
        // matching `Editor::can_maximize`'s default.
        self.inner.lock().is_ok_and(|g| g.can_maximize())
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
        // Pick the baseview scale policy. On Linux an embedded plugin
        // follows the host's scale (default 1.0) rather than the desktop
        // Xft.dpi, which a non-DPI-aware host (Bitwig) doesn't share; the
        // standalone and every macOS/Windows path keep SystemScaleFactor.
        let scale_policy = if let Some(s) = crate::platform::editor_window_scale(
            self.use_system_scale,
            self.host_scale_set,
            self.scale.get(),
        ) {
            self.scale.set(s);
            WindowScalePolicy::ScaleFactor(s)
        } else {
            self.scale
                .set(crate::platform::query_backing_scale(&parent));
            WindowScalePolicy::SystemScaleFactor
        };
        // Host-driven scale = pinned `ScaleFactor` policy (embedded plug-in).
        // In that mode baseview's echoed `info.scale()` is our own pinned
        // value, not new information, so the `Resized` handler must not let it
        // overwrite a later host-reported scale.
        let host_driven_scale = matches!(scale_policy, WindowScalePolicy::ScaleFactor(_));
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
            scale: scale_policy,
        };

        let parent_wrapper = ParentWindow(parent);

        let window = baseview::Window::open_parented(
            &parent_wrapper,
            options,
            move |window: &mut Window| {
                // Display scale never exceeds 4.0 in practice.
                #[allow(clippy::cast_possible_truncation)]
                let scale = system_scale as f32;
                // Surface + GPU init live with the pump; the backend
                // arrives via `take_init` in `on_frame`. The panic
                // flag is unused here (no device-loss rebuild in this
                // handler) - a dead pump just leaves the editor blank.
                let device_lost = Arc::new(AtomicBool::new(false));
                let pump = unsafe {
                    truce_gpu::pump::SurfacePump::spawn(
                        window,
                        &device_lost,
                        Box::new(move |_, adapter, surface| {
                            WgpuBackend::pump_init(adapter, surface, size.0, size.1, scale)
                        }),
                    )
                };

                GpuWindowHandler {
                    inner,
                    pump,
                    gpu: None,
                    translator: crate::interaction::BaseviewTranslator::default(),
                    current_size: size,
                    scale: scale_handle,
                    last_applied_scale: scale,
                    host_driven_scale,
                    pacer: crate::platform::PaintPacer::default(),
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
        self.host_scale_set = true;
        self.scale.set(factor);
    }

    fn set_uses_system_scale(&mut self, yes: bool) {
        self.use_system_scale = yes;
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
