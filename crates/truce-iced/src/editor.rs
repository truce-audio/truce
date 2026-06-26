//! `IcedEditor` - implements `truce_core::Editor` using iced for rendering.
//!
//! Drives iced's `UserInterface` directly each frame against a wgpu
//! surface provided by baseview. Used to lean on
//! `iced_runtime::program::State` for this; that surface was removed
//! in iced 0.14, so this module now manages the build / update / draw
//! / cache cycle inline.

use std::sync::Arc;

use crate::iced::{Event, Size};
use iced_wgpu::wgpu;
use truce_core::editor::{Editor, PluginContext};
use truce_gui::EditorScale;
use truce_gui::layout::GridLayout;
use truce_params::Params;

use crate::param_cache::ParamCache;
use crate::runtime::{
    AutoPlugin, IcedPlugin, IcedProgram, IcedRuntime, editor_backends, panic_message,
};

// IcedEditor - main entry point, implements truce_core::Editor

/// Iced-based plugin editor.
///
/// Type parameters:
/// - `P` - the plugin's `Params` type
/// - `M` - the plugin's `IcedPlugin` implementation
pub struct IcedEditor<P, M>
where
    P: Params + 'static,
    M: IcedPlugin<P>,
{
    params: Arc<P>,
    size: (u32, u32),
    /// Live content-scale factor, shared with the runtime via
    /// [`truce_gui::EditorScale`]. Both `set_scale_factor` (host) and
    /// the baseview `Resized` handler write here; the runtime's
    /// `tick()` reads it and reconfigures the surface/viewport when it
    /// diverges from `last_applied_scale`.
    scale: EditorScale,
    font: Option<&'static [u8]>,
    runtime: Option<IcedRuntime<P, M>>,
    /// Constructor closure for the plugin model. Each constructor
    /// stores a closure that produces an `M` of the correct concrete
    /// type:
    /// - `from_layout` captures the `GridLayout` and returns
    ///   `AutoPlugin { layout: layout.clone() }` (the `impl` block
    ///   fixes `M = AutoPlugin`).
    /// - `new` defers to `M::new(params)`.
    ///
    /// `Fn` (not `FnOnce`) so `open()` and `screenshot()` can each
    /// produce a fresh `M`. Hosts that destroy and recreate the editor
    /// (CLAP `gui_destroy` / `gui_create`) call `open()` more than once;
    /// `screenshot()` builds a separate offscreen iced program. The
    /// closure also carries the construction invariant for `AutoPlugin`,
    /// whose `IcedPlugin::new` is `panic!("must be created via
    /// from_layout")` - going through `M::new` instead would panic on
    /// the screenshot path.
    make_plugin: Box<dyn Fn(Arc<P>) -> M + Send + Sync>,
    meter_ids: Vec<u32>,
    baseview_window: Option<baseview::WindowHandle>,
    /// Pending logical size shared with the baseview handler.
    /// Packed as `(width << 32) | height`; `0` is the "no resize
    /// pending" sentinel. `Editor::set_size` writes here so the
    /// handler's `on_frame` can call `baseview::Window::resize`
    /// (which sets the NSView/HWND/X11 frame on the underlying
    /// platform window) and reconfigure the wgpu surface in one
    /// place. Without this handoff the wgpu surface gets the new
    /// size but the platform window stays at its original
    /// dimensions, so the editor renders into a viewport but the
    /// host only paints the un-resized rectangle (visible on
    /// standalone as an editor that fills the original area only
    /// while the outer window grew around it).
    pending_size: Arc<std::sync::atomic::AtomicU64>,
    /// Resize-capability flag exposed via `Editor::can_resize`.
    /// Defaults to `false`; iced plugins that have been designed
    /// with a flexible widget tree opt in with `.resizable(true)`.
    /// The default keeps every existing fixed-size plugin pinned
    /// to its built dimensions instead of silently following an
    /// autoresize-driven parent `NSView` grow.
    can_resize: bool,
    /// Whether the standalone host may maximize the window, exposed
    /// via `Editor::can_maximize`. Defaults to `false`; only consulted
    /// for resizable editors. Opt in with `.maximizable(true)` for
    /// editors that render correctly at any size.
    can_maximize: bool,
    /// Constraints exposed through the `Editor` trait so format
    /// wrappers can hand the host honest bounds.
    min_size: (u32, u32),
    max_size: (u32, u32),
    aspect_ratio: Option<(u32, u32)>,
    prefers_pow2: bool,
}

// SAFETY: `baseview::WindowHandle` holds a raw native window pointer
// (HWND / NSView / X11 Window) and is not auto-`Send`. Hosts call
// `Editor::open` / `idle` / `close` from a single dedicated GUI thread
// - never concurrently and never from the audio thread - so the
// handle is only ever touched on the thread that created it. The
// `Editor` trait requires `Send` so the editor can live behind a
// trait object; this impl asserts that the type doesn't escape its
// thread in practice. The `make_plugin` boxed closure is already
// `Send`-bounded; runtime / meter_ids / size are trivially `Send`.
unsafe impl<P: Params, M: IcedPlugin<P>> Send for IcedEditor<P, M> {}

impl<P: Params + 'static, M: IcedPlugin<P> + 'static> Drop for IcedEditor<P, M> {
    /// Defensive cleanup for hosts that drop the editor without first
    /// calling `Editor::close`. Pro Tools AAX has been seen to do this
    /// on plugin removal under certain conditions; live-coding hosts
    /// and unit tests can also short-circuit the lifecycle. On Linux
    /// `baseview::WindowHandle` has no `Drop`, so without an explicit
    /// `close` the render thread would keep running against a freed
    /// `*mut IcedEditor` and later panic inside wgpu as surfaces tear
    /// down. `close()` is idempotent - `baseview_window.take()`
    /// no-ops on the second call - so calling it here on top of a
    /// well-behaved host's earlier `close()` is safe.
    fn drop(&mut self) {
        Editor::close(self);
    }
}

impl<P: Params + 'static> IcedEditor<P, AutoPlugin> {
    /// Create an editor that auto-generates the UI from a `GridLayout`.
    pub fn from_layout(params: Arc<P>, layout: GridLayout) -> Self {
        let size = (layout.width, layout.height);
        let meter_ids: Vec<u32> = layout
            .widgets
            .iter()
            .filter_map(|w| w.meter_ids.as_ref())
            .flatten()
            .copied()
            .collect();

        let make_plugin: Box<dyn Fn(Arc<P>) -> AutoPlugin + Send + Sync> =
            Box::new(move |_params| AutoPlugin {
                layout: layout.clone(),
            });

        Self {
            params,
            size,
            scale: EditorScale::new(truce_gui::backing_scale()),
            font: None,
            runtime: None,
            make_plugin,
            meter_ids,
            baseview_window: None,
            pending_size: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            can_resize: false,
            can_maximize: false,
            min_size: (1, 1),
            max_size: (u32::MAX, u32::MAX),
            aspect_ratio: None,
            prefers_pow2: false,
        }
    }
}

impl<P: Params + 'static, M: IcedPlugin<P> + 'static> IcedEditor<P, M> {
    /// Create an editor with a custom `IcedPlugin` implementation.
    pub fn new(params: Arc<P>, size: (u32, u32)) -> Self {
        Self {
            params,
            size,
            scale: EditorScale::new(truce_gui::backing_scale()),
            font: None,
            runtime: None,
            make_plugin: Box::new(|p| M::new(p)),
            meter_ids: Vec::new(),
            baseview_window: None,
            pending_size: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            can_resize: false,
            can_maximize: false,
            min_size: (1, 1),
            max_size: (u32::MAX, u32::MAX),
            aspect_ratio: None,
            prefers_pow2: false,
        }
    }

    /// Set a custom default font. The family name is read from the
    /// TTF `name` table - matches the `with_font(bytes)` shape used
    /// by `truce-egui::EguiEditor` and `truce-vizia::ViziaEditor`.
    ///
    /// ```ignore
    /// IcedEditor::new(params, (250, 330))
    ///     .with_font(truce_font::JETBRAINS_MONO)
    /// ```
    #[must_use]
    pub fn with_font(mut self, data: &'static [u8]) -> Self {
        self.font = Some(data);
        self
    }

    /// Set meter IDs to poll each tick.
    #[must_use]
    pub fn with_meter_ids(mut self, ids: Vec<impl Into<u32>>) -> Self {
        self.meter_ids = ids.into_iter().map(std::convert::Into::into).collect();
        self
    }

    /// Opt out of host-driven resizing. iced editors default to
    /// resizable because the widget tree reflows for free; pass
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
    /// standalone host consults this, and only when `resizable(true)`.
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

    /// Maximum logical-point dimensions the editor accepts.
    #[must_use]
    pub fn max_size(mut self, max: (u32, u32)) -> Self {
        self.max_size = max;
        self
    }

    /// Lock the aspect ratio as `(numerator, denominator)`. Pass
    /// `None` (the default) for free resizing.
    #[must_use]
    pub fn aspect_ratio(mut self, ratio: Option<(u32, u32)>) -> Self {
        self.aspect_ratio = ratio;
        self
    }

    /// Hint that the renderer prefers power-of-two surface sizes.
    #[must_use]
    pub fn prefers_pow2(mut self, prefers: bool) -> Self {
        self.prefers_pow2 = prefers;
        self
    }
}

// Baseview window handler (all platforms)

struct IcedBaseviewHandler<P: Params + 'static, M: IcedPlugin<P>> {
    editor: *mut IcedEditor<P, M>,
    last_cursor: Option<baseview::MouseCursor>,
}

// SAFETY: The raw `*mut IcedEditor<P, M>` is only dereferenced from
// the baseview event thread (which `WindowHandler` is bound to). The
// editor's host-side close path joins this thread before dropping the
// editor, so the pointer is always valid while `WindowHandler`
// methods run. baseview requires `Send` for its handler types so that
// the handler can be moved onto the dedicated event thread on
// construction; once moved, it never crosses threads again.
unsafe impl<P: Params, M: IcedPlugin<P>> Send for IcedBaseviewHandler<P, M> {}

impl<P: Params + 'static, M: IcedPlugin<P>> Drop for IcedBaseviewHandler<P, M> {
    fn drop(&mut self) {
        // Drop wgpu/iced render state on the baseview event thread, while
        // any underlying display connection (e.g. X11 Display via XcbConnection)
        // is still alive. If we let the host-thread close() path drop
        // `runtime.render` instead, NVIDIA's Vulkan surface-destruction code
        // tries to use a freed Display and segfaults inside _XSend.
        //
        // Safety: close() always calls window.close() which joins this
        // thread before returning. While this drop runs, the host thread
        // is blocked in join(), so `self.editor` is still valid.
        let editor = unsafe { &mut *self.editor };
        if let Some(ref mut runtime) = editor.runtime {
            runtime.render = None;
        }
    }
}

// The explicit `Idle | None => Default` arm documents iced's known
// no-cursor states; the trailing `_ => Default` keeps forward-compat
// against future iced enum variants. Both intentionally share the
// value.
#[allow(clippy::match_same_arms)]
fn iced_interaction_to_cursor(
    interaction: crate::iced::mouse::Interaction,
) -> baseview::MouseCursor {
    use crate::iced::mouse::Interaction;
    match interaction {
        Interaction::Idle | Interaction::None => baseview::MouseCursor::Default,
        Interaction::Pointer | Interaction::Grab => baseview::MouseCursor::Hand,
        Interaction::Grabbing => baseview::MouseCursor::HandGrabbing,
        Interaction::Text => baseview::MouseCursor::Text,
        Interaction::Crosshair => baseview::MouseCursor::Crosshair,
        Interaction::NotAllowed => baseview::MouseCursor::NotAllowed,
        Interaction::ResizingHorizontally => baseview::MouseCursor::EwResize,
        Interaction::ResizingVertically => baseview::MouseCursor::NsResize,
        _ => baseview::MouseCursor::Default,
    }
}

impl<P: Params + 'static, M: IcedPlugin<P>> baseview::WindowHandler for IcedBaseviewHandler<P, M> {
    fn on_frame(&mut self, window: &mut baseview::Window) {
        // Catch panics at the FFI boundary: baseview drives this from an
        // `extern "system"` window proc (Windows) / AppKit callback (macOS),
        // so an unwinding panic - e.g. a wgpu device loss mid-resize - would
        // cross a C frame and abort the host. Swallow and log instead.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
            let editor = unsafe { &mut *self.editor };
            // Rebuild the pipeline if the device was lost (flagged by the
            // device-lost callback or a swallowed render panic). Skip the rest of
            // this frame; the next tick renders against the fresh device.
            if let Some(ref mut runtime) = editor.runtime
                && runtime
                    .device_lost
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                let ok = runtime.recover_device(window);
                log::warn!("iced device-loss recovery: rebuilt ok={ok}");
                return;
            }
            // Pick up host-driven `set_size` requests since the last
            // frame. Without this the wgpu surface would be at the new
            // size but the platform window stays at the original
            // dimensions, so the editor visibly fills only the old
            // rect inside a larger host frame.
            let packed = editor
                .pending_size
                .swap(0, std::sync::atomic::Ordering::Acquire);
            if packed != 0 {
                #[allow(clippy::cast_possible_truncation)]
                let new_w = (packed >> 32) as u32;
                #[allow(clippy::cast_possible_truncation)]
                let new_h = (packed & 0xFFFF_FFFF) as u32;
                if new_w > 0 && new_h > 0 {
                    window.resize(baseview::Size::new(f64::from(new_w), f64::from(new_h)));
                    if let Some(ref mut runtime) = editor.runtime {
                        runtime.size = (new_w, new_h);
                        if let Some(ref mut render) = runtime.render {
                            let scale = editor.scale.get();
                            let pw = truce_gui::to_physical_px(new_w, scale);
                            let ph = truce_gui::to_physical_px(new_h, scale);
                            #[allow(clippy::cast_possible_truncation)]
                            let scale_f32 = scale as f32;
                            render.viewport = iced_graphics::Viewport::with_physical_size(
                                Size::new(pw, ph),
                                scale_f32,
                            );
                            render.surface_config.width = pw;
                            render.surface_config.height = ph;
                            render
                                .surface
                                .configure(&render.device, &render.surface_config);
                        }
                    }
                }
            }
            if let Some(ref mut runtime) = editor.runtime {
                runtime.tick();
                if let Some(ref render) = runtime.render {
                    let cursor = iced_interaction_to_cursor(render.interaction);
                    if self.last_cursor != Some(cursor) {
                        self.last_cursor = Some(cursor);
                        window.set_mouse_cursor(cursor);
                    }
                }
            }
        }));
        if let Err(e) = result {
            log::error!("iced on_frame panic swallowed: {}", panic_message(&e));
            // A render panic almost always means the device is dead (e.g.
            // `queue.write_buffer_with` -> None after a loss that didn't fire
            // the callback). Arm recovery so the next frame rebuilds.
            let editor = unsafe { &mut *self.editor };
            if let Some(ref runtime) = editor.runtime {
                runtime
                    .device_lost
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }
    }

    fn on_event(
        &mut self,
        #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
        window: &mut baseview::Window,
        event: baseview::Event,
    ) -> baseview::EventStatus {
        // Catch panics at the FFI boundary, like `on_frame`; report the event
        // as `Ignored` on panic instead of aborting the host.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let editor = unsafe { &mut *self.editor };
            let Some(runtime) = editor.runtime.as_mut() else {
                return baseview::EventStatus::Ignored;
            };

            match event {
                baseview::Event::Mouse(mouse) => {
                    match mouse {
                        baseview::MouseEvent::CursorMoved { position, .. } => {
                            // baseview reports logical points; iced widgets
                            // hit-test in logical units against
                            // `viewport.logical_size()`, so forward as-is.
                            // Window dimensions stay well below 2^23 - the
                            // f64 → f32 narrowing is invisible.
                            #[allow(clippy::cast_possible_truncation)]
                            let pos = (position.x as f32, position.y as f32);
                            runtime.queue_cursor_move(pos.0, pos.1);
                        }
                        baseview::MouseEvent::CursorLeft => {
                            runtime
                                .pending_events
                                .push(Event::Mouse(crate::iced::mouse::Event::CursorLeft));
                        }
                        baseview::MouseEvent::ButtonPressed {
                            button: baseview::MouseButton::Left,
                            ..
                        } => {
                            // WS_CHILD plugin windows don't receive WM_KEYDOWN
                            // until focused; baseview doesn't SetFocus on click,
                            // so we do it here. Without this, text-edit widgets
                            // never see keystrokes on Windows.
                            #[cfg(target_os = "windows")]
                            {
                                if !window.has_focus() {
                                    window.focus();
                                }
                            }
                            runtime.pending_events.push(Event::Mouse(
                                crate::iced::mouse::Event::ButtonPressed(
                                    crate::iced::mouse::Button::Left,
                                ),
                            ));
                        }
                        baseview::MouseEvent::ButtonReleased {
                            button: baseview::MouseButton::Left,
                            ..
                        } => {
                            runtime.pending_events.push(Event::Mouse(
                                crate::iced::mouse::Event::ButtonReleased(
                                    crate::iced::mouse::Button::Left,
                                ),
                            ));
                        }
                        baseview::MouseEvent::WheelScrolled { delta, .. } => {
                            let dy = match delta {
                                baseview::ScrollDelta::Lines { y, .. } => y * 30.0,
                                baseview::ScrollDelta::Pixels { y, .. } => y,
                            };
                            runtime.pending_events.push(Event::Mouse(
                                crate::iced::mouse::Event::WheelScrolled {
                                    delta: crate::iced::mouse::ScrollDelta::Pixels {
                                        x: 0.0,
                                        y: dy,
                                    },
                                },
                            ));
                        }
                        _ => return baseview::EventStatus::Ignored,
                    }
                    baseview::EventStatus::Captured
                }
                baseview::Event::Window(baseview::WindowEvent::Resized(info)) => {
                    crate::platform::note_linux_scale_factor(info.scale());
                    // Mirror the OS-reported scale into the shared cell
                    // (so a follow-up `set_scale_factor` from the host
                    // reads a fresh baseline) and bump `last_applied_scale`
                    // so `tick()`'s diff-check stays a no-op - we apply
                    // the reconfigure inline below.
                    runtime.scale.set(info.scale());
                    runtime.last_applied_scale = info.scale();
                    if let Some(ref mut render) = runtime.render {
                        let pw = info.physical_size().width;
                        let ph = info.physical_size().height;
                        render.surface_config.width = pw.max(1);
                        render.surface_config.height = ph.max(1);
                        render
                            .surface
                            .configure(&render.device, &render.surface_config);
                        #[allow(clippy::cast_possible_truncation)] // display DPI; bounded
                        let scale_f32 = info.scale() as f32;
                        render.viewport = iced_graphics::Viewport::with_physical_size(
                            Size::new(pw, ph),
                            scale_f32,
                        );
                    }
                    baseview::EventStatus::Captured
                }
                baseview::Event::Keyboard(kb) => {
                    // Feed native keys into the `UserInterface` event queue;
                    // iced widgets (text_input, a custom key-capture widget)
                    // then receive them. Keys only arrive when the host grants
                    // the editor window OS focus, which varies by DAW.
                    runtime
                        .pending_events
                        .push(Event::Keyboard(crate::keyboard::to_iced_event(&kb)));
                    baseview::EventStatus::Captured
                }
                baseview::Event::Window(_) => baseview::EventStatus::Ignored,
            }
        }));
        match result {
            Ok(status) => status,
            Err(e) => {
                log::error!("iced on_event panic swallowed: {}", panic_message(&e));
                baseview::EventStatus::Ignored
            }
        }
    }
}

// Editor trait implementation

impl<P: Params + 'static, M: IcedPlugin<P>> Editor for IcedEditor<P, M> {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: truce_core::editor::RawWindowHandle, context: PluginContext) {
        let (w, h) = self.size;
        // Drop any stale `set_size` that fired before this
        // `open()` so the handler doesn't immediately re-resize
        // the freshly-built window to a previous request.
        self.pending_size
            .store(0, std::sync::atomic::Ordering::Relaxed);

        // Create the plugin model. The closure is `Fn`, not `FnOnce`,
        // so destroy/recreate cycles (CLAP `gui_destroy` / `gui_create`,
        // some VST3 hosts that close+reopen the editor) reuse it.
        let plugin = (self.make_plugin)(self.params.clone());

        let mut param_cache = ParamCache::new(self.params.clone());
        if let Some(data) = self.font {
            // `apply_font` is idempotent on the iced font-system side
            // (load_font is fine to call twice with the same bytes);
            // the redundant load here is cheap and lets the canvas
            // widgets reuse the correct family without threading the
            // already-derived `crate::iced::Font` from the runtime path.
            param_cache.set_font(crate::font::apply_font(data));
        }
        let typed_ctx = context.with_params(self.params.clone());
        let program = IcedProgram {
            plugin,
            param_cache,
            context: typed_ctx,
            meter_ids: self.meter_ids.clone(),
        };

        self.runtime = Some(IcedRuntime::new(
            (w, h),
            self.scale.clone(),
            self.font,
            program,
        ));

        let parent_wrapper = crate::platform::ParentWindow(parent);
        let options = baseview::WindowOpenOptions {
            title: String::from("truce-iced"),
            size: baseview::Size::new(f64::from(w), f64::from(h)),
            scale: baseview::WindowScalePolicy::SystemScaleFactor,
        };

        let editor_addr = std::ptr::from_mut::<IcedEditor<P, M>>(self) as usize;

        let window = baseview::Window::open_parented(
            &parent_wrapper,
            options,
            move |window: &mut baseview::Window| {
                let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
                    backends: editor_backends(),
                    ..Default::default()
                });

                let surface = unsafe { crate::platform::create_wgpu_surface(&instance, window) };

                if let Some(surface) = surface {
                    let editor = unsafe { &mut *(editor_addr as *mut IcedEditor<P, M>) };
                    if let Some(ref mut runtime) = editor.runtime {
                        runtime.init_render(instance, surface);
                    }
                }

                IcedBaseviewHandler::<P, M> {
                    editor: editor_addr as *mut IcedEditor<P, M>,
                    last_cursor: None,
                }
            },
        );

        self.baseview_window = Some(window);
        log::info!("editor opened via baseview ({w}x{h})");
    }

    fn close(&mut self) {
        // baseview's Linux WindowHandle has no Drop impl - we must call
        // close() explicitly to request shutdown and join the render
        // thread. Without this, the thread keeps running against a
        // dangling self pointer after the host drops this editor, which
        // later panics inside wgpu as surfaces get torn down.
        if let Some(mut window) = self.baseview_window.take() {
            window.close();
        }
        self.runtime = None;
        log::info!("editor closed");
    }

    fn idle(&mut self) {
        // baseview drives its own frame loop via on_frame().
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

    fn screenshot(
        &mut self,
        _params: Arc<dyn truce_params::Params>,
    ) -> Option<(Vec<u8>, u32, u32)> {
        // Build the plugin via the editor's own constructor closure.
        // Calling `M::new` directly would panic for `AutoPlugin` -
        // `from_layout` captures the `GridLayout` in the closure and
        // the `IcedPlugin::new` impl on `AutoPlugin` is `panic!("must
        // be created via from_layout")`.
        let plugin = (self.make_plugin)(Arc::clone(&self.params));
        // Match the live editor's content scale so the screenshot
        // exercises the same render path the user sees. `EditorScale`
        // falls back to `backing_scale()` for pre-open / headless
        // calls.
        let scale = self.scale.get();
        crate::screenshot::render_to_pixels::<P, M>(
            Arc::clone(&self.params),
            plugin,
            self.size,
            scale,
            self.font,
        )
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 {
            return false;
        }
        self.size = (width, height);
        // Hand the new logical size to the live baseview handler;
        // its `on_frame` reads the cell and runs the unified
        // `baseview::Window::resize` + iced viewport + wgpu
        // surface reconfigure sequence in one place. The handler
        // also exists when the editor isn't open, but the cell
        // gets reset to `0` in `open()` to drop any stale write.
        self.pending_size.store(
            (u64::from(width) << 32) | u64::from(height),
            std::sync::atomic::Ordering::Release,
        );
        true
    }

    fn set_scale_factor(&mut self, factor: f64) {
        // Write to the shared cell; the runtime's `tick()` picks up the
        // change on its next frame and reconfigures the surface and
        // viewport.
        self.scale.set(factor);
    }
}
