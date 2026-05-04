/// Magic bytes for state identification.
const STATE_MAGIC: &[u8; 4] = b"OAST";
const STATE_VERSION: u32 = 1;

/// Serialize plugin state: parameter values + optional extra state.
#[must_use] 
pub fn serialize_state(
    plugin_id_hash: u64,
    param_ids: &[u32],
    param_values: &[f64],
    extra: Option<&[u8]>,
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

    // Extra state block
    if let Some(extra) = extra {
        let len = extra.len() as u64;
        data.extend_from_slice(&len.to_le_bytes());
        data.extend_from_slice(extra);
    } else {
        data.extend_from_slice(&0u64.to_le_bytes());
    }

    data
}

/// Deserialized state.
pub struct DeserializedState {
    pub params: Vec<(u32, f64)>,
    pub extra: Option<Vec<u8>>,
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
// `snapshot_plugin` / `restore_plugin` — high-level helpers wrapping
// `serialize_state` + `deserialize_state` with the params-collect /
// restore + custom-state plumbing every host needs to do anyway.
// ---------------------------------------------------------------------------

use crate::export::PluginExport;
use truce_params::Params;

/// Errors `restore_plugin` can return. `Invalid` covers all
/// envelope-level failures the user might care to distinguish:
/// missing / wrong magic, version mismatch, plugin-ID mismatch,
/// truncated body. The caller typically just prints a diagnostic
/// and proceeds with default params.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreError {
    /// The bytes don't parse as a state envelope for this plugin.
    Invalid,
}

/// Serialize a plugin instance into the canonical state envelope —
/// parameter values + optional `Plugin::save_state()` payload, with
/// the magic / version / plugin-ID header `serialize_state` writes.
///
/// Same shape every format wrapper produces, so a `.state` file
/// written by one host loads in any other (subject to the
/// plugin-ID match `deserialize_state` enforces).
pub fn snapshot_plugin<P: PluginExport>(plugin: &P) -> Vec<u8> {
    let (ids, values) = plugin.params().collect_values();
    let extra = plugin.save_state();
    serialize_state(
        hash_plugin_id(P::info().clap_id),
        &ids,
        &values,
        extra.as_deref(),
    )
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
        plugin.load_state(&extra);
    }
    Ok(())
}

/// Resolve the state-envelope hash a non-VST3 format wrapper should
/// stamp into the saved blob. Today this is just `hash_plugin_id(info.clap_id)`,
/// which means the same plugin built as CLAP / AU / AAX / VST2 / LV2
/// produces a single state space — saving in one host and loading in
/// another will round-trip parameter values (provided the `Plugin::save_state` /
/// `load_state` extra payload is also format-agnostic).
///
/// **Trade-off:** because the input is the CLAP ID, renaming
/// `info.clap_id` invalidates **every** saved session across all
/// non-VST3 formats. VST3 is exempt — it uses `info.vst3_id` (a
/// fixed 16-byte UID by spec) so renames there are independent.
/// Callers that want format-pinned state (e.g. an AU build that
/// shouldn't share state with the same plugin's CLAP build) should
/// add a per-format ID field to [`PluginInfo`] and route through it
/// instead.
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
    let mut hash: u64 = 0xcbf29ce484222325; // FNV-1a offset basis
    for byte in id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3); // FNV prime
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

        let data = serialize_state(plugin_id, &ids, &values, Some(extra));
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
        let data = serialize_state(plugin_id, &[], &[], None);
        assert!(deserialize_state(&data, 12345).is_none());
    }
}
