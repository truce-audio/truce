//! Gesture tracking wrapper for drag-based parameter widgets.
//!
//! Wraps a child view (knob, slider) and emits `BeginEdit` on mouse
//! down and `EndEdit` on mouse up. The child's `on_change` callback
//! should emit `SetNormalized` (not `SetImmediate`) during the drag.

use vizia::prelude::*;

use crate::param_model::ParamEvent;

/// Transparent wrapper that tracks drag gestures and emits
/// `BeginEdit`/`EndEdit` events for proper automation recording.
///
/// Vizia's built-in Knob and Slider call `cx.capture()` on mouse
/// down, so descendants don't receive subsequent events. But events
/// DO propagate up through ancestors, so this wrapper (as a parent)
/// receives both MouseDown and MouseUp.
pub struct GestureWrapper {
    param_id: u32,
    editing: bool,
}

impl GestureWrapper {
    pub fn new<F>(cx: &mut Context, param_id: impl Into<u32>, content: F) -> Handle<'_, Self>
    where
        F: FnOnce(&mut Context),
    {
        let param_id = param_id.into();
        Self {
            param_id,
            editing: false,
        }
        .build(cx, |cx| {
            content(cx);
        })
    }
}

impl View for GestureWrapper {
    fn event(&mut self, cx: &mut EventContext, event: &mut vizia::events::Event) {
        event.map(|window_event, _| match window_event {
            WindowEvent::MouseDown(MouseButton::Left) => {
                if !self.editing {
                    self.editing = true;
                    cx.emit(ParamEvent::BeginEdit(self.param_id));
                }
            }
            WindowEvent::MouseUp(MouseButton::Left) => {
                if self.editing {
                    self.editing = false;
                    cx.emit(ParamEvent::EndEdit(self.param_id));
                }
            }
            _ => {}
        });
    }
}
