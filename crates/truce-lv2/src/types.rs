//! Hand-written bindings for the LV2 core C ABI (`lv2.h`).
//!
//! The LV2 spec guarantees these types are stable — they have not changed
//! since LV2 1.0 (2011) and new extensions go through separate URIs rather
//! than modifying the core struct.

use std::ffi::{c_char, c_void};

/// Plugin instance handle — opaque `void*` on the C side.
pub type LV2Handle = *mut c_void;

/// Function pointer signatures for the descriptor's methods.
pub type InstantiateFn = unsafe extern "C" fn(
    descriptor: *const LV2Descriptor,
    sample_rate: f64,
    bundle_path: *const c_char,
    features: *const *const LV2Feature,
) -> LV2Handle;

pub type ConnectPortFn = unsafe extern "C" fn(handle: LV2Handle, port: u32, data: *mut c_void);
pub type LifecycleFn = unsafe extern "C" fn(handle: LV2Handle);
pub type RunFn = unsafe extern "C" fn(handle: LV2Handle, n_samples: u32);
pub type ExtensionDataFn = unsafe extern "C" fn(uri: *const c_char) -> *const c_void;

/// LV2's per-plugin descriptor struct. Matches the layout of
/// `LV2_Descriptor` from `lv2.h`.
#[repr(C)]
pub struct LV2Descriptor {
    pub uri: *const c_char,
    pub instantiate: InstantiateFn,
    pub connect_port: ConnectPortFn,
    pub activate: Option<LifecycleFn>,
    pub run: RunFn,
    pub deactivate: Option<LifecycleFn>,
    pub cleanup: LifecycleFn,
    pub extension_data: ExtensionDataFn,
}

unsafe impl Send for LV2Descriptor {}
unsafe impl Sync for LV2Descriptor {}

/// `LV2_Feature` — passed by the host to `instantiate` via a null-terminated
/// array. Each feature carries a URI identifying what it provides and an
/// opaque data pointer defined by that extension.
#[repr(C)]
pub struct LV2Feature {
    pub uri: *const c_char,
    pub data: *mut c_void,
}

// ---------------------------------------------------------------------------
// Well-known URIs
// ---------------------------------------------------------------------------

pub const LV2_URID__MAP: &str = "http://lv2plug.in/ns/ext/urid#map";
pub const LV2_URID__UNMAP: &str = "http://lv2plug.in/ns/ext/urid#unmap";
pub const LV2_ATOM__SEQUENCE: &str = "http://lv2plug.in/ns/ext/atom#Sequence";
pub const LV2_MIDI__MIDI_EVENT: &str = "http://lv2plug.in/ns/ext/midi#MidiEvent";
