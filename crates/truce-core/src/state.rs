//! Plugin-state save / restore helpers layered on the canonical wire
//! format in [`truce_utils::state`]. The wire functions are
//! re-exported here so format wrappers and plugin code keep a single
//! import path; the envelope itself lives in `truce-utils` so
//! `cargo-truce` can emit byte-identical blobs (factory preset files)
//! without inheriting `truce-core`'s runtime dependency chain.

pub use truce_utils::state::{
    DeserializedState, deserialize_state, hash_plugin_id, serialize_state, vst3_cid,
};

/// Reason a [`crate::PluginRuntime::load_state`] /
/// `truce_plugin::PluginLogic::load_state` implementation failed to
/// interpret the host-supplied extra-state blob. Format wrappers
/// receive this on the audio-thread apply path and log it; hosts
/// that surface a non-success code to the DAW (e.g. CLAP
/// `state_load` returning `false`) read the variant via that path.
///
/// `Malformed` is the typical case: the blob's framing or content
/// doesn't match what `save_state` would emit (version skew between
/// older session files and newer plugin builds is the canonical
/// example). `Other` carries a free-form message for plugin-specific
/// failures that don't fit the malformed-bytes shape.
#[derive(Debug)]
#[non_exhaustive]
pub enum StateLoadError {
    /// State blob is too short, mis-framed, or otherwise unparseable.
    Malformed(&'static str),
    /// Plugin-specific failure with a free-form message.
    Other(String),
}

impl std::fmt::Display for StateLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(s) => write!(f, "malformed state: {s}"),
            Self::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for StateLoadError {}

/// Apply a deserialized state to a plugin: write parameter values,
/// snap smoothers, then hand the optional extra blob to
/// [`crate::plugin::PluginRuntime::load_state`].
///
/// Format wrappers call this from the audio thread after popping a
/// pending load off their per-instance handoff queue. The reason it
/// must run on the audio thread (and not on the host's main thread,
/// where state-load callbacks are typically invoked): `load_state`
/// takes `&mut P`, which would alias the audio thread's `&mut P`
/// inside `process()` and produce a data race. The audio thread is
/// the single thread that already owns `&mut P` between blocks, so
/// running the load there sidesteps the race entirely.
///
/// `restore_values` and `snap_smoothers` go through the param
/// struct's interior atomics, so they don't strictly need to run on
/// the audio thread - but applying the whole state in one place keeps
/// the param values and the user's extra-state blob coherent for any
/// observer reading after this returns.
pub fn apply_state<P: crate::export::PluginExport>(plugin: &mut P, state: &DeserializedState) {
    use truce_params::Params;
    plugin.params().restore_values(&state.params);
    plugin.params().snap_smoothers();
    if let Some(extra) = &state.extra
        && let Err(e) = plugin.load_state(extra)
    {
        // Audio-thread error path: host already received a "yes I
        // accepted the state" return from the format wrapper's setChunk
        // by the time we run, so the only thing left is logging.
        // `eprintln!` is deliberate - `truce-core` is the audio-runtime
        // crate, no `log` dep, and a state-load failure is a one-shot
        // event not a per-block hot path. Format wrappers that surface
        // this to the host (e.g. CLAP's `state_load` returning `false`)
        // do so synchronously *before* the queue handoff.
        eprintln!("truce: load_state failed: {e}");
    }
}

/// Apply just the parameter values from a deserialized state - the
/// host-thread-safe subset of [`apply_state`]. Format wrappers call
/// this from their state-load callback (host main thread) before
/// pushing the full state onto the audio-thread handoff queue, so
/// host-thread reads of `getParameter`/equivalents see the restored
/// values immediately. Validators (auval, pluginval, the VST2 binary
/// smoke) read parameters synchronously after `setChunk`/equivalents
/// without first running a render block, and would otherwise see the
/// pre-restore values until the audio thread caught up.
///
/// The extra blob still has to round-trip through the audio thread
/// because [`crate::plugin::PluginRuntime::load_state`] takes `&mut P`, which
/// would alias `process()`'s `&mut P` if called from the host thread.
/// `restore_values` and `snap_smoothers` go through atomic interior
/// mutability and are safe to call concurrently with `process()`.
pub fn apply_params<P: truce_params::Params>(params: &P, state: &DeserializedState) {
    params.restore_values(&state.params);
    params.snap_smoothers();
}

// ---------------------------------------------------------------------------
// `snapshot_plugin` / `restore_plugin` - high-level helpers wrapping
// `serialize_state` + `deserialize_state` with the params-collect /
// restore + custom-state plumbing every host needs to do anyway.
// ---------------------------------------------------------------------------

use crate::export::PluginExport;
use truce_params::Params;

/// Errors `restore_plugin` can return.
///
/// `Invalid` covers envelope-level failures (missing / wrong magic,
/// version mismatch, plugin-ID mismatch, truncated body); `LoadState`
/// covers a successfully-parsed envelope whose extra-state blob the
/// plugin's [`crate::PluginRuntime::load_state`] rejected. The caller
/// typically prints a diagnostic and proceeds with default params.
#[derive(Debug)]
pub enum RestoreError {
    /// The bytes don't parse as a state envelope for this plugin.
    Invalid,
    /// Envelope parsed but the plugin couldn't interpret its extra
    /// bytes.
    LoadState(StateLoadError),
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid => f.write_str("state envelope is invalid"),
            Self::LoadState(e) => write!(f, "plugin load_state failed: {e}"),
        }
    }
}

impl std::error::Error for RestoreError {}

/// Serialize a plugin instance into the canonical state envelope -
/// parameter values + optional `Plugin::save_state()` payload, with
/// the magic / version / plugin-ID header `serialize_state` writes.
///
/// Same shape every format wrapper produces, so a `.state` file
/// written by one host loads in any other (subject to the
/// plugin-ID match `deserialize_state` enforces).
pub fn snapshot_plugin<P: PluginExport>(plugin: &P) -> Vec<u8> {
    let (ids, values) = plugin.params().collect_values();
    let extra = plugin.save_state();
    serialize_state(hash_plugin_id(P::info().clap_id), &ids, &values, &extra)
}

/// Inverse of [`snapshot_plugin`]. Validates the envelope's magic,
/// version, and plugin-ID hash; on success restores parameter
/// values via `Params::restore_values` and forwards the optional
/// extra payload to `Plugin::load_state`.
///
/// # Errors
///
/// Returns [`RestoreError::Invalid`] if the magic / version /
/// plugin-ID hash check fails or the envelope is truncated. A
/// successful return guarantees the params and (optional) extra
/// payload were forwarded to the plugin.
pub fn restore_plugin<P: PluginExport>(plugin: &mut P, bytes: &[u8]) -> Result<(), RestoreError> {
    let id = hash_plugin_id(P::info().clap_id);
    let s = deserialize_state(bytes, id).ok_or(RestoreError::Invalid)?;
    plugin.params().restore_values(&s.params);
    if let Some(extra) = s.extra {
        plugin.load_state(&extra).map_err(RestoreError::LoadState)?;
    }
    Ok(())
}

/// Resolve the state-envelope hash every format wrapper stamps into
/// the saved blob. Today this is just `hash_plugin_id(info.clap_id)`,
/// which means the same plugin built as CLAP / VST3 / AU / AAX / VST2
/// / LV2 produces a single state space - saving in one host and
/// loading in another will round-trip parameter values (provided the
/// `Plugin::save_state` / `load_state` extra payload is also
/// format-agnostic).
///
/// **Trade-off:** because the input is the CLAP ID, renaming
/// `info.clap_id` invalidates **every** saved session across **every**
/// format. Callers that want format-pinned state (e.g. an AU build
/// that shouldn't share state with the same plugin's CLAP build)
/// should add a per-format ID field to [`crate::PluginInfo`] and
/// route through it instead.
#[must_use]
pub fn shared_plugin_state_hash(info: &crate::PluginInfo) -> u64 {
    hash_plugin_id(info.clap_id)
}
