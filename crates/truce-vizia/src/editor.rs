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
}

impl<P: Params + 'static> Editor for ViziaEditor<P> {
    fn size(&self) -> (u32, u32) {
        self.size
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

        let parent_wrapper = ParentWindow(parent);

        let app = Application::new(move |cx| {
            // Register the embedded font, if any, before any view
            // builds - vizia caches font shaping per family.
            if let Some(bytes) = font {
                cx.add_font_mem(bytes);
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
