//! Shared building blocks for the reload-transition fixtures.
//!
//! Three cdylibs (`reload-fixture-keep-a`, `-keep-b`, `-reset`) export
//! these via `truce_loader::export_plugin!`. Each carries a DSP state
//! whose `counter` advances one step per `process` call and is surfaced
//! through `latency`, so a loader-level test can read it back and prove
//! whether a reload kept the live state or re-initialized it.
//!
//! `CounterState` (used by keep-a and keep-b) has one layout, so the two
//! dylibs share a fingerprint - a reload between them is the code-only
//! case that must preserve state. `ResetState` adds a field, so its
//! fingerprint differs and a reload to it must drop + re-init.

use truce::prelude::*;

#[derive(Params)]
pub struct FxParams {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)")]
    pub gain: FloatParam,
}

/// One-field state shared by keep-a and keep-b (same fingerprint).
pub struct CounterState {
    pub counter: u64,
}

/// Two-field state (distinct fingerprint from [`CounterState`]).
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

    fn init(_params: &FxParams) -> CounterState {
        CounterState { counter: 0 }
    }

    fn reset(_state: &mut CounterState, params: &FxParams, config: &AudioConfig) {
        params.set_sample_rate(config.sample_rate);
    }

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

    fn init(_params: &FxParams) -> ResetState {
        ResetState {
            counter: 0,
            extra: 0,
        }
    }

    fn reset(_state: &mut ResetState, params: &FxParams, config: &AudioConfig) {
        params.set_sample_rate(config.sample_rate);
    }

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
