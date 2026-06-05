//! LV2 URID (URI-to-int) extension handling.
//!
//! Hosts provide a `map` function that interns strings. We fetch and
//! cache the IDs we need at `instantiate()` time so the audio-thread
//! code (`run()`, atom decoding) never has to call back into the host.

use std::ffi::{CStr, CString, c_char, c_void};

use crate::types::{LV2_ATOM__SEQUENCE, LV2_MIDI__MIDI_EVENT, LV2_URID__MAP, LV2Feature};

pub type Urid = u32;

#[repr(C)]
struct Lv2UridMapFeature {
    handle: *mut c_void,
    map: Option<unsafe extern "C" fn(handle: *mut c_void, uri: *const c_char) -> Urid>,
}

/// Cached URIDs we need during `run()` and state save/restore. Pre-fetched
/// at instantiation so the audio thread doesn't call back into the host.
#[derive(Default)]
pub struct UridMap {
    pub midi_event: Urid,
    pub atom_sequence: Urid,
    pub atom_chunk: Urid,
    // Atom value types used when reading time:Position object fields.
    pub atom_blank: Urid,
    pub atom_object: Urid,
    pub atom_bool: Urid,
    pub atom_int: Urid,
    pub atom_long: Urid,
    pub atom_float: Urid,
    pub atom_double: Urid,
    // LV2 time:* URIDs. When the host passes an atom object whose `otype`
    // matches `time_position`, we read its fields using these keys.
    pub time_position: Urid,
    pub time_bar: Urid,
    pub time_bar_beat: Urid,
    pub time_beat: Urid,
    pub time_beat_unit: Urid,
    pub time_beats_per_bar: Urid,
    pub time_beats_per_minute: Urid,
    pub time_frame: Urid,
    pub time_speed: Urid,
    // LV2 1.18+ patch:* vocabulary - the host-→-plugin parameter
    // automation path. Hosts wrap each parameter update as a
    // `patch:Set` Object whose `patch:property` is the parameter's
    // URID and whose `patch:value` is the new value (an atom-typed
    // primitive, typically `atom:Float`). The atom event's
    // `time_frames` carries the within-block sample offset.
    pub patch_set: Urid,
    pub patch_property: Urid,
    pub patch_value: Urid,
    pub patch_subject: Urid,
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
        unsafe {
            if features.is_null() {
                return UridMap::default();
            }
            let map_uri = CString::new(LV2_URID__MAP).unwrap();
            let mut handle: *mut c_void = std::ptr::null_mut();
            let mut map_fn = None;
            let mut i = 0;
            while !(*features.add(i)).is_null() {
                let feat = &**features.add(i);
                if !feat.uri.is_null() && CStr::from_ptr(feat.uri) == map_uri.as_c_str() {
                    let map_feat = feat.data as *const Lv2UridMapFeature;
                    if !map_feat.is_null() {
                        handle = (*map_feat).handle;
                        map_fn = (*map_feat).map;
                    }
                    break;
                }
                i += 1;
            }
            UridMap::from_host(handle, map_fn)
        }
    }

    /// Build directly from the resolved host map handle + function pair.
    /// Used by the UI side, which extracts the host record in a single
    /// feature-array sweep alongside the other UI-only features
    /// (`ui:parent`, `ui:resize`).
    ///
    /// Caller passes `None` for `map_fn` when the host doesn't expose
    /// URID:map; intern then becomes a no-op and every URID stays 0.
    ///
    /// # Safety
    /// `handle` and `map_fn` must be a coherent pair as produced by
    /// the host's `URID_MAP_FEATURE.data` record.
    pub unsafe fn from_host(
        handle: *mut c_void,
        map_fn: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> Urid>,
    ) -> Self {
        let mut out = UridMap {
            handle,
            map_fn,
            ..UridMap::default()
        };
        out.midi_event = out.intern(LV2_MIDI__MIDI_EVENT);
        out.atom_sequence = out.intern(LV2_ATOM__SEQUENCE);
        out.atom_chunk = out.intern("http://lv2plug.in/ns/ext/atom#Chunk");
        // Atom value types.
        out.atom_blank = out.intern("http://lv2plug.in/ns/ext/atom#Blank");
        out.atom_object = out.intern("http://lv2plug.in/ns/ext/atom#Object");
        out.atom_bool = out.intern("http://lv2plug.in/ns/ext/atom#Bool");
        out.atom_int = out.intern("http://lv2plug.in/ns/ext/atom#Int");
        out.atom_long = out.intern("http://lv2plug.in/ns/ext/atom#Long");
        out.atom_float = out.intern("http://lv2plug.in/ns/ext/atom#Float");
        out.atom_double = out.intern("http://lv2plug.in/ns/ext/atom#Double");
        // LV2 time:* vocabulary.
        out.time_position = out.intern("http://lv2plug.in/ns/ext/time#Position");
        out.time_bar = out.intern("http://lv2plug.in/ns/ext/time#bar");
        out.time_bar_beat = out.intern("http://lv2plug.in/ns/ext/time#barBeat");
        out.time_beat = out.intern("http://lv2plug.in/ns/ext/time#beat");
        out.time_beat_unit = out.intern("http://lv2plug.in/ns/ext/time#beatUnit");
        out.time_beats_per_bar = out.intern("http://lv2plug.in/ns/ext/time#beatsPerBar");
        out.time_beats_per_minute = out.intern("http://lv2plug.in/ns/ext/time#beatsPerMinute");
        out.time_frame = out.intern("http://lv2plug.in/ns/ext/time#frame");
        out.time_speed = out.intern("http://lv2plug.in/ns/ext/time#speed");
        out.patch_set = out.intern("http://lv2plug.in/ns/ext/patch#Set");
        out.patch_property = out.intern("http://lv2plug.in/ns/ext/patch#property");
        out.patch_value = out.intern("http://lv2plug.in/ns/ext/patch#value");
        out.patch_subject = out.intern("http://lv2plug.in/ns/ext/patch#subject");
        out
    }

    /// True when the host exposed a URID:map feature. False forces
    /// `intern()` to return 0 for every URI; callers can short-circuit
    /// URID-keyed lookups in that case (e.g. skip the patch:Set
    /// decoder, since its property URIDs all hash to 0).
    #[must_use]
    pub fn has_map(&self) -> bool {
        self.map_fn.is_some()
    }

    /// Intern a URI string. Returns 0 if URID:map is unavailable.
    pub fn intern(&self, uri: &str) -> Urid {
        let Some(map_fn) = self.map_fn else {
            return 0;
        };
        let Ok(c) = CString::new(uri) else {
            return 0;
        };
        unsafe { map_fn(self.handle, c.as_ptr()) }
    }
}

unsafe impl Send for UridMap {}
