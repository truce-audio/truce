//! Ergonomic wrapper around `EditorContext` for use in `IcedPlugin::update()`.

use truce_core::editor::EditorContext;

/// Wraps `EditorContext` callbacks for ergonomic parameter editing.
pub struct EditorHandle {
    ctx: EditorContext,
}

impl EditorHandle {
    pub(crate) fn new(ctx: EditorContext) -> Self {
        Self { ctx }
    }

    /// Begin a parameter edit gesture (call before dragging).
    pub fn begin_edit(&self, id: u32) {
        (self.ctx.begin_edit)(id);
    }

    /// Set a parameter's normalized value and notify the host.
    pub fn set_param(&self, id: u32, value: f64) {
        (self.ctx.set_param)(id, value);
    }

    /// End a parameter edit gesture.
    pub fn end_edit(&self, id: u32) {
        (self.ctx.end_edit)(id);
    }

    /// Request window resize from the host.
    pub fn request_resize(&self, w: u32, h: u32) -> bool {
        (self.ctx.request_resize)(w, h)
    }

    pub(crate) fn context(&self) -> &EditorContext {
        &self.ctx
    }
}
