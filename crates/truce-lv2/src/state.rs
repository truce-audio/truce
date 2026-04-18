//! LV2 State extension (http://lv2plug.in/ns/ext/state).
//!
//! Hosts call `save()` to serialize and `restore()` to re-hydrate plugin
//! state, passing opaque store/retrieve function pointers along with a
//! handle. We stash a single blob of truce's standard `serialize_state`
//! output under a well-known URI.

use std::ffi::{c_char, c_void, CString};

use truce_core::export::PluginExport;
use truce_core::state::{deserialize_state, serialize_state};
use truce_params::Params;

use crate::urid::Urid;
use crate::Lv2Instance;

pub(crate) const LV2_STATE__INTERFACE_URI: &str = "http://lv2plug.in/ns/ext/state#interface";

const TRUCE_STATE_KEY_URI: &str = "urn:truce:state-blob";

/// `LV2_State_Status` — returned by store/retrieve + our save/restore.
const LV2_STATE_SUCCESS: u32 = 0;
const _LV2_STATE_ERR_UNKNOWN: u32 = 1;

/// `LV2_State_Flags` — bit 0 is POD (we always are), bit 1 is PORTABLE.
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
    // Monomorphized static per-P — safe because no captured state.
    struct Holder<P>(std::marker::PhantomData<P>);
    impl<P: PluginExport> Holder<P> {
        const IFACE: Lv2StateInterface = Lv2StateInterface {
            save: save_cb::<P>,
            restore: restore_cb::<P>,
        };
    }
    &<Holder<P>>::IFACE
}

unsafe extern "C" fn save_cb<P: PluginExport>(
    instance: *mut c_void,
    store: StoreFn,
    handle: *mut c_void,
    _flags: u32,
    _features: *const *const crate::types::LV2Feature,
) -> u32 {
    if instance.is_null() {
        return 0;
    }
    let inst = &mut *(instance as *mut Lv2Instance<P>);
    let (ids, values) = inst.plugin.params().collect_values();
    let extra = inst.plugin.save_state();
    let blob = serialize_state(inst.plugin_id_hash, &ids, &values, extra.as_deref());

    let key = inst.urid_map.intern(TRUCE_STATE_KEY_URI);
    let chunk_urid = inst.urid_map.atom_chunk;
    if key == 0 || chunk_urid == 0 {
        return 0;
    }
    let flags = LV2_STATE_IS_POD | LV2_STATE_IS_PORTABLE;
    let _ = store(
        handle,
        key,
        blob.as_ptr() as *const c_void,
        blob.len(),
        chunk_urid,
        flags,
    );
    LV2_STATE_SUCCESS
}

unsafe extern "C" fn restore_cb<P: PluginExport>(
    instance: *mut c_void,
    retrieve: RetrieveFn,
    handle: *mut c_void,
    _flags: u32,
    _features: *const *const crate::types::LV2Feature,
) -> u32 {
    if instance.is_null() {
        return 0;
    }
    let inst = &mut *(instance as *mut Lv2Instance<P>);
    let key = inst.urid_map.intern(TRUCE_STATE_KEY_URI);
    if key == 0 {
        return 0;
    }
    let mut size = 0usize;
    let mut type_: Urid = 0;
    let mut state_flags: u32 = 0;
    let data = retrieve(handle, key, &mut size, &mut type_, &mut state_flags);
    if data.is_null() || size == 0 {
        return 0;
    }
    let slice = core::slice::from_raw_parts(data as *const u8, size);
    if let Some(state) = deserialize_state(slice, inst.plugin_id_hash) {
        inst.plugin.params().restore_values(&state.params);
        inst.plugin.params().snap_smoothers();
        if let Some(extra) = state.extra {
            inst.plugin.load_state(&extra);
        }
    }
    LV2_STATE_SUCCESS
}

// Quiet unused-import for future generic symbol lookups.
const _: Option<CString> = None;
const _: Option<*const c_char> = None;
