/// Magic bytes for state identification.
const STATE_MAGIC: &[u8; 4] = b"OAST";
const STATE_VERSION: u32 = 1;

/// Serialize plugin state: parameter values + optional extra state.
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
    let count = param_ids.len() as u32;
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

    let mut params = Vec::with_capacity(count);
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
    let extra_len = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?) as usize;
    offset += 8;

    let extra = if extra_len > 0 {
        if offset + extra_len > data.len() {
            return None;
        }
        Some(data[offset..offset + extra_len].to_vec())
    } else {
        None
    };

    Some(DeserializedState { params, extra })
}

/// Compute a simple hash of the plugin ID string for state identification.
pub fn hash_plugin_id(id: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325; // FNV-1a offset basis
    for byte in id.bytes() {
        hash ^= byte as u64;
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
