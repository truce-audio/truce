//! The PluginLogic trait — the single trait plugin developers implement.
//!
//! Construction (`new()`) is an inherent method on each plugin struct,
//! not part of this trait. The `plugin!` macro calls it with
//! `Arc<Params>` so the plugin shares params with the shell and GUI.

use truce_core::buffer::AudioBuffer;
use truce_core::events::EventList;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_gui::interaction::WidgetRegion;
use truce_gui::render::RenderBackend;
use truce_gui::widgets::WidgetType;

/// The trait for hot-reloadable plugin logic.
///
/// Implement this on your plugin struct. Construction happens via an
/// inherent `new(params: Arc<YourParams>)` method — the `plugin!` macro
/// calls it and passes the shared `Arc<Params>`.
///
/// All methods use safe Rust types. No `unsafe`, no `#[repr(C)]`,
/// no raw pointers.
pub trait PluginLogic: Send + 'static {

    /// Reset for a new sample rate / block size.
    fn reset(&mut self, sample_rate: f64, max_block_size: usize);

    /// Process one block of audio.
    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus;

    /// Render the GUI into the backend.
    ///
    /// Default: no-op. The shell uses BuiltinEditor with the layout
    /// from `layout()` to draw standard widgets automatically.
    /// Override only for custom visuals.
    fn render(&self, _backend: &mut dyn RenderBackend) {}

    /// Whether this plugin uses a custom render() implementation.
    /// If false (default), the shell uses BuiltinEditor with
    /// standard widget drawing from layout().
    fn uses_custom_render(&self) -> bool { false }

    /// Return the widget layout.
    ///
    /// Use `GridLayout::build()` for the layout. Widgets auto-flow
    /// left-to-right. Use `.cols(n)` and `.rows(n)` for spanning.
    fn layout(&self) -> truce_gui::layout::GridLayout {
        truce_gui::layout::GridLayout::build("", "", 1, 80.0, vec![])
    }

    /// Hit test: which widget (if any) is at (x, y)?
    fn hit_test(&self, widgets: &[WidgetRegion], x: f32, y: f32) -> Option<usize> {
        default_hit_test(widgets, x, y)
    }

    /// Serialize plugin-specific state (DSP state, not params).
    fn save_state(&self) -> Vec<u8> { Vec::new() }

    /// Restore plugin-specific state.
    fn load_state(&mut self, _data: &[u8]) {}

    /// Report latency in samples.
    fn latency(&self) -> u32 { 0 }

    /// Report tail time in samples.
    fn tail(&self) -> u32 { 0 }

    /// Provide a custom editor instead of the built-in widget layout.
    ///
    /// Return `Some(editor)` to use a custom `Editor` implementation
    /// (e.g., `truce_egui::EguiEditor`). The shell calls this first;
    /// if it returns `None`, the shell falls back to creating a
    /// `BuiltinEditor` from `layout()`.
    fn custom_editor(&self) -> Option<Box<dyn truce_core::editor::Editor>> { None }
}

/// Default hit test: circular for knobs, rectangular for others,
/// skip meters.
pub fn default_hit_test(widgets: &[WidgetRegion], x: f32, y: f32) -> Option<usize> {
    for (i, w) in widgets.iter().enumerate() {
        if w.widget_type == WidgetType::Meter { continue; }
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
