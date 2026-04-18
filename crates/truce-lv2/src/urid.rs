//! LV2 URID (URI-to-int) extension handling.
//!
//! Hosts provide a `map` function that interns strings. We fetch and
//! cache the IDs we need at `instantiate()` time so the audio-thread
//! code (run(), atom decoding) never has to call back into the host.

use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;

use crate::types::{LV2Feature, LV2_ATOM__SEQUENCE, LV2_MIDI__MIDI_EVENT, LV2_URID__MAP};

pub type Urid = u32;

#[repr(C)]
struct Lv2UridMapFeature {
    handle: *mut c_void,
    map: Option<unsafe extern "C" fn(handle: *mut c_void, uri: *const c_char) -> Urid>,
}

/// Cached URIDs we need during run() and state save/restore. Pre-fetched
/// at instantiation so the audio thread doesn't call back into the host.
#[derive(Default)]
pub(crate) struct UridMap {
    pub midi_event: Urid,
    pub atom_sequence: Urid,
    pub atom_chunk: Urid,
    /// Raw map pointers preserved so `state` and dynamic code paths can
    /// intern additional URIs on demand (still on main thread).
    handle: *mut c_void,
    map_fn: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> Urid>,
}

impl UridMap {
    /// Build from the null-terminated feature array the host passed to
    /// `instantiate()`. Missing URID:map leaves everything at 0 (valid
    /// LV2 host behavior: we simply won't match any atom events).
    ///
    /// # Safety
    /// `features` is a null-terminated array of `*const LV2Feature`, or
    /// null itself.
    pub unsafe fn from_features(features: *const *const LV2Feature) -> Self {
        let mut out = UridMap::default();
        if features.is_null() {
            return out;
        }
        let map_uri = CString::new(LV2_URID__MAP).unwrap();
        let mut i = 0;
        while !(*features.add(i)).is_null() {
            let feat = &**features.add(i);
            if !feat.uri.is_null() && CStr::from_ptr(feat.uri) == map_uri.as_c_str() {
                let map_feat = feat.data as *const Lv2UridMapFeature;
                if !map_feat.is_null() {
                    out.handle = (*map_feat).handle;
                    out.map_fn = (*map_feat).map;
                }
                break;
            }
            i += 1;
        }
        out.midi_event = out.intern(LV2_MIDI__MIDI_EVENT);
        out.atom_sequence = out.intern(LV2_ATOM__SEQUENCE);
        out.atom_chunk = out.intern("http://lv2plug.in/ns/ext/atom#Chunk");
        out
    }

    /// Intern a URI string. Returns 0 if URID:map is unavailable.
    pub fn intern(&self, uri: &str) -> Urid {
        let Some(map_fn) = self.map_fn else {
            return 0;
        };
        let c = match CString::new(uri) {
            Ok(c) => c,
            Err(_) => return 0,
        };
        unsafe { map_fn(self.handle, c.as_ptr()) }
    }
}

unsafe impl Send for UridMap {}

// Default UridMap = no host interning. Safe to use — id lookups return 0
// and any event compared against them won't match.
impl UridMap {
    pub fn _placeholder() -> Self {
        UridMap {
            handle: ptr::null_mut(),
            map_fn: None,
            ..Default::default()
        }
    }
}
