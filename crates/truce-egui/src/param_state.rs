//! Parameter state bridge between truce's EditorContext and egui widgets.
//!
//! Wraps the `begin_edit` / `set_param` / `end_edit` host protocol into
//! ergonomic accessors that egui widgets can call during a frame.

use truce_core::editor::EditorContext;

/// Bridge between truce's EditorContext and egui widgets.
///
/// Provides read/write access to parameter values with proper automation
/// gesture handling (`begin_edit` / `set_param` / `end_edit`).
pub struct ParamState {
    ctx: EditorContext,
}

impl ParamState {
    /// Create a new ParamState wrapping an EditorContext.
    pub fn new(ctx: EditorContext) -> Self {
        Self { ctx }
    }

    /// Get a parameter's normalized value (0.0-1.0).
    pub fn get(&self, id: impl Into<u32>) -> f64 {
        let id = id.into();
        (self.ctx.get_param)(id)
    }

    /// Get a parameter's plain value (in its native range).
    pub fn get_plain(&self, id: impl Into<u32>) -> f64 {
        let id = id.into();
        (self.ctx.get_param_plain)(id)
    }

    /// Get a parameter's formatted display string.
    pub fn format(&self, id: impl Into<u32>) -> String {
        let id = id.into();
        (self.ctx.format_param)(id)
    }

    /// Begin + set + end in one shot (for clicks/toggles).
    pub fn set_immediate(&self, id: impl Into<u32>, normalized: f64) {
        let id = id.into();
        (self.ctx.begin_edit)(id);
        (self.ctx.set_param)(id, normalized);
        (self.ctx.end_edit)(id);
    }

    /// Begin a drag gesture (call once on mouse-down).
    pub fn begin_gesture(&self, id: impl Into<u32>) {
        let id = id.into();
        (self.ctx.begin_edit)(id);
    }

    /// Update during a drag gesture.
    pub fn set_value(&self, id: impl Into<u32>, normalized: f64) {
        let id = id.into();
        (self.ctx.set_param)(id, normalized);
    }

    /// End a drag gesture (call once on mouse-up).
    pub fn end_gesture(&self, id: impl Into<u32>) {
        let id = id.into();
        (self.ctx.end_edit)(id);
    }

    /// Read a meter value (0.0-1.0) by meter ID.
    pub fn meter(&self, id: impl Into<u32>) -> f32 {
        let id = id.into();
        (self.ctx.get_meter)(id)
    }

    /// Request a resize from the host. Returns true if accepted.
    pub fn request_resize(&self, width: u32, height: u32) -> bool {
        (self.ctx.request_resize)(width, height)
    }

    /// Create a ParamState backed by real parameter defaults.
    /// Uses `P::default_for_gui()` to provide accurate formatting and values.
    pub fn from_params<P: truce_params::Params + 'static>(params: std::sync::Arc<P>) -> Self {
        let p1 = params.clone();
        let p2 = params.clone();
        let p3 = params.clone();
        Self {
            ctx: EditorContext {
                begin_edit: std::sync::Arc::new(|_| {}),
                set_param: std::sync::Arc::new(|_, _| {}),
                end_edit: std::sync::Arc::new(|_| {}),
                request_resize: std::sync::Arc::new(|_, _| false),
                get_param: std::sync::Arc::new(move |id| p1.get_normalized(id).unwrap_or(0.5)),
                get_param_plain: std::sync::Arc::new(move |id| p2.get_plain(id).unwrap_or(0.0)),
                format_param: std::sync::Arc::new(move |id| {
                    let plain = p3.get_plain(id).unwrap_or(0.0);
                    p3.format_value(id, plain).unwrap_or_else(|| format!("{plain:.2}"))
                }),
                get_meter: std::sync::Arc::new(|_| 0.0),
                get_state: std::sync::Arc::new(|| Vec::new()),
                set_state: std::sync::Arc::new(|_| {}),
            },
        }
    }
}
