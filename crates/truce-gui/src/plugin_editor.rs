//! The GUI surface every plugin implements alongside
//! [`truce_core::PluginLogic`].
//!
//! All methods carry default impls so a headless plugin's impl
//! block is one line: `impl PluginEditor for MyPlugin {}`. The
//! `truce::plugin!` macro bridges this trait plus
//! [`truce_core::PluginLogic`] into [`truce_core::Plugin`] for
//! format wrappers.

use truce_core::editor::Editor;

use crate::interaction::WidgetRegion;
use crate::layout::GridLayout;
use crate::render::RenderBackend;
use crate::widgets::WidgetType;

/// The GUI surface every plugin implements. All methods default,
/// so headless plugins write `impl PluginEditor for MyPlugin {}`.
pub trait PluginEditor {
    /// Return the widget layout for the built-in GUI. Default:
    /// empty layout (`GridLayout::build(vec![])`). Plugins that
    /// supply a custom editor via [`Self::custom_editor`] can
    /// leave this default — the format wrappers prefer the
    /// custom editor when present.
    fn layout(&self) -> GridLayout {
        GridLayout::build(vec![])
    }

    /// Render the GUI into a backend. Default: no-op. Override
    /// only for custom GPU/CPU rasterisation outside the standard
    /// widget set; flip [`Self::uses_custom_render`] to `true`
    /// when you do.
    fn render(&self, _backend: &mut dyn RenderBackend) {}

    /// Whether this plugin overrides [`Self::render`]. Default:
    /// `false`. The shell uses [`crate::BuiltinEditor`] with
    /// standard widget drawing from [`Self::layout`] when this is
    /// `false`.
    fn uses_custom_render(&self) -> bool {
        false
    }

    /// Hit test: which widget (if any) is at `(x, y)`? Default:
    /// the standard hit-test (circular for knobs, rectangular
    /// for others, meters skipped).
    fn hit_test(&self, widgets: &[WidgetRegion], x: f32, y: f32) -> Option<usize> {
        default_hit_test(widgets, x, y)
    }

    /// Provide a custom [`Editor`] instead of the built-in widget
    /// layout (egui, iced, slint, raw window handle). Default:
    /// `None`. The shell calls this first; if it returns `None`,
    /// the shell falls back to creating a [`crate::BuiltinEditor`]
    /// from [`Self::layout`].
    fn custom_editor(&self) -> Option<Box<dyn Editor>> {
        None
    }
}

/// Default hit test: circular for knobs, rectangular for
/// everything else, skip meters. Used by
/// [`PluginEditor::hit_test`]'s default impl.
#[must_use]
pub fn default_hit_test(widgets: &[WidgetRegion], x: f32, y: f32) -> Option<usize> {
    for (i, w) in widgets.iter().enumerate() {
        if w.widget_type == WidgetType::Meter {
            continue;
        }
        if w.widget_type == WidgetType::Knob {
            let dx = x - w.cx;
            let dy = y - w.cy;
            if dx * dx + dy * dy <= w.radius * w.radius {
                return Some(i);
            }
        } else if x >= w.x && x <= w.x + w.w && y >= w.y && y <= w.y + w.h {
            return Some(i);
        }
    }
    None
}
