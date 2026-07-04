//! LV2 State extension (`http://lv2plug.in/ns/ext/state`).
//!
//! Hosts call `save()` to serialize and `restore()` to re-hydrate plugin
//! state, passing opaque store/retrieve function pointers along with a
//! handle. We stash a single blob of truce's standard `serialize_state`
//! output under a well-known URI.

use std::ffi::{CString, c_char, c_void};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use truce_core::export::PluginExport;
use truce_core::state::{
    DeserializedState, ForeignState, PluginFormat, deserialize_state, parse_or_migrate,
    serialize_state,
};
use truce_core::wrapper::run_extern_callback_with;
use truce_params::Params;

use crate::Lv2Instance;
use crate::urid::Urid;

pub(crate) const LV2_STATE__INTERFACE_URI: &str = "http://lv2plug.in/ns/ext/state#interface";

const TRUCE_STATE_KEY_URI: &str = "urn:truce:state-blob";

/// `LV2_State_Status` - returned by store/retrieve + our save/restore.
const LV2_STATE_SUCCESS: u32 = 0;
const LV2_STATE_ERR_UNKNOWN: u32 = 1;
const LV2_STATE_ERR_NO_PROPERTY: u32 = 5;

/// `LV2_State_Flags` - bit 0 is POD (we always are), bit 1 is PORTABLE.
const LV2_STATE_IS_POD: u32 = 1 << 0;
const LV2_STATE_IS_PORTABLE: u32 = 1 << 1;

/// Host-provided store function: `store(handle, key, value, size, type, flags)`.
type StoreFn = unsafe extern "C" fn(
    handle: *mut c_void,
    key: Urid,
    value: *const c_void,
    size: usize,
    type_: Urid,
    flags: u32,
) -> u32;

/// Host-provided retrieve function:
/// `retrieve(handle, key, size_out, type_out, flags_out) -> *const value`.
type RetrieveFn = unsafe extern "C" fn(
    handle: *mut c_void,
    key: Urid,
    size: *mut usize,
    type_: *mut Urid,
    flags: *mut u32,
) -> *const c_void;

#[repr(C)]
pub struct Lv2StateInterface {
    pub save: unsafe extern "C" fn(
        instance: *mut c_void,
        store: StoreFn,
        handle: *mut c_void,
        flags: u32,
        features: *const *const crate::types::LV2Feature,
    ) -> u32,
    pub restore: unsafe extern "C" fn(
        instance: *mut c_void,
        retrieve: RetrieveFn,
        handle: *mut c_void,
        flags: u32,
        features: *const *const crate::types::LV2Feature,
    ) -> u32,
}

/// Build (or retrieve) the state interface vtable for this plugin type.
pub(crate) fn state_interface<P: PluginExport>() -> &'static Lv2StateInterface {
    // Monomorphized static per-P - safe because no captured state.
    struct Holder<P>(std::marker::PhantomData<P>);
    impl<P: PluginExport> Holder<P> {
        const IFACE: Lv2StateInterface = Lv2StateInterface {
            save: save_cb::<P>,
            restore: restore_cb::<P>,
        };
    }
    &<Holder<P>>::IFACE
}

/// Fallback decode for hosts that hand `restore()` the preset's
/// `^^xsd:base64Binary` literal as text instead of mapping it back to
/// a raw `atom:Chunk`. lilv-based hosts (Ardour, Carla, jalv) decode
/// it for us; REAPER does not - it returns the base64 string itself,
/// usually NUL-padded. Strip everything outside the base64 alphabet,
/// decode, and deserialize against the plugin-ID hash. `None` if the
/// bytes aren't valid base64 or the decoded blob isn't our envelope.
fn decode_base64_envelope(slice: &[u8], plugin_id_hash: u64) -> Option<DeserializedState> {
    let cleaned: Vec<u8> = slice
        .iter()
        .copied()
        .filter(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
        .collect();
    let bytes = BASE64.decode(&cleaned).ok()?;
    deserialize_state(&bytes, plugin_id_hash)
}

unsafe extern "C" fn save_cb<P: PluginExport>(
    instance: *mut c_void,
    store: StoreFn,
    handle: *mut c_void,
    _flags: u32,
    _features: *const *const crate::types::LV2Feature,
) -> u32 {
    // Guard the user's `save_state()` against panics so a stray
    // `unwrap` in custom-state code reports a state error instead of
    // aborting the host across this `extern "C"` boundary.
    run_extern_callback_with::<P, u32>("LV2", "save_state", LV2_STATE_ERR_UNKNOWN, || unsafe {
        if instance.is_null() {
            return 0;
        }
        let inst = &mut *instance.cast::<Lv2Instance<P>>();
        let (ids, values) = inst.plugin.params().collect_values();
        let extra = inst.plugin.save_state();
        let blob = serialize_state(inst.plugin_id_hash, &ids, &values, &extra);

        let key = inst.urid_map.intern(TRUCE_STATE_KEY_URI);
        let chunk_urid = inst.urid_map.atom_chunk;
        if key == 0 || chunk_urid == 0 {
            return 0;
        }
        let flags = LV2_STATE_IS_POD | LV2_STATE_IS_PORTABLE;
        let _ = store(
            handle,
            key,
            blob.as_ptr().cast::<c_void>(),
            blob.len(),
            chunk_urid,
            flags,
        );
        LV2_STATE_SUCCESS
    })
}

unsafe extern "C" fn restore_cb<P: PluginExport>(
    instance: *mut c_void,
    retrieve: RetrieveFn,
    handle: *mut c_void,
    _flags: u32,
    _features: *const *const crate::types::LV2Feature,
) -> u32 {
    // Guard the user's `load_state()` / custom deserialize against
    // panics so a malformed blob reports a state error instead of
    // aborting the host across this `extern "C"` boundary.
    run_extern_callback_with::<P, u32>("LV2", "load_state", LV2_STATE_ERR_UNKNOWN, || unsafe {
        if instance.is_null() {
            return 0;
        }
        let inst = &mut *instance.cast::<Lv2Instance<P>>();
        let key = inst.urid_map.intern(TRUCE_STATE_KEY_URI);
        if key == 0 {
            return 0;
        }
        let mut size = 0usize;
        let mut type_: Urid = 0;
        let mut state_flags: u32 = 0;
        let data = retrieve(
            handle,
            key,
            &raw mut size,
            &raw mut type_,
            &raw mut state_flags,
        );
        let state = if data.is_null() || size == 0 {
            // Truce's own key is absent: a pre-truce build stored its
            // state under *its* URI, which only the developer knows -
            // probe the `[plugin.legacy_state]` `lv2_uris` and feed
            // the first hit to the plugin's `migrate_state` hook.
            probe_legacy_uris::<P>(inst, retrieve, handle)
        } else {
            let slice = core::slice::from_raw_parts(data.cast::<u8>(), size);
            // Hosts that decode the preset's `^^xsd:base64Binary` literal
            // into a raw `atom:Chunk` (lilv: Ardour, Carla, jalv) hand us
            // the envelope bytes directly, so they deserialize as-is.
            // REAPER does NOT decode the literal - it returns the base64
            // *text* (a string literal, not atom:Chunk, often NUL-padded) -
            // so when the raw bytes aren't our envelope, strip anything
            // outside the base64 alphabet and decode before retrying.
            // Only after both fail is the blob offered to the plugin's
            // `migrate_state` hook (renamed-plugin envelope, or foreign
            // bytes a legacy build stored under truce's key).
            deserialize_state(slice, inst.plugin_id_hash)
                .or_else(|| decode_base64_envelope(slice, inst.plugin_id_hash))
                .or_else(|| {
                    parse_or_migrate::<P>(
                        slice,
                        inst.plugin_id_hash,
                        PluginFormat::Lv2,
                        Some(TRUCE_STATE_KEY_URI),
                    )
                })
        };
        let Some(state) = state else {
            // Nothing parsed and `migrate_state` declined (or the
            // property was absent entirely): fail the load honestly,
            // like the other formats - a success here would leave the
            // host believing defaults are the restored session.
            return LV2_STATE_ERR_NO_PROPERTY;
        };
        inst.plugin.params().restore_values(&state.params);
        inst.plugin.params().snap_smoothers();
        if let Some(extra) = state.extra
            && let Err(e) = inst.plugin.load_state(&extra)
        {
            eprintln!("truce: lv2 load_state failed: {e}");
        }
        LV2_STATE_SUCCESS
    })
}

/// Probe the plugin's declared legacy LV2 state URIs (first present
/// wins) and offer the bytes to `migrate_state`. Called when truce's
/// own state key is absent from the host's property map - a legacy
/// build stored its state under its own URI, which truce never reads
/// unless declared in `truce.toml`'s `[plugin.legacy_state]`.
///
/// # Safety
/// `retrieve` / `handle` must be the live pair the host passed to
/// `restore()`; returned pointers are only read within this call.
unsafe fn probe_legacy_uris<P: PluginExport>(
    inst: &Lv2Instance<P>,
    retrieve: RetrieveFn,
    handle: *mut c_void,
) -> Option<DeserializedState> {
    for uri in P::info().legacy_lv2_uris {
        let key = inst.urid_map.intern(uri);
        if key == 0 {
            continue;
        }
        let mut size = 0usize;
        let mut type_: Urid = 0;
        let mut flags: u32 = 0;
        // SAFETY: host-provided retrieve with its own handle, same
        // contract as the truce-key call in `restore_cb`.
        let data = unsafe { retrieve(handle, key, &raw mut size, &raw mut type_, &raw mut flags) };
        if data.is_null() || size == 0 {
            continue;
        }
        // SAFETY: non-null host pointer valid for `size` bytes for
        // the duration of the restore call.
        let bytes = unsafe { core::slice::from_raw_parts(data.cast::<u8>(), size) };
        if let Some(migrated) = P::migrate_state(&ForeignState::Raw {
            format: PluginFormat::Lv2,
            source_key: Some(uri),
            bytes,
        }) {
            return Some(migrated.into());
        }
    }
    None
}

// Quiet unused-import for future generic symbol lookups.
const _: Option<CString> = None;
const _: Option<*const c_char> = None;

#[cfg(test)]
mod tests {
    use base64::Engine as _;

    use super::{BASE64, decode_base64_envelope};
    use truce_core::state::serialize_state;

    // REAPER hands `restore()` the preset's `xsd:base64Binary` literal
    // as undecoded base64 *text* (often NUL-terminated) rather than the
    // raw `atom:Chunk` bytes lilv produces. The fallback must recover
    // the envelope from that text.
    #[test]
    fn base64_text_envelope_round_trips() {
        let hash = 0x1234_5678_9abc_def0_u64;
        let ids = [1_u32, 2, 3];
        let values = [0.25_f64, -6.0, 8000.0];
        let blob = serialize_state(hash, &ids, &values, &[]);

        // Mimic REAPER: base64 text of the blob, NUL-padded.
        let mut text = BASE64.encode(&blob).into_bytes();
        text.push(0);

        let state = decode_base64_envelope(&text, hash).expect("base64 fallback should decode");
        assert_eq!(state.params, vec![(1, 0.25), (2, -6.0), (3, 8000.0)]);
        assert!(state.extra.is_none());
    }

    // A different plugin's preset (hash mismatch) must be rejected, not
    // silently applied to the wrong plugin.
    #[test]
    fn base64_fallback_rejects_foreign_plugin_hash() {
        let hash = 0xAAAA_BBBB_CCCC_DDDD_u64;
        let blob = serialize_state(hash, &[7_u32], &[1.0_f64], &[]);
        let text = BASE64.encode(&blob).into_bytes();
        assert!(decode_base64_envelope(&text, hash ^ 1).is_none());
    }

    // Non-base64 garbage must not panic or false-positive.
    #[test]
    fn base64_fallback_rejects_garbage() {
        assert!(decode_base64_envelope(b"not base64 !!!", 0).is_none());
        assert!(decode_base64_envelope(&[], 0).is_none());
    }
}
