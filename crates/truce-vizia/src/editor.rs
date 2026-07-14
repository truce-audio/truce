//! `ViziaEditor` - the `truce_core::editor::Editor` impl that wraps
//! a `vizia::Application` mounted via `baseview-truce` onto the
//! DAW-provided parent window.

#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use truce_core::editor::{Editor, PluginContext, RawWindowHandle};
use truce_params::Params;
use vizia::prelude::*;

use crate::param_lens::ParamLens;
use crate::platform::ParentWindow;

/// Per-frame setup closure. The plugin author writes their vizia
/// view inside this closure using the `Context` + `ParamLens<P>`
/// provided.
///
/// Bounded `Fn` (not `FnOnce`) because hosts may close and re-open
/// the editor; each open call invokes this once to build a fresh
/// widget tree.
pub type SetupFn<P> = Arc<dyn Fn(&mut Context, ParamLens<P>) + Send + Sync>;

/// Stylesheet applied when a plugin calls `with_font(...)`. Points
/// the root entity at the registered font's family; vizia inherits
/// `font-family` from root down to every descendant, so the
/// universal `*` selector (which forces a restyle on every entity
/// per tick and caused intermittent partial-paint artefacts) isn't
/// needed.
pub(crate) const JETBRAINS_MONO_FAMILY_CSS: &str = ":root { font-family: \"JetBrains Mono\"; }";

pub struct ViziaEditor<P: Params + ?Sized> {
    params: Arc<P>,
    size: (u32, u32),
    setup: SetupFn<P>,
    /// User-supplied stylesheets, applied in the order
    /// `with_stylesheet` was called. `ViziaEditor` adds nothing
    /// itself - plugins opting into the bundled widgets pass
    /// `truce_vizia::widgets::BASE_CSS`; plugins opting into the
    /// dark palette pass `truce_vizia::theme::DARK`; everything
    /// else is the plugin author's call.
    stylesheets: Vec<&'static str>,
    /// Optional embedded font bytes. Most plugins pass
    /// `truce_font::JETBRAINS_MONO`.
    font: Option<&'static [u8]>,
    /// Lower clamp retained from the builder. Currently unused for
    /// host enforcement (vizia editors are fixed-size on every
    /// platform - see [`Editor::can_resize`]); kept so the
    /// `.min_size()` / `.max_size()` builders stay API-stable.
    min_size: (u32, u32),
    /// Upper clamp retained from the builder. See [`Self::min_size`].
    max_size: (u32, u32),
    /// Host content-scale factor (`Editor::set_scale_factor`); pins
    /// vizia's window scale policy in `open()`. `None` -> vizia's OS
    /// `SystemScaleFactor` default.
    scale: Option<f64>,
    /// Standalone hosts set this (via `set_uses_system_scale`) so an
    /// editor with no host-reported scale honors the desktop `Xft.dpi`
    /// on Linux; an embedded plugin leaves it false and defaults to 1.0
    /// there instead (a non-DPI-aware host runs at 1x). No effect off
    /// Linux, or when the host reported a scale.
    use_system_scale: bool,
    window: Option<vizia::WindowHandle>,
    /// Live handle to vizia's event loop, captured from the setup
    /// closure on `open()` and zeroed on `close()` / `Drop`. Lets
    /// `idle()` push a late host scale change into vizia via
    /// `WindowEvent::SetUserScale` - the only rescale entry point for
    /// an already-open vizia window (its `WindowScalePolicy` is frozen
    /// at open). Shared because the closure runs on baseview's window
    /// thread while `idle()` runs on the host GUI thread.
    scale_proxy: Arc<Mutex<Option<ContextProxy>>>,
    /// The scale vizia's window opened at (its `ScaleFactor` policy).
    /// `idle()` compares the live host scale against this to derive the
    /// compensating user-scale. `None` when the window opened under the
    /// OS `SystemScaleFactor` (standalone), where late host scale
    /// reports don't apply.
    opened_scale: Option<f64>,
    /// Last user-scale pushed via `SetUserScale`, so `idle()` re-emits
    /// only on an actual change (init `1.0` = no compensation).
    last_user_scale: f64,
    /// Host parent `NSView` pointer the `on_idle` re-anchor walks.
    /// Shared with the idle closure and zeroed on `close()` / `Drop`
    /// so a late idle tick (queued past window teardown) can't message
    /// an `NSView` the host has already freed or reused.
    #[cfg(target_os = "macos")]
    reanchor_parent: Arc<AtomicUsize>,
}

// SAFETY: `vizia::WindowHandle` holds an opaque baseview handle that
// is `Send` per baseview's contract. The setup closure is bounded
// `Send + Sync` at construction. Mirrors the unsafe-impl that
// `truce-egui` / `truce-iced` / `truce-slint` all use - hosts always
// call `Editor::open` / `close` on a single GUI thread.
unsafe impl<P: Params + ?Sized> Send for ViziaEditor<P> {}

impl<P: Params + 'static> ViziaEditor<P> {
    /// Build a new editor.
    ///
    /// `size` is the window size in logical points. `setup` runs on
    /// each `open()` and constructs the vizia view tree against the
    /// supplied `ParamLens`.
    pub fn new(
        params: Arc<P>,
        size: (u32, u32),
        setup: impl Fn(&mut Context, ParamLens<P>) + Send + Sync + 'static,
    ) -> Self {
        Self {
            params,
            size,
            setup: Arc::new(setup),
            stylesheets: Vec::new(),
            font: None,
            min_size: (1, 1),
            max_size: (u32::MAX, u32::MAX),
            scale: None,
            use_system_scale: false,
            window: None,
            scale_proxy: Arc::new(Mutex::new(None)),
            opened_scale: None,
            last_user_scale: 1.0,
            #[cfg(target_os = "macos")]
            reanchor_parent: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Append a CSS stylesheet, applied after vizia's defaults *and*
    /// after every previously-added sheet. Call repeatedly to layer
    /// (e.g. `with_stylesheet(widgets::BASE_CSS)` followed by
    /// `with_stylesheet(theme::DARK)` followed by a plugin-specific
    /// `include_str!("../assets/extras.css")`). The editor adds no
    /// stylesheet itself - everything is opt-in.
    #[must_use]
    pub fn with_stylesheet(mut self, css: &'static str) -> Self {
        self.stylesheets.push(css);
        self
    }

    /// Register an embedded font for the editor's default family.
    /// Pass `truce_font::JETBRAINS_MONO` to match the look of
    /// every other built-in editor.
    #[must_use]
    pub fn with_font(mut self, font_bytes: &'static [u8]) -> Self {
        self.font = Some(font_bytes);
        self
    }

    /// **Currently a no-op** — vizia editors are fixed-size on every
    /// platform. `vizia_baseview` exposes no resize entry point we can
    /// drive from `Editor::set_size`, so host/user resize can't be
    /// followed; advertising it just lets the host grow the window while
    /// vizia stays put, leaving uninitialised pixels in the exposed gap.
    /// Kept (accepting and ignoring `value`) so existing plugin code and
    /// the `.min_size()` / `.max_size()` builders stay API-stable; will
    /// gain real behaviour if a `vizia_baseview` resize entry point lands.
    #[must_use]
    pub fn resizable(self, _value: bool) -> Self {
        self
    }

    /// Lower clamp on host-driven resize requests, in logical
    /// points. Defaults to `(1, 1)`. Surfaced through
    /// `Editor::min_size` to CLAP `gui_get_resize_hints` and VST3
    /// `checkSizeConstraint`.
    #[must_use]
    pub fn min_size(mut self, size: (u32, u32)) -> Self {
        self.min_size = size;
        self
    }

    /// Upper clamp on host-driven resize requests. Defaults to
    /// `(u32::MAX, u32::MAX)`. Surfaced through `Editor::max_size`.
    #[must_use]
    pub fn max_size(mut self, size: (u32, u32)) -> Self {
        self.max_size = size;
        self
    }
}

impl<P: Params + 'static> Editor for ViziaEditor<P> {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn can_resize(&self) -> bool {
        // Fixed-size on every platform. `vizia_baseview` exposes no
        // resize entry point, so `set_size` can't be applied; reporting
        // resizable would let the host/WM grow the outer window while
        // vizia stays put, leaving uninitialised pixels in the exposed
        // gap. Reporting `false` makes every format pin the window
        // instead (VST3 `canResize` -> 0, CLAP no resize hints,
        // standalone `pin_size`).
        false
    }

    fn min_size(&self) -> (u32, u32) {
        self.min_size
    }

    fn max_size(&self) -> (u32, u32) {
        self.max_size
    }

    fn set_size(&mut self, w: u32, h: u32) -> bool {
        if !self.can_resize() || w == 0 || h == 0 {
            return false;
        }
        // Clamp to the plugin's declared bounds and record the new
        // logical size so subsequent `Editor::size()` reads (and the
        // standalone's outer-window snap) see honest values. The
        // actual surface update arrives through the macOS autoresize
        // cascade: when the parent `NSView` grows, baseview-truce's
        // `setFrameSize:` override fires `Resized` and
        // `vizia_baseview`'s existing handler reconfigures skia +
        // calls `cx.set_window_size`. We accept the request (returning
        // `true`) under all callers so the contract matches the other
        // backends; a host that calls `gui_set_size` without also
        // resizing the parent `NSView` will see this method succeed
        // but won't see a visual change until the cascade arrives, or
        // until a `vizia_baseview` upstream patch exposes a
        // window-event resize entry point.
        let w = w.clamp(self.min_size.0.max(1), self.max_size.0.max(1));
        let h = h.clamp(self.min_size.1.max(1), self.max_size.1.max(1));
        self.size = (w, h);
        true
    }

    fn set_scale_factor(&mut self, factor: f64) {
        // Capture the host's content scale (VST3
        // `IPlugViewContentScaleSupport` / CLAP `set_scale`) so `open()`
        // pins vizia to it instead of the OS-detected scale, which can
        // differ from the host's and mis-size the editor against the
        // allocated rect. Applies on the next `open()` - vizia_baseview
        // has no live rescale entry point for an already-open window.
        if factor.is_finite() && factor > 0.0 {
            self.scale = Some(factor);
        }
    }

    fn set_uses_system_scale(&mut self, yes: bool) {
        self.use_system_scale = yes;
    }

    fn screenshot(
        &mut self,
        _params: Arc<dyn truce_params::Params>,
    ) -> Option<(Vec<u8>, u32, u32)> {
        let typed_ctx = truce_core::editor::for_test_params(
            self.params.clone() as Arc<dyn truce_params::Params>
        )
        .with_params(self.params.clone());
        crate::screenshot::render_with_state::<P>(
            &self.setup,
            typed_ctx,
            self.size,
            &self.stylesheets,
            self.font,
        )
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        // Refuse to open without hardware OpenGL. Without the WGL
        // extensions, baseview's GL bootstrap panics inside the Win32
        // window proc during `open_parented` - a context where a panic
        // cannot unwind and aborts the entire host, out of reach of
        // the `catch_unwind` below. A blank editor beats a dead DAW.
        #[cfg(target_os = "windows")]
        {
            if !truce_gui_utils::wgl_extensions_available() {
                log::error!(
                    "vizia editor: hardware OpenGL (WGL extensions) unavailable;                      not opening the editor. A broken or mismatched GPU driver                      (or a remote session) is the usual cause"
                );
                return;
            }
        }
        let (lw, lh) = self.size;
        let setup = Arc::clone(&self.setup);
        let stylesheets = self.stylesheets.clone();
        let font = self.font;
        let typed_ctx = context.with_params(self.params.clone());

        // Reset the late-scale channel for this open. The setup closure
        // (below) restocks the proxy once vizia's context exists; clear
        // any stale one from a prior open so `idle()` can't emit through
        // a dead event loop before the new window is up, and reset the
        // last-pushed user-scale to the identity baseline.
        if let Ok(mut slot) = self.scale_proxy.lock() {
            *slot = None;
        }
        self.last_user_scale = 1.0;
        let scale_proxy = Arc::clone(&self.scale_proxy);

        // Capture the parent NSView pointer (macOS only) as `usize`
        // so vizia's `on_idle` closure can move it across the `Send`
        // bound. The pointer is an opaque AppKit handle, not a Rust
        // resource, and is only dereferenced on the GUI thread.
        #[cfg(target_os = "macos")]
        let parent_for_reanchor: usize = match parent {
            RawWindowHandle::AppKit(ptr) => ptr as usize,
            _ => 0,
        };

        let parent_wrapper = ParentWindow(parent);

        // Build the lens up front so the view-building setup closure and
        // the idle tick (below) share the same signal maps.
        let lens = ParamLens::new(typed_ctx);
        let lens_setup = lens.clone();

        let app = Application::new(move |cx| {
            // Register the embedded font, if any, before any view
            // builds - vizia caches font shaping per family.
            if let Some(bytes) = font {
                cx.add_font_mem(bytes);
                // Vizia registers the font under whatever family name
                // the TTF advertises but leaves `:root { font-family }`
                // pointing at the system stack from its default
                // theme. Override here so plugins that opt into
                // `with_font(JETBRAINS_MONO)` actually render in
                // JetBrains Mono. truce-font is the only font crate
                // we ship and the only one `with_font` is documented
                // to take, so the family name is fixed.
                let _ = cx.add_stylesheet(JETBRAINS_MONO_FAMILY_CSS);
            }
            // Stylesheets are applied in the order they were added
            // via `with_stylesheet`. Plugins opt into widget styling
            // by passing `truce_vizia::widgets::BASE_CSS`; everything
            // else is the plugin author's call.
            for css in &stylesheets {
                let _ = cx.add_stylesheet(*css);
            }
            // Build the user's view tree; its widgets register their
            // value / meter signals into the shared `lens` here, before
            // the idle tick starts fanning store values into them.
            (setup)(cx, lens_setup.clone());

            // Stash a proxy into vizia's event loop so `idle()` can push
            // a late host content-scale change back in as
            // `WindowEvent::SetUserScale`. Captured here because this
            // closure runs on baseview's window thread, where the live
            // `Context` exists; `idle()` (host GUI thread) only ever
            // holds the shared slot.
            if let Ok(mut slot) = scale_proxy.lock() {
                *slot = Some(cx.get_proxy());
            }
        })
        .inner_size((lw, lh));

        // Pin the window scale to the host's reported content scale when
        // we have one, rather than vizia's default `SystemScaleFactor`
        // (the OS-detected DPI). The two can disagree for an embedded
        // plug-in view - notably on Windows - which renders the editor at
        // the wrong scale and overflows/clips the host-allocated rect.
        //
        // With no host report, keep the OS default (`None`) EXCEPT for an
        // embedded plug-in on Linux: there `SystemScaleFactor` reads the
        // desktop `Xft.dpi`, a bad proxy for a non-DPI-aware host's scale
        // (Bitwig on X11 runs at 1x and would get a double-sized window),
        // so default to 1.0. The standalone (`use_system_scale`) keeps the
        // OS default so its top-level window still honors desktop scaling.
        let policy_scale = if let Some(scale) = self.scale {
            Some(scale)
        } else if cfg!(target_os = "linux") && !self.use_system_scale {
            Some(1.0)
        } else {
            None
        };
        let app = match policy_scale {
            Some(scale) => app.with_scale_policy(WindowScalePolicy::ScaleFactor(scale)),
            None => app,
        };
        // Remember the scale the window is pinned to so `idle()` can
        // detect a later host scale report and compensate. `None`
        // (OS `SystemScaleFactor`) opts out - that path already tracks
        // real DPI and needs no correction.
        self.opened_scale = policy_scale;

        // vizia's render-loop idle callback drives host->UI sync. We use
        // it instead of `cx.add_timer` because an embedded plug-in child
        // window (notably Bitwig on Windows) never delivers timer ticks,
        // which left automation-driven `value_signal` widgets frozen
        // until the editor was reopened. Throttled to ~30Hz;
        // `refresh_params` uses `set_if_changed`, so an idle editor
        // doesn't repaint on this account.
        //
        // On macOS the same tick also re-anchors every child NSView of
        // the host's plug-in pane to the parent's top, so a canvas that
        // grows under host/user resize doesn't drift its top edge out of
        // view (clipping the header / first row). The parent pointer
        // lives in a shared atomic that `close()` / `Drop` zero, so a
        // late tick fired after teardown reads 0 and skips instead of
        // messaging a freed `NSView`.
        #[cfg(target_os = "macos")]
        self.reanchor_parent
            .store(parent_for_reanchor, Ordering::Relaxed);
        #[cfg(target_os = "macos")]
        let reanchor = Arc::clone(&self.reanchor_parent);
        let idle_lens = lens;
        let last_refresh = std::cell::Cell::new(std::time::Instant::now());
        let app = app.on_idle(move |_cx| {
            #[cfg(target_os = "macos")]
            {
                let ptr = reanchor.load(Ordering::Relaxed);
                if ptr != 0 {
                    truce_gui_utils::reanchor_all_children_to_top(ptr as *mut std::ffi::c_void);
                }
            }
            // Throttle the store->signal fan-out; `on_idle` can fire at
            // the full frame rate, faster than the UI needs.
            let now = std::time::Instant::now();
            if now.duration_since(last_refresh.get()) >= std::time::Duration::from_millis(33) {
                last_refresh.set(now);
                idle_lens.refresh_params();
                idle_lens.refresh_meters();
            }
        });

        // Catch panics at the FFI boundary, like the other GUI
        // backends' handlers: this `open` runs inside the plugin
        // format's `extern "C"` callback, where an escaping panic
        // aborts the entire host. The known case is OpenGL context
        // creation inside `open_parented` - baseview unwraps WGL
        // extension pointers that a software-GL environment (broken
        // ICD, RDP session) doesn't provide. The editor stays
        // unopened; the host keeps running.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.open_parented(&parent_wrapper)
        })) {
            Ok(window) => self.window = Some(window),
            Err(e) => {
                let msg = e
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| e.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                log::error!("vizia editor failed to open: {msg}");
            }
        }
    }

    fn close(&mut self) {
        // Stop the idle re-anchor from touching the parent before the
        // window tears down (both run on the GUI thread, so this is
        // ordered ahead of any later idle tick).
        #[cfg(target_os = "macos")]
        self.reanchor_parent.store(0, Ordering::Relaxed);
        // Drop the event-loop proxy so a late `idle()` can't emit into a
        // window that's tearing down.
        if let Ok(mut slot) = self.scale_proxy.lock() {
            *slot = None;
        }
        if let Some(mut window) = self.window.take() {
            window.close();
        }
    }

    fn idle(&mut self) {
        // vizia drives its own repaint loop via baseview, so no tick is
        // needed for that. The one thing truce must forward is a *late*
        // host content-scale report: REAPER on Linux only calls
        // `IPlugViewContentScaleSupport::setContentScaleFactor` after
        // the view is attached, so the window opens at the wrong scale -
        // a 1x editor stranded in the host's 2x frame. vizia's
        // `WindowScalePolicy` is frozen at open, so the only correction
        // path is `WindowEvent::SetUserScale(host / opened)`, which
        // re-DPIs the tree and resizes the child to fill the host frame.
        // The common case (scale already correct at open) yields `1.0`
        // and emits nothing.
        let (Some(opened), Some(host)) = (self.opened_scale, self.scale) else {
            return;
        };
        if opened <= 0.0 {
            return;
        }
        let user_scale = host / opened;
        if (user_scale - self.last_user_scale).abs() <= 1.0e-3 {
            return;
        }
        if let Ok(mut slot) = self.scale_proxy.lock()
            && let Some(proxy) = slot.as_mut()
            && proxy.emit(WindowEvent::SetUserScale(user_scale)).is_ok()
        {
            self.last_user_scale = user_scale;
        }
    }
}

impl<P: Params + ?Sized> Drop for ViziaEditor<P> {
    fn drop(&mut self) {
        // Mirrors the `Drop` impls on the other backends:
        // `WindowHandle::close()` cancels the macOS frame timer so a
        // host that drops the editor without calling `Editor::close`
        // doesn't leak the underlying CFRunLoop source.
        #[cfg(target_os = "macos")]
        self.reanchor_parent.store(0, Ordering::Relaxed);
        if let Ok(mut slot) = self.scale_proxy.lock() {
            *slot = None;
        }
        if let Some(mut window) = self.window.take() {
            window.close();
        }
    }
}
