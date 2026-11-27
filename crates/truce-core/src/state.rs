//! Plugin-state save / restore helpers layered on the canonical wire
//! format in [`truce_utils::state`]. The wire functions are
//! re-exported here so format wrappers and plugin code keep a single
//! import path; the envelope itself lives in `truce-utils` so
//! `cargo-truce` can emit byte-identical blobs (factory preset files)
//! without inheriting `truce-core`'s runtime dependency chain.

pub use truce_utils::state::{
    DeserializedState, StateParse, deserialize_state, hash_plugin_id, parse_state, serialize_state,
    vst3_cid,
};

/// The plugin format whose wrapper found a foreign state blob.
/// Carried in [`ForeignState::Raw`] so a `migrate_state`
/// implementation can branch per format when the old builds
/// serialized differently per format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginFormat {
    Clap,
    Vst3,
    Vst2,
    Au,
    Lv2,
    Aax,
}

/// What a format wrapper found where truce state should have been.
/// Handed to [`crate::plugin::PluginRuntime::migrate_state`] on the
/// host thread; the plugin decides whether it recognizes the bytes.
pub enum ForeignState<'a> {
    /// Bytes that aren't a truce envelope: a previous framework's
    /// state, exactly as the old build saved it.
    Raw {
        format: PluginFormat,
        /// The container key the bytes were found under, for keyed
        /// formats (AU dict key, LV2 property URI, AAX chunk id).
        /// `None` for stream formats (CLAP / VST3 / VST2).
        source_key: Option<&'a str>,
        bytes: &'a [u8],
    },
    /// A valid truce envelope whose `plugin_id_hash` doesn't match:
    /// the plugin was renamed / re-identified. Params, extra, and the
    /// `#[persist]` block are already decoded; the plugin only decides
    /// whether to accept them. Forward `persist` into the returned
    /// [`MigratedState`] to keep GUI layout / file paths / instance
    /// names across the rename.
    MismatchedEnvelope {
        plugin_id_hash: u64,
        params: &'a [(u32, f64)],
        extra: Option<&'a [u8]>,
        persist: &'a [u8],
    },
}

/// Truce-shaped state produced by a successful
/// [`crate::plugin::PluginRuntime::migrate_state`]. Rides the normal
/// restore pipeline as a synthetic [`DeserializedState`]; the next
/// save writes a regular envelope, so migration is a one-shot door,
/// not a permanent dual-format reader.
#[derive(Default)]
pub struct MigratedState {
    pub params: Vec<(u32, f64)>,
    pub extra: Option<Vec<u8>>,
    /// Serialized `#[persist]` block to restore. Leave empty when the
    /// source has no truce persist data (a foreign framework's blob);
    /// forward [`ForeignState::MismatchedEnvelope::persist`] to carry
    /// a renamed plugin's persisted fields across the migration.
    pub persist: Vec<u8>,
}

impl From<MigratedState> for DeserializedState {
    fn from(migrated: MigratedState) -> Self {
        Self {
            params: migrated.params,
            extra: migrated.extra,
            persist: migrated.persist,
        }
    }
}

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

/// Apply a deserialized state to a plugin on the host thread: write
/// parameter values, snap smoothers, then hand the custom state to
/// [`load_custom_state`]. For a full load where the caller owns the plugin
/// outright, like the standalone host's presets.
///
/// Format wrappers don't use this for a host or editor load: they apply
/// the parameters at once on the host thread ([`apply_params`]) and queue
/// the custom state, which the plugin's next owner applies with
/// [`load_custom_state`].
///
/// `#[persist]` fields are **not** applied here - `load_persist` takes a
/// lock per field that a GUI reader can hold; [`apply_params`] applies
/// them.
pub fn apply_state<P: crate::export::PluginExport>(plugin: &mut P, state: &DeserializedState) {
    use truce_params::Params;
    plugin.params().restore_values(&state.params);
    plugin.params().snap_smoothers();
    load_custom_state(plugin, state);
}

/// Hand a queued load's custom state to the plugin's `load_state` - only
/// the custom state. Every queued load had its parameters applied on the
/// host thread when it was queued ([`apply_params`]), and an editor's
/// `set_state` carries none, so restoring them again where the queue
/// drains - the top of a block, or an activate - would revert any
/// parameter change made in between (a host flush, `setParamNormalized`,
/// a GUI knob) without the host hearing of it.
///
/// Runs on the audio thread when a block drains the queue, so a
/// `load_state` failure is only a debug breadcrumb: the host already got a
/// success return from the load callback, and envelope-level failures are
/// reported synchronously in `parse_or_migrate` before the handoff.
pub fn load_custom_state<P: crate::plugin::PluginRuntime + ?Sized>(
    plugin: &mut P,
    state: &DeserializedState,
) {
    if let Some(extra) = &state.extra
        && let Err(e) = plugin.load_state(extra)
    {
        // `eprintln!` locks stderr and allocates, so never in a release /
        // shipped build.
        #[cfg(debug_assertions)]
        eprintln!("truce: load_state failed: {e}");
        #[cfg(not(debug_assertions))]
        let _ = e;
    }
}

/// Settle the custom state a restore hands the plugin: the envelope's own
/// `extra`, or - when it carries none - the plugin's
/// [`PluginRuntime::default_extra`](crate::plugin::PluginRuntime::default_extra).
///
/// The envelope stores an empty `extra` and an absent one identically, so
/// "none" covers both a preset authored with parameters only and a session
/// whose custom state serialized to nothing. Filling it here, on the host
/// thread before the load is applied or queued, turns that case into an
/// ordinary load: the wrapper can publish the bytes before the audio thread
/// drains the queue, so a save in between carries them rather than the
/// pre-load custom state (truce#251). `None` from the plugin leaves `extra`
/// absent, so `load_state` doesn't run and the instance keeps its state.
#[must_use]
pub fn with_default_extra<P: crate::plugin::PluginRuntime>(
    mut state: DeserializedState,
) -> DeserializedState {
    if state.extra.is_none() {
        state.extra = P::default_extra();
    }
    state
}

/// Parse a host-supplied state blob and, when it isn't this plugin's
/// envelope, offer it to the plugin's
/// [`crate::plugin::PluginRuntime::migrate_state`] hook. One routing
/// point for every format wrapper's state callback:
///
/// - a matching envelope loads as always;
/// - foreign bytes ([`StateParse::NotAnEnvelope`]) and renamed-plugin
///   envelopes ([`StateParse::WrongPlugin`]) go to `migrate_state`;
/// - a future envelope version and a corrupt envelope fail the load
///   (never handed to the plugin), each with its own log line.
///
/// A state that carries no custom state comes back with the plugin's
/// [`with_default_extra`] filled in.
///
/// `None` means the load failed and the wrapper must report failure
/// to the host in its own idiom. Runs on the host thread - that's
/// where `migrate_state` is allowed to do allocator-heavy parsing.
pub fn parse_or_migrate<P: PluginExport>(
    data: &[u8],
    expected_plugin_id: u64,
    format: PluginFormat,
    source_key: Option<&str>,
) -> Option<DeserializedState> {
    let parsed = match truce_utils::state::parse_state(data, expected_plugin_id) {
        StateParse::Ok(state) => Some(state),
        StateParse::NotAnEnvelope => P::migrate_state(&ForeignState::Raw {
            format,
            source_key,
            bytes: data,
        })
        .map(Into::into),
        StateParse::WrongPlugin { found, state } => {
            P::migrate_state(&ForeignState::MismatchedEnvelope {
                plugin_id_hash: found,
                params: &state.params,
                extra: state.extra.as_deref(),
                persist: &state.persist,
            })
            .map(Into::into)
        }
        StateParse::UnknownVersion(version) => {
            // Same logging rationale as `apply_state`: one-shot event,
            // no `log` dep in the audio-runtime crate.
            eprintln!(
                "truce: state blob carries envelope version {version}; this build \
                 reads version 1 - load failed"
            );
            None
        }
        StateParse::Corrupt => {
            eprintln!("truce: state blob is a corrupt truce envelope - load failed");
            None
        }
    };
    parsed.map(with_default_extra::<P>)
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
///
/// `#[persist]` fields are applied here too, and deliberately *not* on the
/// audio thread: `load_persist` takes a `RwLock::write` / `Mutex::lock`
/// per persisted field - fields a GUI thread reads at frame rate - so
/// applying it inside `process()` would let the editor block the audio
/// thread (priority inversion). It also allocates (deserializing
/// `String` / `Vec`). Running it on the host thread keeps both off the
/// real-time path.
pub fn apply_params<P: truce_params::Params>(params: &P, state: &DeserializedState) {
    params.restore_values(&state.params);
    params.load_persist(&state.persist);
    params.snap_smoothers();
}

// ---------------------------------------------------------------------------
// `snapshot_plugin` / `restore_plugin` - high-level helpers wrapping
// `serialize_state` + `deserialize_state` with the params-collect /
// restore + custom-state plumbing every host needs to do anyway.
// ---------------------------------------------------------------------------

use crate::export::{PluginExport, read_custom_state_offthread};
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
/// parameter values + optional custom-state payload, with the magic /
/// version / plugin-ID header `serialize_state` writes.
///
/// Same shape every format wrapper produces, so a `.state` file
/// written by one host loads in any other (subject to the
/// plugin-ID match `deserialize_state` enforces).
pub fn snapshot_plugin<P: PluginExport>(plugin: &P) -> Vec<u8> {
    use truce_params::Params;
    let (ids, values) = plugin.params().collect_values();
    let extra = read_custom_state_offthread(plugin);
    let persist = plugin.params().serialize_persist();
    serialize_state(
        hash_plugin_id(P::info().clap_id),
        &ids,
        &values,
        &extra,
        &persist,
    )
}

/// Inverse of [`snapshot_plugin`]. Validates the envelope's magic,
/// version, and plugin-ID hash; on success restores parameter
/// values via `Params::restore_values` and forwards the extra payload
/// (or, when the envelope carries none, the plugin's `default_extra`) to
/// `Plugin::load_state`.
///
/// # Errors
///
/// Returns [`RestoreError::Invalid`] if the magic / version /
/// plugin-ID hash check fails or the envelope is truncated. A
/// successful return guarantees the params and (optional) extra
/// payload were forwarded to the plugin.
pub fn restore_plugin<P: PluginExport>(plugin: &mut P, bytes: &[u8]) -> Result<(), RestoreError> {
    let id = hash_plugin_id(P::info().clap_id);
    let s = with_default_extra::<P>(deserialize_state(bytes, id).ok_or(RestoreError::Invalid)?);
    plugin.params().restore_values(&s.params);
    plugin.params().load_persist(&s.persist);
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

#[cfg(test)]
mod tests {
    use super::{DeserializedState, MigratedState};

    // A renamed plugin that forwards its old envelope's `#[persist]`
    // bytes must keep them: the `From` bridge used to hardcode
    // `persist: Vec::new()`, silently reverting persisted fields to
    // default on every migrated load.
    #[test]
    fn migrated_state_forwards_persist() {
        let migrated = MigratedState {
            params: vec![(0, 1.0)],
            extra: Some(vec![9, 9]),
            persist: vec![1, 2, 3],
        };
        let restored: DeserializedState = migrated.into();
        assert_eq!(restored.persist, vec![1, 2, 3]);
        assert_eq!(restored.extra.as_deref(), Some(&[9, 9][..]));
        assert_eq!(restored.params, vec![(0, 1.0)]);
    }

    #[test]
    fn migrated_state_default_has_empty_persist() {
        let restored: DeserializedState = MigratedState::default().into();
        assert_eq!(restored.persist, [] as [u8; 0]);
        assert!(restored.extra.is_none());
        assert_eq!(restored.params, [] as [(u32, f64); 0]);
    }
}
