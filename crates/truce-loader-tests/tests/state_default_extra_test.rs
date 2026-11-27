//! truce#251: a restored state with no custom state must not leave the
//! previous custom state in place - in the plugin, or in what a save
//! returns before the audio thread has applied the load.
//!
//! The scenario from the report: a plugin whose custom state is a list of
//! patch cables loads preset A (one cable), then preset B, authored with
//! parameters only. The envelope can't tell "no custom state" from "custom
//! state that serialized to nothing", so B carries no `extra`. A plugin
//! that wants B to replace A's cables returns its default state from
//! `default_extra`; the wrapper fills it in on the host thread, which also
//! lets it publish the bytes before the queued load is applied.

use std::sync::Arc;

use crossbeam_queue::ArrayQueue;
use truce::prelude::{Editor, PluginContext};
use truce_core::buffer::AudioBuffer;
use truce_core::custom_state::State;
use truce_core::editor::RawWindowHandle;
use truce_core::events::EventList;
use truce_core::plugin::PluginRuntime;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_core::state::{DeserializedState, StateLoadError, apply_params, with_default_extra};
use truce_core::wrapper::{apply_queued_state, publish_loaded_extra, queue_host_state, save_extra};
use truce_derive::{Params, State};
use truce_gui::PluginLogic;
use truce_loader::static_shell::StaticShell;
use truce_params::{FloatParam, Params};

#[derive(Params)]
struct PatchParams {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)")]
    gain: FloatParam,
}

/// The persisted part: cables, in the documented `#[derive(State)]` idiom.
#[derive(State, Default, Clone, Debug, PartialEq)]
struct Patch {
    cables: Vec<u32>,
}

/// Shared `PluginLogic` body for both plugins under test; only
/// `default_extra` differs, so it's supplied per plugin.
macro_rules! patch_plugin {
    ($name:ident $(, $default:item)?) => {
        struct $name;

        impl PluginLogic for $name {
            type Params = PatchParams;
            type DspState = Patch;

            fn process(
                _state: &mut Patch,
                _params: &PatchParams,
                _buffer: &mut AudioBuffer,
                _events: &EventList,
                _ctx: &mut ProcessContext,
            ) -> ProcessStatus {
                ProcessStatus::Normal
            }

            fn snapshot_into(state: &Patch, buf: &mut Vec<u8>) -> bool {
                buf.extend_from_slice(&state.serialize());
                true
            }

            fn load_state(state: &mut Patch, data: &[u8]) -> Result<(), StateLoadError> {
                *state = Patch::deserialize(data).ok_or(StateLoadError::Malformed("Patch"))?;
                Ok(())
            }

            $($default)?

            fn editor(_params: Arc<PatchParams>) -> Box<dyn Editor> {
                Box::new(NoEditor)
            }
        }
    };
}

// Opts in: a full restore replaces the patch, so no custom state means none.
patch_plugin!(
    ReplacingSynth,
    fn default_extra() -> Option<Vec<u8>> {
        Some(Patch::default().serialize())
    }
);

// Keeps the default (`None`): a parameter-only preset leaves the patch.
patch_plugin!(KeepingSynth);

struct NoEditor;
impl Editor for NoEditor {
    fn size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn open(&mut self, _: RawWindowHandle, _: PluginContext) {}
    fn close(&mut self) {}
    fn idle(&mut self, _dt: std::time::Duration) {}
}

type Shell<L> = StaticShell<PatchParams, L>;

/// An instance that has loaded preset A and published it, as after a block.
fn after_preset_a<L>() -> Shell<L>
where
    L: PluginLogic<Params = PatchParams, DspState = Patch>,
{
    let mut plugin = Shell::<L>::from_parts(Arc::new(PatchParams::new()), None);
    plugin
        .load_state(&Patch { cables: vec![42] }.serialize())
        .expect("preset A loads");
    plugin.republish_snapshot();
    plugin
}

/// Preset B as the host-thread side of a load sees it once settled: no
/// custom state in the envelope, so the plugin's `default_extra`.
fn preset_b<L>() -> DeserializedState
where
    L: PluginLogic<Params = PatchParams, DspState = Patch>,
{
    with_default_extra::<Shell<L>>(DeserializedState {
        params: vec![(0, 0.5)],
        extra: None,
        persist: Vec::new(),
    })
}

/// Apply a settled state the way the audio thread's drain does.
fn apply<L>(plugin: &mut Shell<L>, state: &DeserializedState)
where
    L: PluginLogic<Params = PatchParams, DspState = Patch>,
{
    if let Some(extra) = &state.extra {
        plugin.load_state(extra).expect("loads");
    }
    plugin.republish_snapshot();
}

#[test]
fn a_restore_without_custom_state_loads_an_opted_in_plugins_default() {
    let mut plugin = after_preset_a::<ReplacingSynth>();
    apply(&mut plugin, &preset_b::<ReplacingSynth>());
    assert_eq!(*plugin.state_ref(), Patch::default(), "B has no cables");
}

#[test]
fn the_default_keeps_the_current_custom_state() {
    let state = preset_b::<KeepingSynth>();
    assert!(state.extra.is_none(), "nothing to load");
    let mut plugin = after_preset_a::<KeepingSynth>();
    apply(&mut plugin, &state);
    assert_eq!(plugin.state_ref().cables, vec![42]);
}

/// The reported bug: a load queued for the audio thread, then a save
/// before the block that applies it. The save must already carry B's
/// custom state, not A's beside B's params.
#[test]
fn a_save_before_the_queued_load_applies_carries_the_default() {
    let plugin = after_preset_a::<ReplacingSynth>();
    let slot = plugin.snapshot_slot();
    assert_eq!(save_extra(&slot), Patch { cables: vec![42] }.serialize());

    // Host thread: settle and publish; the audio thread hasn't drained yet.
    let state = preset_b::<ReplacingSynth>();
    publish_loaded_extra(&slot, state.extra.as_deref());

    assert_eq!(save_extra(&slot), Patch::default().serialize());
}

/// For a keeping plugin the slot already holds what the instance keeps,
/// so a pre-drain save carries A's cables, as it should.
#[test]
fn a_save_before_the_queued_load_applies_keeps_a_keeping_plugins_state() {
    let plugin = after_preset_a::<KeepingSynth>();
    let slot = plugin.snapshot_slot();
    let state = preset_b::<KeepingSynth>();
    publish_loaded_extra(&slot, state.extra.as_deref());
    assert_eq!(save_extra(&slot), Patch { cables: vec![42] }.serialize());
}

/// Why bytes and not `load_state(&[])`: the documented `#[derive(State)]`
/// idiom rejects empty bytes, so passing them would fail every
/// parameter-only preset instead of resetting anything.
#[test]
fn derived_state_rejects_empty_bytes() {
    assert!(Patch::deserialize(&[]).is_none());
}

/// A full load queued while the plugin is inactive (as a host recall
/// before activation is) must not revert a parameter changed before the
/// queue drains. The load's parameters were applied on the host thread
/// when it was queued; the drain - here an activate's - applies only the
/// custom state.
#[test]
fn draining_a_queued_load_keeps_later_parameter_changes() {
    let params = Arc::new(PatchParams::new());
    let mut plugin = Shell::<KeepingSynth>::from_parts(params.clone(), None);
    let slot = plugin.snapshot_slot();
    let pending = ArrayQueue::new(1);

    let load = DeserializedState {
        params: vec![(0, 0.25)],
        extra: Some(Patch { cables: vec![7] }.serialize()),
        persist: Vec::new(),
    };
    apply_params(&*params, &load);
    queue_host_state(&slot, &pending, load);
    assert_eq!(
        params.get_plain(0),
        Some(0.25),
        "applied on the host thread"
    );

    // The host moves the parameter before anything drains the queue.
    params.set_plain(0, 0.75);
    apply_queued_state(&mut plugin, &pending);

    assert_eq!(
        params.get_plain(0),
        Some(0.75),
        "the drain kept the later change"
    );
    assert_eq!(
        plugin.state_ref().cables,
        vec![7],
        "the custom state applied"
    );
}

/// Load A queued, then a parameter-only load B before anything drains A.
/// Newest wins in the single-slot queue, but B's "keep the current custom
/// state" means A's, which a save in between already returned - so B
/// carries A's custom state instead of discarding it.
#[test]
fn a_parameter_only_load_keeps_an_undrained_loads_custom_state() {
    let mut plugin = Shell::<KeepingSynth>::from_parts(Arc::new(PatchParams::new()), None);
    let slot = plugin.snapshot_slot();
    let pending = ArrayQueue::new(1);

    queue_host_state(
        &slot,
        &pending,
        DeserializedState {
            params: Vec::new(),
            extra: Some(Patch { cables: vec![42] }.serialize()),
            persist: Vec::new(),
        },
    );
    let b = preset_b::<KeepingSynth>();
    assert!(b.extra.is_none(), "B carries no custom state");
    queue_host_state(&slot, &pending, b);
    assert_eq!(save_extra(&slot), Patch { cables: vec![42] }.serialize());

    apply_queued_state(&mut plugin, &pending);
    assert_eq!(
        plugin.state_ref().cables,
        vec![42],
        "A's custom state survived B"
    );
    assert_eq!(save_extra(&slot), Patch { cables: vec![42] }.serialize());
}

/// With nothing queued, a load without custom state has nothing to hand
/// the plugin, so it isn't queued.
#[test]
fn a_parameter_only_load_with_nothing_queued_queues_nothing() {
    let plugin = Shell::<KeepingSynth>::from_parts(Arc::new(PatchParams::new()), None);
    let pending = ArrayQueue::new(1);
    queue_host_state(
        &plugin.snapshot_slot(),
        &pending,
        preset_b::<KeepingSynth>(),
    );
    assert!(pending.is_empty());
}
