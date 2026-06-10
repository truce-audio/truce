//! `ViziaEditor` - the `truce_core::editor::Editor` impl that wraps
//! a `vizia::Application` mounted via `baseview-truce` onto the
//! DAW-provided parent window.

use std::sync::Arc;

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
    window: Option<vizia::WindowHandle>,
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
            window: None,
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
            let lens = ParamLens::new(typed_ctx.clone());

            // Run the user's setup *first* so the view tree is built
            // (and meter widgets have registered their signals via
            // `lens.meter_signal(id)`) before the polling timer can
            // fire. Calling `start_timer` ahead of `(setup)` raced
            // against the initial style / layout / draw passes and
            // sometimes left the standalone window grey on first
            // paint.
            (setup)(cx, lens.clone());

            // Single root timer drives every `level_meter` widget.
            // The tick callback fans the latest store values into
            // every registered meter signal once per frame; vizia's
            // reactive graph then re-evaluates the Memos driving the
            // bar heights. ~30Hz is plenty for visible motion and
            // cheaper than a render-rate tick.
            let lens_for_timer = lens;
            let timer = cx.add_timer(
                std::time::Duration::from_millis(33),
                None,
                move |_ev, action| {
                    if matches!(action, TimerAction::Tick(_)) {
                        lens_for_timer.refresh_meters();
                    }
                },
            );
            cx.start_timer(timer);
        })
        .inner_size((lw, lh));

        // Per-frame re-anchor on macOS: pin every child NSView of the
        // host's plug-in pane to the parent's top so a canvas that
        // grows under host/user resize doesn't drift its top edge
        // above the visible area (clipping the editor's header /
        // first row). No-op on other platforms.
        #[cfg(target_os = "macos")]
        let app = {
            let ptr = parent_for_reanchor;
            app.on_idle(move |_cx| {
                truce_gui_utils::reanchor_all_children_to_top(
                    ptr as *mut std::ffi::c_void,
                );
            })
        };

        let window = app.open_parented(&parent_wrapper);
        self.window = Some(window);
    }

    fn close(&mut self) {
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
        if let Some(mut window) = self.window.take() {
            window.close();
        }
    }
}
