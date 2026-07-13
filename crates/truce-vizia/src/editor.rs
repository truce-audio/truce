//! `ViziaEditor` - the `truce_core::editor::Editor` impl that wraps
//! a `vizia::Application` mounted via `baseview-truce` onto the
//! DAW-provided parent window.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicUsize, Ordering};

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

        // Created before the view tree so `on_idle` can poll host-written
        // params/meters without `cx.add_timer`/`start_timer`. vizia_core
        // 0.4.0's timer heap has a `modify_timer` infinite-loop bug, and
        // embedded hosts (Bitwig on Windows) often never deliver timer
        // ticks to a plug-in child window anyway - LX plugins drive their
        // own ~33ms cadence via a draw()-based Ticker + `needs_redraw()`,
        // which does run `on_idle` every frame.
        let lens = ParamLens::new(typed_ctx.clone());
        let lens_for_idle = lens.clone();
        let last_param_poll = Arc::new(Mutex::new(Instant::now()));

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
            // Run setup first so widgets register `value_signal` /
            // `meter_signal` handles before the first idle poll.
            (setup)(cx, lens.clone());
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

        // ~30 Hz idle poll: host→store→`value_signal` / `meter_signal`
        // sync plus macOS child-view re-anchor. Replaces the old
        // `cx.add_timer` path (see `lens` comment above).
        #[cfg(target_os = "macos")]
        {
            self.reanchor_parent
                .store(parent_for_reanchor, Ordering::Relaxed);
        }
        let last_param_poll = Arc::clone(&last_param_poll);
        #[cfg(target_os = "macos")]
        let reanchor = Arc::clone(&self.reanchor_parent);
        let app = app.on_idle(move |_cx| {
            let now = Instant::now();
            let due = last_param_poll
                .lock()
                .map(|mut last| {
                    if now.duration_since(*last) >= Duration::from_millis(33) {
                        *last = now;
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if due {
                lens_for_idle.refresh_params();
                lens_for_idle.refresh_meters();
            }
            #[cfg(target_os = "macos")]
            {
                let ptr = reanchor.load(Ordering::Relaxed);
                if ptr != 0 {
                    truce_gui_utils::reanchor_all_children_to_top(ptr as *mut std::ffi::c_void);
                }
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
        if let Some(mut window) = self.window.take() {
            window.close();
        }
    }

    fn idle(&mut self) {
        // vizia drives its own event loop via baseview - no idle
        // tick needed from truce.
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
        if let Some(mut window) = self.window.take() {
            window.close();
        }
    }
}
