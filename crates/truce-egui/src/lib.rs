//! egui-based GUI backend for truce audio plugins.
//!
//! Provides `EguiEditor`, an implementation of `truce_core::Editor` that
//! renders using egui's immediate-mode UI via egui-wgpu. Gives plugin
//! developers access to egui's full widget library, layout system, and
//! ecosystem while retaining truce's parameter binding and host integration.
//!
//! # Quick Start
//!
//! ```ignore
//! use truce_egui::EguiEditor;
//! use truce_egui::widgets::{param_knob, param_slider};
//! use truce_core::editor::PluginContext;
//!
//! let editor = EguiEditor::new(params, (800, 600), |ui: &mut egui::Ui, state: &PluginContext<MyParams>| {
//!     ui.heading("My Plugin");
//!     param_slider(ui, state, 0u32);
//! });
//! ```

// `editor.rs` is the baseview-driven desktop path; `editor_ios.rs`
// drives the UIView + CADisplayLink + CAMetalLayer host on iOS.
// `renderer.rs` (egui-wgpu wrapper) is shared - it has both a
// baseview-window and a raw-CAMetalLayer constructor.
mod claims;
#[cfg(not(target_os = "ios"))]
pub mod editor;
pub mod font;
pub mod platform;
#[cfg(target_os = "windows")]
mod render_thread;
pub mod renderer;
#[cfg(not(target_os = "ios"))]
mod screenshot;
pub mod theme;
pub mod widgets;

#[cfg(target_os = "ios")]
mod editor_ios;

pub use claims::{claim_keys, claim_shortcuts};
#[cfg(not(target_os = "ios"))]
pub use editor::{EditorUi, EguiEditor};

#[cfg(target_os = "ios")]
pub use editor_ios::{EditorUi, EguiEditor};

/// Run author UI code inside an egui `run_ui` closure, catching a panic so
/// egui still finishes its pass. Letting it unwind out of `run_ui` is
/// worse than a lost frame: egui 0.37 leaves the root `Ui` borrowed, so
/// every later `run_ui` panics and the editor is dead for good; and if an
/// earlier pass of the same frame requested a discard (an `egui::Grid`'s
/// first frame does), egui's own `FullOutput` unwinds holding that pass's
/// texture uploads and epaint's drop assert fires mid-unwind - an abort
/// (debug builds). Returns the panic payload for the caller to log, or to
/// re-raise once the frame's output is safely owned.
pub(crate) fn catch_ui_panic(f: impl FnOnce()) -> Option<Box<dyn std::any::Any + Send>> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).err()
}

/// The message of a caught panic payload.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// A fresh egui context for a live editor. egui's keyboard zoom
/// (Cmd/Ctrl with `+` / `-` / `=` / `0`) is off: it rescales the UI's
/// points-per-pixel inside a window whose size the host owns, so the
/// content clips or offsets until reset - and those chords belong to the
/// host anyway unless the editor claims them.
pub(crate) fn new_egui_context() -> egui::Context {
    let ctx = egui::Context::default();
    ctx.options_mut(|o| o.zoom_with_keyboard = false);
    ctx
}

#[cfg(test)]
mod ui_panic_tests {
    /// An author-UI panic on egui's second pass (the first requested a
    /// discard, as a `Grid`'s first frame does) must neither abort in
    /// epaint's drop assert nor leave the context's root `Ui` borrowed:
    /// the next frame runs normally.
    #[test]
    fn second_pass_ui_panic_neither_aborts_nor_kills_the_context() {
        let ctx = crate::new_egui_context();
        crate::font::apply_font(&ctx, truce_font::JETBRAINS_MONO);
        let mut passes = 0;
        for frame in 0..2 {
            let mut ui_panic = None;
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                if let Some(p) = crate::catch_ui_panic(|| {
                    passes += 1;
                    ui.label("x");
                    if frame == 0 {
                        if passes == 1 {
                            ui.ctx().request_discard("test: force a second pass");
                        } else {
                            panic!("author ui panicked on the second pass");
                        }
                    }
                }) {
                    ui_panic.get_or_insert(p);
                }
            });
            output.textures_delta.clear();
            assert_eq!(ui_panic.is_some(), frame == 0, "frame {frame}");
        }
        assert!(passes >= 3, "frame 0 ran two passes, frame 1 at least one");
    }
}
