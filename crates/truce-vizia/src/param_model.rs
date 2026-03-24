//! Vizia Model for parameter state and host communication.
//!
//! `ParamModel` is built into the vizia context tree on editor open.
//! Widgets interact with parameters by emitting `ParamEvent` events,
//! which this model handles by calling the appropriate `EditorContext`
//! callbacks (begin_edit / set_param / end_edit).

use vizia::prelude::*;

use truce_core::editor::EditorContext;

/// Events emitted by parameter widgets. `ParamModel` handles these
/// automatically — widgets just emit and forget.
#[derive(Debug, Clone)]
pub enum ParamEvent {
    /// Begin a parameter edit gesture (mouse-down on a control).
    BeginEdit(u32),
    /// Set a parameter's normalized value during a gesture.
    SetNormalized(u32, f64),
    /// End a parameter edit gesture (mouse-up).
    EndEdit(u32),
    /// Begin + set + end in one shot (click toggles, selectors).
    SetImmediate(u32, f64),
    /// Internal: host signaled new parameter values. Views should
    /// re-read any cached values.
    Sync,
}

/// Vizia model that holds the `EditorContext` and handles `ParamEvent`s.
///
/// Built into the vizia context tree by `ViziaEditor::open()`. Widgets
/// higher in the tree can emit `ParamEvent` and this model will route
/// them to the host.
pub struct ParamModel {
    context: EditorContext,
}

impl ParamModel {
    pub fn new(context: EditorContext) -> Self {
        Self { context }
    }

    /// Read a parameter's normalized value (0.0–1.0).
    pub fn get(&self, id: impl Into<u32>) -> f64 {
        (self.context.get_param)(id.into())
    }

    /// Read a parameter's plain value (native range).
    pub fn get_plain(&self, id: impl Into<u32>) -> f64 {
        (self.context.get_param_plain)(id.into())
    }

    /// Read a parameter's formatted display string.
    pub fn format(&self, id: impl Into<u32>) -> String {
        (self.context.format_param)(id.into())
    }

    /// Read a meter value (0.0–1.0).
    pub fn meter(&self, id: impl Into<u32>) -> f32 {
        (self.context.get_meter)(id.into())
    }
}

impl Model for ParamModel {
    fn event(&mut self, _cx: &mut EventContext, event: &mut vizia::events::Event) {
        event.map(|param_event, _| match param_event {
            ParamEvent::BeginEdit(id) => {
                (self.context.begin_edit)(*id);
            }
            ParamEvent::SetNormalized(id, val) => {
                (self.context.set_param)(*id, *val);
            }
            ParamEvent::EndEdit(id) => {
                (self.context.end_edit)(*id);
            }
            ParamEvent::SetImmediate(id, val) => {
                (self.context.begin_edit)(*id);
                (self.context.set_param)(*id, *val);
                (self.context.end_edit)(*id);
            }
            ParamEvent::Sync => {
                // Views that cache values should re-read them.
                // The event propagates to the subtree; individual
                // widgets handle it in their own event() impl.
            }
        });
    }
}
