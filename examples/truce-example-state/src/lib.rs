//! Hello world for `#[derive(State)]`.
//!
//! Strings can't be parameters (the param system stores numeric atoms),
//! so the user's per-instance label lives in a separate state struct
//! that the framework serialises alongside the parameter envelope.
//! The plugin does nothing to the audio - it's a pass-through whose
//! only job is to demonstrate `save_state` / `load_state` end-to-end.

use std::sync::Arc;
use truce::prelude::*;
use truce_core::custom_state::{State as StateTrait, StateBinding};
use truce_core::editor::PluginContext;
use truce_egui::theme::{HEADER_BG, HEADER_TEXT};
use truce_egui::{EditorUi, EguiEditor};
use truce_gui::font;

const WINDOW_W: u32 = 320;
const WINDOW_H: u32 = 120;

// --- Parameters ---

/// Use `Param` for values the host should treat as a control: gain,
/// frequency, mix, bypass, mode selectors. Hosts list params in
/// their automation editor, draw automation lanes against them,
/// record undo entries on every change, and feed them through MIDI
/// CC / OSC mappings. They're numeric atoms (`f32` / `f64` / `bool`
/// / int / enum-as-index) read lock-free from the audio thread.
///
/// Use `derive(State)` for everything that isn't a numeric atom:
/// strings, file paths, loaded sample buffers, lists, view modes,
/// nested structs. State is opaque to the host - no automation, no
/// CC mapping, no UI in the host's parameter list - the framework
/// just round-trips the bytes you hand it via `save_state` /
/// `load_state` (see [`InstanceMemo`] below).
///
/// In this example: `Active` is a bool the user might want to
/// automate, so it's a param. The instance label is plain text,
/// so it's state.
#[derive(Params)]
pub struct StateExampleParams {
    #[param(name = "Active", default = 1)]
    pub active: BoolParam,
}

// --- Persistent extra state ---

#[derive(State, Default, Clone)]
pub struct InstanceMemo {
    /// User-typed label for this plugin instance. Persists across
    /// session save/load and preset recall.
    pub label: String,
}

// --- Plugin ---

pub struct StateExample {
    params: Arc<StateExampleParams>,
    memo: InstanceMemo,
    /// Runtime counter - how many times the host has restored
    /// state on this instance (preset recall, undo, session
    /// load). Lives on the plugin struct, *not* in `InstanceMemo`,
    /// because it's diagnostic and shouldn't persist across
    /// sessions. See `PluginLogic::state_changed` below.
    state_load_count: u32,
}

impl StateExample {
    pub fn new(params: Arc<StateExampleParams>) -> Self {
        Self {
            params,
            memo: InstanceMemo::default(),
            state_load_count: 0,
        }
    }
}

impl PluginLogic for StateExample {
    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.params.set_sample_rate(sample_rate);
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Pass-through: copy input to output for every channel.
        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            out.copy_from_slice(inp);
        }
        ProcessStatus::Normal
    }

    fn save_state(&self) -> Vec<u8> {
        self.memo.serialize()
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), truce_core::state::StateLoadError> {
        match InstanceMemo::deserialize(data) {
            Some(m) => {
                self.memo = m;
                Ok(())
            }
            None => Err(truce_core::state::StateLoadError::Malformed(
                "InstanceMemo deserialize",
            )),
        }
    }

    /// Called on the audio thread immediately after `load_state`.
    /// The standard place for plugin-side cache invalidation that
    /// the next `process()` block reads - decoded IRs, sample
    /// thumbnails, computed pad layouts, etc.
    ///
    /// This example has no DSP-side derived data, so the body is
    /// just a diagnostic counter. The companion editor-side hook
    /// (`StateExampleUi::state_changed` below, on
    /// [`truce_core::Editor`]) is what refreshes the GUI cache -
    /// the two hooks split plugin-thread invalidation from
    /// GUI-thread repaint.
    fn state_changed(&mut self) {
        self.state_load_count = self.state_load_count.saturating_add(1);
    }

    fn custom_editor(&self) -> Option<Box<dyn Editor>> {
        Some(Box::new(
            EguiEditor::with_ui(
                self.params.clone(),
                (WINDOW_W, WINDOW_H),
                StateExampleUi {
                    binding: StateBinding::default(),
                    edit_buf: String::new(),
                },
            )
            .with_visuals(truce_egui::theme::dark())
            .with_font(font::JETBRAINS_MONO),
        ))
    }
}

// --- Editor ---

/// Stateful UI: holds the [`StateBinding`] cache + a local edit buffer
/// for the text field. The buffer lets the user type freely without
/// every keystroke roundtripping through `serialize` / `deserialize`.
struct StateExampleUi {
    binding: StateBinding<InstanceMemo>,
    edit_buf: String,
}

impl EditorUi<StateExampleParams> for StateExampleUi {
    fn opened(&mut self, ctx: &PluginContext<StateExampleParams>) {
        // Wire up the binding now that we have a real PluginContext.
        // `StateBinding::default()` was a placeholder before this point.
        self.binding = StateBinding::new(ctx);
        self.edit_buf = self.binding.get().label.clone();
    }

    fn state_changed(&mut self, _ctx: &PluginContext<StateExampleParams>) {
        // Host restored a session / preset / undo step. Re-read the
        // cached state and refresh the edit buffer to match.
        self.binding.sync();
        self.edit_buf = self.binding.get().label.clone();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _state: &PluginContext<StateExampleParams>) {
        egui::Panel::top("header")
            .exact_size(30.0)
            .frame(egui::Frame::NONE.fill(HEADER_BG))
            .show_inside(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new("STATE")
                            .size(14.0)
                            .color(HEADER_TEXT)
                            .strong(),
                    );
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(12.0))
            .show_inside(ui, |ui| {
                ui.label("Instance label");
                ui.add_space(4.0);
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.edit_buf)
                        .hint_text("(unnamed)")
                        .desired_width(f32::INFINITY),
                );
                // Push to plugin state on every keystroke. Cheap - the
                // memo only holds one String, and `update` does one
                // serialize + one set_state per call.
                if response.changed() {
                    let new_label = self.edit_buf.clone();
                    self.binding.update(|m| m.label = new_label);
                }
            });
    }
}

truce::plugin! {
    logic: StateExample,
    params: StateExampleParams,
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn make_plugin() -> StateExample {
        StateExample::new(Arc::new(StateExampleParams::new()))
    }

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    /// Set a label, save, fresh instance, load, label survived.
    /// Bypasses the format-wrapper envelope and tests the
    /// `PluginLogic::save_state` ↔ `load_state` direct path.
    #[test]
    fn label_round_trips() {
        let mut p = make_plugin();
        p.memo.label = "guitar bus".to_string();
        let bytes = p.save_state();

        let mut fresh = make_plugin();
        assert_eq!(fresh.memo.label, "");
        fresh.load_state(&bytes).unwrap();
        assert_eq!(fresh.memo.label, "guitar bus");
    }

    #[test]
    fn empty_label_round_trips() {
        let p = make_plugin();
        let bytes = p.save_state();
        let mut fresh = make_plugin();
        fresh.load_state(&bytes).unwrap();
        assert_eq!(fresh.memo.label, "");
    }

    #[test]
    fn unicode_label_round_trips() {
        let mut p = make_plugin();
        p.memo.label = "🎸 distortion ⚡ ã ç ñ".to_string();
        let bytes = p.save_state();
        let mut fresh = make_plugin();
        fresh.load_state(&bytes).unwrap();
        assert_eq!(fresh.memo.label, "🎸 distortion ⚡ ã ç ñ");
    }

    #[test]
    fn long_label_round_trips() {
        // 8 KB of label - exercises the `Vec<u8>` growth path in
        // `serialize` plus the byte-count length-prefix in
        // `StateField` for `String`.
        let mut p = make_plugin();
        p.memo.label = "x".repeat(8 * 1024);
        let bytes = p.save_state();
        let mut fresh = make_plugin();
        fresh.load_state(&bytes).unwrap();
        assert_eq!(fresh.memo.label.len(), 8 * 1024);
    }

    #[test]
    fn garbage_state_doesnt_panic() {
        // A truncated / hostile blob must leave the plugin at its
        // default rather than panic in deserialize. The Err return
        // is the documented signal - we just want to confirm it
        // doesn't unwind.
        let mut p = make_plugin();
        let _ = p.load_state(&[]);
        assert_eq!(p.memo.label, "");
        let _ = p.load_state(&[0xFF; 3]);
        assert_eq!(p.memo.label, "");
        let _ = p.load_state(&[0xFF; 32]);
        assert_eq!(p.memo.label, "");
    }

    /// Exercises the *full* envelope: param hash + version + extra
    /// blob, written + read back via the format wrapper layer
    /// (not just `PluginLogic::save_state`). Catches regressions in
    /// the wrapping logic that the direct test above can't see.
    #[test]
    fn envelope_round_trips() {
        truce_test::assert_state_round_trip::<Plugin>();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/state_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/state_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/state_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
