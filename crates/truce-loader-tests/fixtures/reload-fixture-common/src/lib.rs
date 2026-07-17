//! Shared building blocks for the reload-transition fixtures.
//!
//! Three cdylibs (`reload-fixture-keep-a`, `-keep-b`, `-reset`) export
//! these via `truce_loader::export_plugin!`. Each carries a DSP state
//! whose `counter` advances one step per `process` call and is surfaced
//! through `latency`, so a loader-level test can read it back and prove
//! whether a reload carried the live state over or re-initialized it.
//!
//! `CounterLogic` (keep-a and keep-b) serializes its `counter` through
//! `save_state` / `load_state`, so a reload between them carries the
//! count over. `ResetLogic` defines neither, so a reload to it can't
//! restore the carried blob and starts fresh - the sound fallback.

use truce::prelude::*;

#[derive(Params)]
pub struct FxParams {
    /// Smoothed so shell-level tests can observe smoother behavior
    /// (ramp vs. snap) through a real loaded dylib.
    #[param(id = 0, name = "Gain", range = "linear(0, 1)", smooth = "exp(50)")]
    pub gain: FloatParam,
}

/// State shared by keep-a and keep-b; serialized on reload carry-over.
#[derive(Default)]
pub struct CounterState {
    pub counter: u64,
}

/// State for the reset fixture. Its logic defines no `save_state` /
/// `load_state`, so a reload to it can't restore the carried blob.
#[derive(Default)]
pub struct ResetState {
    pub counter: u64,
    pub extra: u64,
}

/// Saturating narrow so the observable read stays lint-clean.
fn as_u32(counter: u64) -> u32 {
    u32::try_from(counter).unwrap_or(u32::MAX)
}

pub struct CounterLogic;

impl PluginLogic for CounterLogic {
    type Params = FxParams;
    type DspState = CounterState;

    fn process(
        state: &mut CounterState,
        _params: &FxParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        state.counter += 1;
        passthrough(buffer);
        ProcessStatus::Normal
    }

    fn latency(state: &CounterState) -> u32 {
        as_u32(state.counter)
    }

    fn save_state(state: &CounterState) -> Vec<u8> {
        state.counter.to_le_bytes().to_vec()
    }

    fn load_state(state: &mut CounterState, data: &[u8]) -> Result<(), StateLoadError> {
        let bytes: [u8; 8] = data
            .try_into()
            .map_err(|_| StateLoadError::Malformed("CounterState expects 8 bytes"))?;
        state.counter = u64::from_le_bytes(bytes);
        Ok(())
    }

    fn editor(_params: Arc<FxParams>) -> Box<dyn Editor> {
        Box::new(NoEditor)
    }
}

pub struct ResetLogic;

impl PluginLogic for ResetLogic {
    type Params = FxParams;
    type DspState = ResetState;

    fn process(
        state: &mut ResetState,
        _params: &FxParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        state.counter += 1;
        passthrough(buffer);
        ProcessStatus::Normal
    }

    fn latency(state: &ResetState) -> u32 {
        as_u32(state.counter)
    }

    fn editor(_params: Arc<FxParams>) -> Box<dyn Editor> {
        Box::new(NoEditor)
    }
}

fn passthrough(buffer: &mut AudioBuffer) {
    for ch in 0..buffer.channels() {
        let (inp, out) = buffer.io_pair(ch, ch);
        out.copy_from_slice(inp);
    }
}

/// Editor slot is never exercised by the DSP-only reload tests.
struct NoEditor;

impl Editor for NoEditor {
    fn size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn open(&mut self, _: truce::core::editor::RawWindowHandle, _: PluginContext) {}
    fn close(&mut self) {}
    fn idle(&mut self) {}
}
