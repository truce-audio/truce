//! ViziaEditor: implements `truce_core::Editor` using vizia + baseview.
//!
//! On `open()`, creates a vizia `Application` and calls `open_parented()`
//! to embed it as a child of the host's parent window. Vizia owns the
//! event loop, rendering (Skia/GL), and input handling internally.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use baseview::WindowHandle;
use raw_window_handle::{HasRawWindowHandle, RawWindowHandle as RwhRawWindowHandle};
use vizia::prelude::*;

use truce_core::editor::{Editor, EditorContext, RawWindowHandle};

use crate::param_model::ParamModel;
use crate::theme;

/// Vizia-based editor implementing truce's `Editor` trait.
///
/// Owns the vizia application and baseview window handle. On `open()`,
/// creates a child window via `open_parented()`. On `close()`, drops
/// the window handle which tears down the vizia context.
pub struct ViziaEditor {
    size: (u32, u32),
    app: Arc<dyn Fn(&mut Context) + Send + Sync>,
    scale_factor: Option<f64>,
    /// Active baseview window handle — exists only while editor is open.
    window: Option<WindowHandle>,
    /// Shared flag: set true by `idle()`, cleared by `on_idle` callback.
    /// Triggers parameter re-reads in the vizia UI.
    params_changed: Arc<AtomicBool>,
}

// WindowHandle contains raw pointers; only accessed from host UI thread.
unsafe impl Send for ViziaEditor {}

impl ViziaEditor {
    /// Create a vizia editor with a closure-based UI.
    ///
    /// The closure receives a vizia `Context` and should build the view
    /// tree. `ParamModel` is automatically registered before your closure
    /// runs, so widgets can emit `ParamEvent` immediately.
    ///
    /// `size` is the initial window size in physical pixels (matching
    /// `Editor::size()` convention).
    pub fn new(
        size: (u32, u32),
        app: impl Fn(&mut Context) + Send + Sync + 'static,
    ) -> Self {
        Self {
            size,
            app: Arc::new(app),
            scale_factor: None,
            window: None,
            params_changed: Arc::new(AtomicBool::new(false)),
        }
    }
}

// ---------------------------------------------------------------------------
// Parent window handle bridge
// ---------------------------------------------------------------------------

/// Newtype bridging truce's `RawWindowHandle` to baseview's
/// `HasRawWindowHandle` (raw-window-handle 0.5).
struct ParentWindow(RawWindowHandle);

unsafe impl HasRawWindowHandle for ParentWindow {
    fn raw_window_handle(&self) -> RwhRawWindowHandle {
        match self.0 {
            RawWindowHandle::AppKit(ptr) => {
                let mut handle = raw_window_handle::AppKitWindowHandle::empty();
                handle.ns_view = ptr;
                RwhRawWindowHandle::AppKit(handle)
            }
            RawWindowHandle::Win32(ptr) => {
                let mut handle = raw_window_handle::Win32WindowHandle::empty();
                handle.hwnd = ptr;
                RwhRawWindowHandle::Win32(handle)
            }
            RawWindowHandle::X11(window_id) => {
                let mut handle = raw_window_handle::XlibWindowHandle::empty();
                handle.window = window_id;
                RwhRawWindowHandle::Xlib(handle)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Scale factor helpers
// ---------------------------------------------------------------------------

/// Query the backing scale factor from the parent NSView's window,
/// or fall back to the main screen's scale factor.
#[cfg(target_os = "macos")]
fn query_backing_scale(parent: &RawWindowHandle) -> f64 {
    use objc::{msg_send, sel, sel_impl};

    let ns_view_ptr = match parent {
        RawWindowHandle::AppKit(ptr) => *ptr,
        _ => return 1.0,
    };

    if ns_view_ptr.is_null() {
        return 1.0;
    }

    unsafe {
        let ns_view = ns_view_ptr as cocoa::base::id;
        let window: cocoa::base::id = msg_send![ns_view, window];
        let scale: f64 = if !window.is_null() {
            msg_send![window, backingScaleFactor]
        } else {
            let screen: cocoa::base::id = msg_send![objc::class!(NSScreen), mainScreen];
            if !screen.is_null() {
                msg_send![screen, backingScaleFactor]
            } else {
                2.0 // Safe default for macOS
            }
        };
        if scale < 1.0 { 1.0 } else { scale }
    }
}

#[cfg(not(target_os = "macos"))]
fn query_backing_scale(_parent: &RawWindowHandle) -> f64 {
    1.0
}

// ---------------------------------------------------------------------------
// Editor trait implementation
// ---------------------------------------------------------------------------

impl Editor for ViziaEditor {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn open(&mut self, parent: RawWindowHandle, context: EditorContext) {
        let app = self.app.clone();
        let params_changed = self.params_changed.clone();
        let context = Arc::new(context);

        // Editor::size() returns logical points (from GridLayout::compute_size).
        // Baseview's inner_size() also expects logical points — pass through.
        let (logical_w, logical_h) = self.size;

        let application = Application::new(move |cx| {
            // Apply the default dark theme.
            theme::apply_default_theme(cx);

            // Register the parameter model so widgets can emit ParamEvent.
            ParamModel::new((*context).clone()).build(cx);

            // Build the user's UI.
            app(cx);
        })
        .inner_size((logical_w, logical_h))
        // Let baseview detect the actual scale from the system. This
        // avoids double-scaling issues when the Skia/GL surface size
        // is derived from the NSView's backingScaleFactor at runtime.
        .with_scale_policy(baseview::WindowScalePolicy::SystemScaleFactor)
        .on_idle({
            let params_changed = params_changed.clone();
            move |cx| {
                if params_changed
                    .compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    cx.emit_custom(
                        Event::new(crate::param_model::ParamEvent::Sync)
                            .propagate(Propagation::Subtree),
                    );
                }
            }
        });

        let parent_wrapper = ParentWindow(parent);
        let window = application.open_parented(&parent_wrapper);

        self.window = Some(window);
    }

    fn close(&mut self) {
        if let Some(mut window) = self.window.take() {
            window.close();
        }
    }

    fn idle(&mut self) {
        // Signal that the host has new parameter values.
        // The on_idle callback in vizia will pick this up next frame.
        self.params_changed.store(true, Ordering::Relaxed);
    }

    fn can_resize(&self) -> bool {
        true
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        self.size = (width, height);
        true
    }

    fn set_scale_factor(&mut self, factor: f64) {
        self.scale_factor = Some(factor);
    }
}
