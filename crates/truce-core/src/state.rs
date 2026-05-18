/// Magic bytes for state identification.
const STATE_MAGIC: &[u8; 4] = b"OAST";
const STATE_VERSION: u32 = 1;

/// Reason a [`crate::Plugin::load_state`] / `truce_gui::PluginLogic::load_state`
/// implementation failed to interpret the host-supplied extra-state
/// blob. Format wrappers receive this on the audio-thread apply path
/// and currently log it; future hosts will surface a non-success code
/// to the DAW (e.g. CLAP `state_load` returning `false`).
///
/// `Malformed` is the typical case - the blob's framing or content
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

/// Serialize plugin state: parameter values + extra state. Empty
/// `extra` slice serializes as the same `0u64` length-prefix that an
/// absent extra block would, so callers don't need an `Option`
/// wrapper to express "no extra state".
#[must_use]
pub fn serialize_state(
    plugin_id_hash: u64,
    param_ids: &[u32],
    param_values: &[f64],
    extra: &[u8],
) -> Vec<u8> {
    let mut data = Vec::new();

    // Header
    data.extend_from_slice(STATE_MAGIC);
    data.extend_from_slice(&STATE_VERSION.to_le_bytes());
    data.extend_from_slice(&plugin_id_hash.to_le_bytes());

    // Parameter block
    let count = crate::cast::len_u32(param_ids.len());
    data.extend_from_slice(&count.to_le_bytes());
    for (id, value) in param_ids.iter().zip(param_values.iter()) {
        data.extend_from_slice(&id.to_le_bytes());
        data.extend_from_slice(&value.to_le_bytes());
    }

    // Extra state block: length-prefixed, may be zero-length.
    let len = extra.len() as u64;
    data.extend_from_slice(&len.to_le_bytes());
    data.extend_from_slice(extra);

    data
}

/// Deserialized state.
pub struct DeserializedState {
    pub params: Vec<(u32, f64)>,
    pub extra: Option<Vec<u8>>,
}

/// Apply a deserialized state to a plugin: write parameter values,
/// snap smoothers, then hand the optional extra blob to
/// [`crate::plugin::Plugin::load_state`].
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
/// because [`crate::plugin::Plugin::load_state`] takes `&mut P`, which
/// would alias `process()`'s `&mut P` if called from the host thread.
/// `restore_values` and `snap_smoothers` go through atomic interior
/// mutability and are safe to call concurrently with `process()`.
pub fn apply_params<P: truce_params::Params>(params: &P, state: &DeserializedState) {
    params.restore_values(&state.params);
    params.snap_smoothers();
}

/// Deserialize plugin state.
#[must_use]
pub fn deserialize_state(data: &[u8], expected_plugin_id: u64) -> Option<DeserializedState> {
    if data.len() < 16 {
        return None;
    }

    // Check magic
    if &data[0..4] != STATE_MAGIC {
        return None;
    }

    let version = u32::from_le_bytes(data[4..8].try_into().ok()?);
    if version != STATE_VERSION {
        return None;
    }

    let plugin_id = u64::from_le_bytes(data[8..16].try_into().ok()?);
    if plugin_id != expected_plugin_id {
        return None;
    }

    let mut offset = 16;

    // Parameter block
    if offset + 4 > data.len() {
        return None;
    }
    let count = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?) as usize;
    offset += 4;

    // Cap the pre-allocation by what the remaining buffer could
    // possibly hold. Each entry is 12 bytes (`u32 id` + `f64 value`),
    // so a hostile or corrupted blob with `count = u32::MAX` (≈64 GB
    // request) is clamped to at most the remaining byte budget. The
    // per-iteration bounds check below still rejects entries that
    // overrun the buffer; this just keeps the up-front allocation
    // honest.
    let max_count = data.len().saturating_sub(offset) / 12;
    let mut params = Vec::with_capacity(count.min(max_count));
    for _ in 0..count {
        if offset + 12 > data.len() {
            return None;
        }
        let id = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
        offset += 4;
        let value = f64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
        offset += 8;
        params.push((id, value));
    }

    // Extra state block
    if offset + 8 > data.len() {
        return None;
    }
    // The wire format encodes `extra_len` as `u64`; on 32-bit
    // targets the cast may truncate, but the next branch validates
    // `offset.checked_add(extra_len)` against the buffer length.
    #[allow(clippy::cast_possible_truncation)]
    let extra_len = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?) as usize;
    offset += 8;

    let extra = if extra_len > 0 {
        // `offset + extra_len` can wrap to a small value when
        // `extra_len` is huge (host-supplied), making the comparison
        // pass even though the slice would overrun. Use `checked_add`
        // and reject overflow as malformed.
        match offset.checked_add(extra_len) {
            Some(end) if end <= data.len() => Some(data[offset..end].to_vec()),
            _ => return None,
        }
    } else {
        None
    };

    Some(DeserializedState { params, extra })
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
/// plugin's [`crate::Plugin::load_state`] rejected. The caller
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

/// Compute a simple hash of the plugin ID string for state identification.
///
/// Uses FNV-1a-64. **Do not change this without bumping the envelope's
/// `STATE_VERSION` and writing a migration:** the returned hash is
/// stored verbatim in every `.pluginstate` blob the host has saved,
/// and a different algorithm here would invalidate every shipped
/// session. If a stronger hash is ever needed, it must be selected via
/// the version byte in the envelope, not by replacing this function in
/// place.
#[must_use]
pub fn hash_plugin_id(id: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    for byte in id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3); // FNV prime
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_state() {
        let plugin_id = hash_plugin_id("com.test.plugin");
        let ids = [0u32, 1, 2];
        let values = [0.5f64, 1.0, -12.0];
        let extra = b"hello extra state";

        let data = serialize_state(plugin_id, &ids, &values, extra);
        let state = deserialize_state(&data, plugin_id).unwrap();

        assert_eq!(state.params.len(), 3);
        assert_eq!(state.params[0], (0, 0.5));
        assert_eq!(state.params[1], (1, 1.0));
        assert_eq!(state.params[2], (2, -12.0));
        assert_eq!(state.extra.unwrap(), b"hello extra state");
    }

    #[test]
    fn wrong_plugin_id_fails() {
        let plugin_id = hash_plugin_id("com.test.plugin");
        let data = serialize_state(plugin_id, &[], &[], &[]);
        assert!(deserialize_state(&data, 12345).is_none());
    }
}
