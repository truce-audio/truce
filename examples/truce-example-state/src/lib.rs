//! Hello world for `#[persist]`.
//!
//! Strings can't be parameters (the param system stores numeric atoms),
//! so the user's per-instance label lives in a `#[persist]` field on the
//! parameter struct. `#[persist]` fields are editor-facing config the
//! host saves with the project and restores on load, but never lists in
//! its automation editor. The plugin does nothing to the audio - it's a
//! pass-through whose only job is to demonstrate `#[persist]` end-to-end.

use std::sync::{Arc, RwLock};

use truce::prelude::*;
use truce_core::editor::PluginContext;
use truce_egui::theme::{HEADER_BG, HEADER_TEXT};
use truce_egui::{EditorUi, EguiEditor};
use truce_font::JETBRAINS_MONO;

const WINDOW_W: u32 = 320;
const WINDOW_H: u32 = 120;

// --- Parameters ---

/// Use `Param` for values the host should treat as a control: gain,
/// frequency, mix, bypass, mode selectors. Hosts list params in their
/// automation editor, draw automation lanes against them, record undo
/// entries on every change, and feed them through MIDI CC / OSC
/// mappings. They're numeric atoms (`f32` / `f64` / `bool` / int /
/// enum-as-index) read lock-free from the audio thread.
///
/// Use `#[persist]` for editor-facing config that isn't a numeric atom:
/// strings, file paths, view modes, small structs. The host saves and
/// restores the bytes alongside the param values but shows no automation
/// lane, no CC mapping, no entry in its parameter list. The field needs
/// interior mutability so the editor can write it through the shared
/// `Arc<Params>`: reach for a lock-free `AtomicCell<T>` for a `Copy`
/// scalar (a persisted `f32`, enum, or index - `edit_count` below), and
/// a `RwLock` / `Mutex` for a `String`, `Vec`, or `#[derive(State)]`
/// struct (`memo` below).
///
/// In this example: `Active` is a bool the user might want to automate,
/// so it's a param. The instance label is plain text the editor writes,
/// so it's persisted.
#[derive(Params)]
pub struct StateExampleParams {
    #[param(name = "Active", default = 1)]
    pub active: BoolParam,

    #[persist = "memo"]
    pub memo: RwLock<InstanceMemo>,

    /// Lock-free `Copy` persist field: how many edits this instance has
    /// seen. Survives save/load like the memo, but a plain `u32` behind
    /// an `AtomicCell` reads cleaner than a `RwLock<u32>`.
    #[persist = "edit_count"]
    pub edit_count: AtomicCell<u32>,
}

/// A `#[derive(State)]` struct - a compound value the persist machinery
/// round-trips by byte. One field today; it exists as a struct so the
/// memo can grow (color, notes, tags) without touching the wire format.
#[derive(State, Default, Clone)]
pub struct InstanceMemo {
    /// User-typed label for this plugin instance. Persists across
    /// session save/load and preset recall.
    pub label: String,
}

// --- Plugin ---

/// Stateless descriptor. There is no per-instance DSP state: the memo
/// lives in the persisted params, and the audio path is a pass-through.
pub struct StateExample;

impl PurePluginLogic for StateExample {
    type Params = StateExampleParams;

    fn process(
        _params: &StateExampleParams,
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

    fn editor(params: Arc<StateExampleParams>) -> Box<dyn Editor> {
        Box::new(
            EguiEditor::with_ui(
                params.clone(),
                (WINDOW_W, WINDOW_H),
                StateExampleUi {
                    edit_buf: String::new(),
                },
            )
            .with_visuals(truce_egui::theme::dark())
            .with_font(JETBRAINS_MONO),
        )
    }
}

// --- Editor ---

/// Stateful UI: holds a local edit buffer for the text field so the user
/// types freely, and mirrors it to/from the persisted `memo` behind the
/// shared `Arc<Params>`.
struct StateExampleUi {
    edit_buf: String,
}

impl EditorUi<StateExampleParams> for StateExampleUi {
    fn opened(&mut self, ctx: &PluginContext<StateExampleParams>) {
        if let Ok(memo) = ctx.params().memo.read() {
            self.edit_buf.clone_from(&memo.label);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &PluginContext<StateExampleParams>) {
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
                if response.changed() {
                    // Push edits into the persisted memo. The host saves
                    // it at its own discretion (session / preset save).
                    if let Ok(mut memo) = ctx.params().memo.write() {
                        memo.label.clone_from(&self.edit_buf);
                    }
                    ctx.params().edit_count.fetch_add(1);
                } else if !response.has_focus() {
                    // Not being edited: mirror whatever the host last
                    // restored so a session / preset load shows through.
                    if let Ok(memo) = ctx.params().memo.read()
                        && self.edit_buf != memo.label
                    {
                        self.edit_buf.clone_from(&memo.label);
                    }
                }

                // The persisted edit count, once there is one. Hidden at
                // zero so a fresh instance stays visually clean.
                let edits = ctx.params().edit_count.load();
                if edits > 0 {
                    ui.add_space(6.0);
                    ui.label(format!("Edited {edits} times"));
                }
            });
    }
}

truce::plugin! {
    logic: StateExample,
    params: StateExampleParams,
}

truce::enable_rt_paranoid!();

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh param store with the memo label set.
    fn params_with_label(label: &str) -> StateExampleParams {
        let params = StateExampleParams::new();
        params.memo.write().unwrap().label = label.to_string();
        params
    }

    fn label_of(params: &StateExampleParams) -> String {
        params.memo.read().unwrap().label.clone()
    }

    #[test]
    fn process_is_allocation_free() {
        use std::time::Duration;
        use truce_test::{InputSource, assert_no_audio_alloc, driver};
        assert_no_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.25))
                .script(|s| {
                    s.set_param(StateExampleParamsParamId::Active, 0.9);
                    s.wait_ms(15);
                    s.set_param(StateExampleParamsParamId::Active, 0.1);
                    s.wait_ms(15);
                })
                .run()
        });
    }

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    /// Set a label, snapshot the persist blob, restore into a fresh
    /// store, label survived. Exercises the `#[persist]` codegen
    /// (`serialize_persist` <-> `load_persist`) directly.
    #[test]
    fn label_round_trips() {
        let params = params_with_label("guitar bus");
        let blob = params.serialize_persist();

        let fresh = StateExampleParams::new();
        assert_eq!(label_of(&fresh), "");
        fresh.load_persist(&blob);
        assert_eq!(label_of(&fresh), "guitar bus");
    }

    /// The lock-free `AtomicCell` persist field round-trips through the
    /// same keyed blob as the locked `memo`.
    #[test]
    fn edit_count_round_trips() {
        let params = StateExampleParams::new();
        params.edit_count.store(7);
        let blob = params.serialize_persist();

        let fresh = StateExampleParams::new();
        assert_eq!(fresh.edit_count.load(), 0);
        fresh.load_persist(&blob);
        assert_eq!(fresh.edit_count.load(), 7);
    }

    #[test]
    fn empty_label_round_trips() {
        let blob = StateExampleParams::new().serialize_persist();
        let fresh = StateExampleParams::new();
        fresh.load_persist(&blob);
        assert_eq!(label_of(&fresh), "");
    }

    #[test]
    fn unicode_label_round_trips() {
        let params = params_with_label("🎸 distortion ⚡ ã ç ñ");
        let blob = params.serialize_persist();
        let fresh = StateExampleParams::new();
        fresh.load_persist(&blob);
        assert_eq!(label_of(&fresh), "🎸 distortion ⚡ ã ç ñ");
    }

    #[test]
    fn long_label_round_trips() {
        // 8 KB of label - exercises the `Vec<u8>` growth path plus the
        // byte-count length-prefix `StateField` uses for `String`.
        let params = params_with_label(&"x".repeat(8 * 1024));
        let blob = params.serialize_persist();
        let fresh = StateExampleParams::new();
        fresh.load_persist(&blob);
        assert_eq!(label_of(&fresh).len(), 8 * 1024);
    }

    #[test]
    fn garbage_persist_doesnt_panic() {
        // A truncated / hostile blob must leave the field at its default
        // rather than panic. `load_persist` reads defensively and bails
        // on the first short read.
        let params = StateExampleParams::new();
        params.load_persist(&[]);
        assert_eq!(label_of(&params), "");
        params.load_persist(&[0xFF; 3]);
        assert_eq!(label_of(&params), "");
        params.load_persist(&[0xFF; 32]);
        assert_eq!(label_of(&params), "");
    }

    /// Exercises the *full* envelope: param hash + version + persist
    /// block, written + read back via the format wrapper layer. Catches
    /// regressions in the wrapping logic the direct test above can't see.
    #[test]
    fn envelope_round_trips() {
        truce_test::assert_state_round_trip::<Plugin>();
    }

    /// `#[derive(State)]` is keyed: a struct whose fields were reordered,
    /// with one removed and one added, still recovers each field's value
    /// by name. Positional decoding would mis-assign here.
    #[test]
    fn state_keyed_survives_reorder_remove_add() {
        #[derive(State, Default)]
        struct V1 {
            alpha: u32,
            beta: String,
            gamma: u32,
        }
        // gamma/alpha reordered, beta removed, delta added.
        #[derive(State, Default)]
        struct V2 {
            gamma: u32,
            alpha: u32,
            delta: bool,
        }

        let mut buf = Vec::new();
        V1 {
            alpha: 11,
            beta: "gone".into(),
            gamma: 33,
        }
        .serialize_into(&mut buf);

        let v2 = V2::deserialize(&buf).expect("keyed deserialize");
        // Matched by name across the reorder + removal - not positionally
        // (which would have put alpha's 11 into gamma).
        assert_eq!(v2.alpha, 11);
        assert_eq!(v2.gamma, 33);
        // Added field defaults; removed field's bytes are ignored.
        assert!(!v2.delta);
    }

    /// Old sessions still load: a hand-built pre-keyed (positional) blob
    /// deserializes through the legacy path (the leading word is a small
    /// field count, not the keyed magic).
    #[test]
    fn state_reads_legacy_positional_blob() {
        #[derive(State, Default)]
        struct Legacy {
            x: u32,
            y: u32,
        }
        // [field_count=2][len=4][x=5][len=4][y=9] - the old layout.
        let mut blob = Vec::new();
        blob.extend_from_slice(&2u32.to_le_bytes());
        blob.extend_from_slice(&4u32.to_le_bytes());
        blob.extend_from_slice(&5u32.to_le_bytes());
        blob.extend_from_slice(&4u32.to_le_bytes());
        blob.extend_from_slice(&9u32.to_le_bytes());

        let v = Legacy::deserialize(&blob).expect("legacy positional read");
        assert_eq!(v.x, 5);
        assert_eq!(v.y, 9);
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
