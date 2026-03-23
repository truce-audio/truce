//! Standalone host for truce plugins.
//!
//! Provides audio output via cpal and QWERTY keyboard-to-MIDI.
//! With the `gui` feature, opens a window with rendered parameter knobs.

pub mod keyboard;
pub mod runner;

#[cfg(feature = "gui")]
pub mod windowed;

pub use runner::run;
pub use truce_core::export::PluginExport;

/// Re-export for backward compatibility.
pub use truce_core::export::PluginExport as StandaloneExport;

/// Render the plugin GUI to a PNG file.
#[cfg(feature = "gui")]
pub fn render_gui_png<P: truce_params::Params + 'static>(
    params: std::sync::Arc<P>,
    layout: truce_gui::layout::PluginLayout,
    path: &str,
) {
    let mut editor = truce_gui::BuiltinEditor::new(params, layout);
    let pixmap = editor.render();
    pixmap.save_png(path).expect("Failed to save GUI PNG");
    println!("GUI rendered to {path}");
}

/// Run the plugin standalone with a GUI window.
#[cfg(feature = "gui")]
pub fn run_with_gui<P: PluginExport>(layout: truce_gui::layout::PluginLayout)
where
    P::Params: 'static,
{
    windowed::run_windowed::<P>(layout);
}
