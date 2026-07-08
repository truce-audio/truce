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
use truce_font::JETBRAINS_MONO;

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

/// Stateless descriptor. The per-instance state lives in [`StateExampleDspState`].
pub struct StateExample;

#[derive(DspState)]
pub struct StateExampleDspState {
    memo: InstanceMemo,
    /// Runtime counter - how many times the host has restored
    /// state on this instance (preset recall, undo, session
    /// load). Lives in the plugin state, *not* in `InstanceMemo`,
    /// because it's diagnostic and shouldn't persist across
    /// sessions. See `PluginLogic::state_changed` below.
    state_load_count: u32,
}

impl PluginLogic for StateExample {
    type Params = StateExampleParams;
    type DspState = StateExampleDspState;

    fn init(_params: &StateExampleParams) -> StateExampleDspState {
        StateExampleDspState {
            memo: InstanceMemo::default(),
            state_load_count: 0,
        }
    }

    fn reset(_state: &mut StateExampleDspState, params: &StateExampleParams, config: &AudioConfig) {
        params.set_sample_rate(config.sample_rate);
    }

    fn process(
        _state: &mut StateExampleDspState,
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

    fn snapshot_into(state: &StateExampleDspState, buf: &mut Vec<u8>) -> bool {
        // Opt into lock-free save: the audio thread publishes the memo
        // into the shell's slot each block, and the host serializes it
        // without ever taking the plugin lock. (The default `save_state`
        // delegates here, so the fallback path stays consistent.)
        //
        // `serialize_into` clears and refills `buf`, reusing its
        // capacity - no allocation once warmed, as the audio thread
        // requires.
        state.memo.serialize_into(buf);
        true
    }

    fn load_state(
        state: &mut StateExampleDspState,
        data: &[u8],
    ) -> Result<(), truce_core::state::StateLoadError> {
        match InstanceMemo::deserialize(data) {
            Some(m) => {
                state.memo = m;
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
    fn state_changed(state: &mut StateExampleDspState, _params: &StateExampleParams) {
        state.state_load_count = state.state_load_count.saturating_add(1);
    }

    fn editor(params: Arc<StateExampleParams>) -> Box<dyn Editor> {
        Box::new(
            EguiEditor::with_ui(
                params.clone(),
                (WINDOW_W, WINDOW_H),
                StateExampleUi {
                    binding: StateBinding::default(),
                    edit_buf: String::new(),
                },
            )
            .with_visuals(truce_egui::theme::dark())
            .with_font(JETBRAINS_MONO),
        )
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

truce::enable_rt_paranoid!();

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

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

    fn make_state() -> StateExampleDspState {
        StateExample::init(&StateExampleParams::new())
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
        let mut p = make_state();
        p.memo.label = "guitar bus".to_string();
        let bytes = StateExample::save_state(&p);

        let mut fresh = make_state();
        assert_eq!(fresh.memo.label, "");
        StateExample::load_state(&mut fresh, &bytes).unwrap();
        assert_eq!(fresh.memo.label, "guitar bus");
    }

    #[test]
    fn empty_label_round_trips() {
        let p = make_state();
        let bytes = StateExample::save_state(&p);
        let mut fresh = make_state();
        StateExample::load_state(&mut fresh, &bytes).unwrap();
        assert_eq!(fresh.memo.label, "");
    }

    #[test]
    fn unicode_label_round_trips() {
        let mut p = make_state();
        p.memo.label = "🎸 distortion ⚡ ã ç ñ".to_string();
        let bytes = StateExample::save_state(&p);
        let mut fresh = make_state();
        StateExample::load_state(&mut fresh, &bytes).unwrap();
        assert_eq!(fresh.memo.label, "🎸 distortion ⚡ ã ç ñ");
    }

    #[test]
    fn long_label_round_trips() {
        // 8 KB of label - exercises the `Vec<u8>` growth path in
        // `serialize` plus the byte-count length-prefix in
        // `StateField` for `String`.
        let mut p = make_state();
        p.memo.label = "x".repeat(8 * 1024);
        let bytes = StateExample::save_state(&p);
        let mut fresh = make_state();
        StateExample::load_state(&mut fresh, &bytes).unwrap();
        assert_eq!(fresh.memo.label.len(), 8 * 1024);
    }

    #[test]
    fn garbage_state_doesnt_panic() {
        // A truncated / hostile blob must leave the plugin at its
        // default rather than panic in deserialize. The Err return
        // is the documented signal - we just want to confirm it
        // doesn't unwind.
        let mut p = make_state();
        let _ = StateExample::load_state(&mut p, &[]);
        assert_eq!(p.memo.label, "");
        let _ = StateExample::load_state(&mut p, &[0xFF; 3]);
        assert_eq!(p.memo.label, "");
        let _ = StateExample::load_state(&mut p, &[0xFF; 32]);
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

    /// Pins the editor-bridge wire-format contract that the format
    /// wrappers (`truce-clap` / `truce-vst3` / `truce-au`) depend on.
    ///
    /// The editor's `StateBinding::update` writes
    /// `InstanceMemo::serialize()` bytes to the bridge's `set_state`.
    /// Those are **raw custom-state** bytes - the same thing
    /// `save_state` emits - and the wrapper must hand them straight to
    /// `load_state`. They are deliberately *not* a `serialize_state`
    /// host envelope (no `OAST` magic / version / plugin-id header).
    ///
    /// A wrapper that mistook this channel for the host channel and ran
    /// the bytes through `deserialize_state` (the envelope parser) would
    /// get `None` and silently drop every GUI edit - which is exactly
    /// the bug this guards against. So we assert both halves: editor
    /// bytes are rejected by the envelope parser, and accepted by
    /// `load_state`.
    #[test]
    fn editor_bytes_feed_load_state_not_envelope() {
        let mut p = make_state();
        p.memo.label = "from editor".to_string();
        let editor_bytes = p.memo.serialize(); // what StateBinding::update sends

        // The envelope parser must reject these (magic check fails first,
        // so the plugin-id argument is irrelevant - pass 0).
        assert!(
            truce_core::state::deserialize_state(&editor_bytes, 0).is_none(),
            "editor custom-state bytes must NOT parse as a host state envelope"
        );

        // The correct sink consumes them directly.
        let mut fresh = make_state();
        StateExample::load_state(&mut fresh, &editor_bytes).unwrap();
        assert_eq!(fresh.memo.label, "from editor");
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

    // --- Lock-free path proofs ---
    //
    // Both hold the plugin lock (standing in for an in-flight audio
    // block) and require the host-side operation to complete anyway. If
    // either op took the plugin lock, the spawned thread would block for
    // the whole hold and the `recv_timeout` would elapse - the tests
    // fail loudly instead of hanging.

    use std::sync::mpsc;
    use std::time::Duration;
    use truce_core::AudioConfig;
    use truce_core::buffer::AudioBuffer;
    use truce_core::events::{EventList, TransportInfo};
    use truce_core::export::PluginExport;
    use truce_core::plugin::PluginRuntime;
    use truce_core::process::ProcessContext;
    use truce_core::wrapper::{lock_plugin, save_extra, shared_plugin};

    #[test]
    fn shell_publishes_snapshot_during_process() {
        let mut inst = Plugin::create();
        inst.init();
        let snapshot = inst.snapshot_slot();
        // Nothing is published before the first block.
        assert!(snapshot.read().is_none());

        inst.reset(&AudioConfig::new(44100.0, 64));
        let input = vec![0.0f32; 64];
        let inputs: Vec<&[f32]> = vec![&input, &input];
        let mut out0 = vec![0.0f32; 64];
        let mut out1 = vec![0.0f32; 64];
        let mut outputs: Vec<&mut [f32]> = vec![&mut out0, &mut out1];
        // SAFETY: the slices outlive the buffer and match `len`.
        let mut buffer = unsafe { AudioBuffer::from_slices(&inputs, &mut outputs, 64) };
        let events = EventList::default();
        let transport = TransportInfo::default();
        let mut output_events = EventList::default();
        let mut ctx = ProcessContext::new(&transport, 44100.0, 64, &mut output_events);
        inst.process(&mut buffer, &events, &mut ctx);

        // The shell published the plugin's `snapshot_into` bytes, and
        // they equal what `save_state` emits (which delegates to it).
        let published = snapshot
            .read()
            .expect("shell should publish a snapshot after a block");
        assert_eq!(published, inst.save_state());
    }

    #[test]
    fn editor_construction_never_takes_the_plugin_lock() {
        let inst = Plugin::create();
        let params = inst.params_arc();
        // The wrapper caches this builder at creation, outside the lock.
        let make_editor = inst.editor_builder();
        let plugin = shared_plugin(inst);
        let _held = lock_plugin(&plugin);

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // The builder binds only the lock-free param store, so it
            // returns while the plugin lock is held.
            let _ = tx.send(make_editor(params).is_some());
        });
        let built = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("editor construction blocked on the held plugin lock");
        assert!(built, "state example must return an editor");
    }

    #[test]
    fn save_reads_snapshot_without_the_plugin_lock() {
        let inst = Plugin::create();
        let snapshot = inst.snapshot_slot();
        // Publish a snapshot the way the shell's `process` does.
        snapshot.publish(|buf| {
            buf.clear();
            buf.extend_from_slice(&[0xAB, 0xCD]);
            true
        });
        let plugin = shared_plugin(inst);
        let _held = lock_plugin(&plugin);

        let (tx, rx) = mpsc::channel();
        let snap = Arc::clone(&snapshot);
        let plug = Arc::clone(&plugin);
        std::thread::spawn(move || {
            let _ = tx.send(save_extra(&snap, &plug));
        });
        let bytes = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("save blocked on the held plugin lock");
        assert_eq!(bytes, vec![0xAB, 0xCD]);
    }
}
