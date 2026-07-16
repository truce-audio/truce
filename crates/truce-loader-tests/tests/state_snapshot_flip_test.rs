//! Regression guard: the custom-state save path is `snapshot_into`, and
//! its default delegates to the legacy `save_state`.
//!
//! A plugin that overrides only `save_state` (the deprecated author
//! hook) must still round-trip on every format, because CLAP / VST3 /
//! AU / AAX read the lock-free snapshot slot the shell publishes via
//! `snapshot_into`, and LV2 / the standalone host serialize live
//! through the same method. Nothing in-tree exercised a `save_state`-only
//! plugin through the `PluginRuntime::snapshot_into` surface, so a shell
//! that forgot to delegate would have silently dropped state.

use std::sync::Arc;

use truce::prelude::{Editor, PluginContext};
use truce_core::AudioConfig;
use truce_core::buffer::AudioBuffer;
use truce_core::editor::RawWindowHandle;
use truce_core::events::EventList;
use truce_core::plugin::PluginRuntime;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_core::state::StateLoadError;
use truce_derive::Params;
use truce_gui::PluginLogic;
use truce_loader::static_shell::StaticShell;
use truce_params::FloatParam;

#[derive(Params)]
struct StateParams {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)")]
    gain: FloatParam,
}

#[derive(Default)]
struct Counter {
    value: u32,
}

/// Overrides only the legacy `save_state` / `load_state` - never
/// `snapshot_into`. The flipped default is what carries its state.
struct LegacyStatePlugin;

impl PluginLogic for LegacyStatePlugin {
    type Params = StateParams;
    type DspState = Counter;

    fn process(
        _state: &mut Counter,
        _params: &StateParams,
        _buffer: &mut AudioBuffer,
        _events: &EventList,
        _ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }

    fn save_state(state: &Counter) -> Vec<u8> {
        state.value.to_le_bytes().to_vec()
    }

    fn load_state(state: &mut Counter, data: &[u8]) -> Result<(), StateLoadError> {
        let bytes: [u8; 4] = data
            .try_into()
            .map_err(|_| StateLoadError::Malformed("Counter expects 4 bytes"))?;
        state.value = u32::from_le_bytes(bytes);
        Ok(())
    }

    fn editor(_params: Arc<StateParams>) -> Box<dyn Editor> {
        Box::new(NoEditor)
    }
}

/// A plugin with no custom state at all - both `save_state` and
/// `snapshot_into` keep their defaults, so it must never publish.
struct PlainPlugin;

impl PluginLogic for PlainPlugin {
    type Params = StateParams;
    type DspState = ();

    fn process(
        _state: &mut (),
        _params: &StateParams,
        _buffer: &mut AudioBuffer,
        _events: &EventList,
        _ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }

    fn editor(_params: Arc<StateParams>) -> Box<dyn Editor> {
        Box::new(NoEditor)
    }
}

struct NoEditor;
impl Editor for NoEditor {
    fn size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn open(&mut self, _: RawWindowHandle, _: PluginContext) {}
    fn close(&mut self) {}
    fn idle(&mut self) {}
}

fn shell<L>() -> StaticShell<StateParams, L>
where
    L: PluginLogic<Params = StateParams>,
{
    let mut shell = StaticShell::<StateParams, L>::from_parts(Arc::new(StateParams::new()), None);
    shell.reset(&AudioConfig::new(44100.0, 64));
    shell
}

/// The flip: a `save_state`-only plugin publishes through the
/// `snapshot_into` slot every RT format reads.
#[test]
fn save_state_only_plugin_publishes_through_snapshot_into() {
    let mut plugin = shell::<LegacyStatePlugin>();
    plugin
        .load_state(&7u32.to_le_bytes())
        .expect("load_state accepts a 4-byte counter");

    let mut buf = Vec::new();
    let published = plugin.snapshot_into(&mut buf);

    assert!(
        published,
        "a save_state-only plugin must publish a snapshot"
    );
    assert_eq!(buf, 7u32.to_le_bytes(), "the published bytes are its state");
}

/// End-to-end: snapshot one instance's state, restore it into a fresh
/// instance, and confirm the fresh instance publishes the same bytes.
#[test]
fn snapshot_into_round_trips_legacy_state() {
    let mut source = shell::<LegacyStatePlugin>();
    source
        .load_state(&0xDEAD_BEEFu32.to_le_bytes())
        .expect("load_state");
    let mut saved = Vec::new();
    source.snapshot_into(&mut saved);

    let mut restored = shell::<LegacyStatePlugin>();
    restored.load_state(&saved).expect("restore the saved blob");
    let mut round_tripped = Vec::new();
    restored.snapshot_into(&mut round_tripped);

    assert_eq!(round_tripped, saved, "state survives snapshot -> restore");
}

/// A plugin with no custom state keeps `snapshot_into` returning false,
/// so the shell's publish path stays latched off (no wasted work).
#[test]
fn plugin_without_custom_state_does_not_publish() {
    let plugin = shell::<PlainPlugin>();
    let mut buf = Vec::new();
    let published = plugin.snapshot_into(&mut buf);

    assert!(!published, "a stateless plugin must not publish a snapshot");
    assert!(buf.is_empty(), "nothing is written when there's no state");
}
