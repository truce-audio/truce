//! `BaseviewTranslator` — the windowing-toolkit-specific half of
//! `truce-gui`'s interaction surface. The platform-agnostic data
//! types (`InputEvent`, `MouseButton`, `Modifiers`, `WidgetRegion`,
//! `InteractionState`, `DragState`, `DropdownState`, `dispatch`, …)
//! live in [`truce_gui_types::interaction`] and are re-exported here
//! so existing `truce_gui::interaction::*` paths keep working.

pub use truce_gui_types::interaction::*;

const DOUBLE_CLICK_MS: u128 = 300;
const DOUBLE_CLICK_SLOP: f32 = 4.0;
const WHEEL_LINE_PX: f32 = 20.0;

/// Stateful translator from baseview events to truce-gui's
/// platform-agnostic [`InputEvent`] stream.
///
/// Exists because baseview emits logical-point mouse positions on every
/// platform (macOS via Cocoa points; X11 and Windows via explicit
/// `to_logical`) but does not carry a position on `ButtonPressed` /
/// `ButtonReleased` nor synthesize double-clicks.
///
/// Emitted `InputEvent`s carry **logical** coordinates unchanged from
/// baseview. The rendering backend (e.g. `WgpuBackend`) handles the
/// logical→physical conversion at raster time; callers must not
/// pre-multiply by `scale`.
// All fields share a `last_` prefix because the struct's whole purpose
// is to remember the previous cursor / click — the prefix is meaningful,
// not redundant.
#[allow(clippy::struct_field_names)]
#[derive(Default)]
pub struct BaseviewTranslator {
    last_cursor: (f32, f32),
    last_click_time: Option<std::time::Instant>,
    last_click_pos: (f32, f32),
}

impl BaseviewTranslator {
    /// The last cursor position we saw from a `CursorMoved`, in logical
    /// points. Useful when a caller needs to query cursor state outside
    /// the event stream (e.g. for its own overlays).
    #[must_use]
    pub fn last_cursor(&self) -> (f32, f32) {
        self.last_cursor
    }

    /// Convert a baseview event into an [`InputEvent`]. Returns `None`
    /// for events truce-gui doesn't consume (keyboard, non-L/R/M mouse
    /// buttons, window lifecycle).
    pub fn translate(&mut self, event: &baseview::Event) -> Option<InputEvent> {
        let baseview::Event::Mouse(m) = event else {
            return None;
        };
        match m {
            baseview::MouseEvent::CursorMoved { position, .. } => {
                // baseview reports cursor in f64 logical points; the
                // hit-test math is f32. Window dimensions never reach
                // 2^23, so the narrowing is invisible.
                #[allow(clippy::cast_possible_truncation)]
                let x = position.x as f32;
                #[allow(clippy::cast_possible_truncation)]
                let y = position.y as f32;
                self.last_cursor = (x, y);
                Some(InputEvent::MouseMove { x, y })
            }
            baseview::MouseEvent::ButtonPressed { button, .. } => {
                let mb = map_button(*button)?;
                let (x, y) = self.last_cursor;
                if mb == MouseButton::Left {
                    let now = std::time::Instant::now();
                    let is_double = self.last_click_time.is_some_and(|t| {
                        now.duration_since(t).as_millis() < DOUBLE_CLICK_MS
                            && (x - self.last_click_pos.0).abs() < DOUBLE_CLICK_SLOP
                            && (y - self.last_click_pos.1).abs() < DOUBLE_CLICK_SLOP
                    });
                    self.last_click_time = Some(now);
                    self.last_click_pos = (x, y);
                    if is_double {
                        self.last_click_time = None;
                        return Some(InputEvent::MouseDoubleClick { x, y });
                    }
                }
                Some(InputEvent::MouseDown { x, y, button: mb })
            }
            baseview::MouseEvent::ButtonReleased { button, .. } => {
                let mb = map_button(*button)?;
                let (x, y) = self.last_cursor;
                Some(InputEvent::MouseUp { x, y, button: mb })
            }
            baseview::MouseEvent::WheelScrolled { delta, .. } => {
                let dy = match delta {
                    baseview::ScrollDelta::Lines { y, .. } => y * WHEEL_LINE_PX,
                    baseview::ScrollDelta::Pixels { y, .. } => *y,
                };
                let (x, y) = self.last_cursor;
                Some(InputEvent::Scroll { x, y, dy })
            }
            baseview::MouseEvent::CursorLeft => Some(InputEvent::MouseLeave),
            _ => None,
        }
    }
}

fn map_button(b: baseview::MouseButton) -> Option<MouseButton> {
    match b {
        baseview::MouseButton::Left => Some(MouseButton::Left),
        baseview::MouseButton::Right => Some(MouseButton::Right),
        baseview::MouseButton::Middle => Some(MouseButton::Middle),
        _ => None,
    }
}
