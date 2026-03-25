//! Parameter state bridge between truce's EditorContext and Slint UI.
//!
//! Wraps the `begin_edit` / `set_param` / `end_edit` host protocol into
//! ergonomic accessors. Clone-able so Slint callbacks can capture it.

use truce_core::editor::EditorContext;

/// Bridge between truce's EditorContext and Slint widgets.
///
/// Provides read/write access to parameter values with proper automation
/// gesture handling (`begin_edit` / `set_param` / `end_edit`).
///
/// Cloneable — all fields are `Arc`-wrapped, so cloning is a ref-count bump.
#[derive(Clone)]
pub struct ParamState {
    ctx: EditorContext,
}

impl ParamState {
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

    /// Begin + set + end in one shot (for clicks/toggles/sliders).
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

    /// Create a mock ParamState for testing. All params return defaults.
    pub fn mock() -> Self {
        use std::sync::Arc;
        Self {
            ctx: EditorContext {
                begin_edit: Arc::new(|_| {}),
                set_param: Arc::new(|_, _| {}),
                end_edit: Arc::new(|_| {}),
                request_resize: Arc::new(|_, _| false),
                get_param: Arc::new(|_| 0.5),
                get_param_plain: Arc::new(|_| 0.0),
                format_param: Arc::new(|id| format!("p{id}")),
                get_meter: Arc::new(|_| 0.0),
            },
        }
    }
}
