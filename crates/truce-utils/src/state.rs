//! Canonical plugin-state wire format.
//!
//! Lives in `truce-utils` (rather than `truce-core`) so build-time
//! tools - `cargo-truce` emits factory-preset files containing this
//! envelope at install time - can produce byte-identical blobs
//! without inheriting `truce-core`'s runtime dependency chain.
//! `truce-core::state` re-exports everything here and layers the
//! plugin-coupled helpers (`apply_state`, `snapshot_plugin`, ...)
//! on top.

/// Magic bytes for state identification.
const STATE_MAGIC: &[u8; 4] = b"OAST";
const STATE_VERSION: u32 = 1;

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

/// The 16-byte VST3 class ID for a plugin ID string.
///
/// FNV-1a-128, per <http://www.isthe.com/chongo/tech/comp/fnv/>.
/// Standard constants - DAWs persist this CID as the plugin's identity
/// in saved sessions and `.vstpreset` files, so the algorithm and
/// constants must stay stable across releases. `truce-vst3` reports
/// this as the component's TUID; `cargo-truce` stamps the same bytes
/// (hex-encoded) into emitted `.vstpreset` headers.
#[must_use]
pub fn vst3_cid(id: &str) -> [u8; 16] {
    const FNV_OFFSET_BASIS: u128 = 0x6C62_272E_07BB_0142_62B8_2175_6295_C58D;
    const FNV_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013B;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in id.bytes() {
        hash ^= u128::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash.to_le_bytes()
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
