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

    /// Access the underlying EditorContext (for `StateBinding::new()`).
    pub fn context(&self) -> &EditorContext {
        &self.ctx
    }

    /// Read custom plugin state bytes (from `save_state()`).
    pub fn get_state(&self) -> Vec<u8> {
        (self.ctx.get_state)()
    }

    /// Write custom state back to the plugin (calls `load_state()`).
    pub fn set_state(&self, data: Vec<u8>) {
        (self.ctx.set_state)(data);
    }
}
