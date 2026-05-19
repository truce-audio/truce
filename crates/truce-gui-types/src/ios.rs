//! Shared helpers used by every backend's `editor_ios.rs`. Hoisted
//! here so the three iOS editor implementations (`truce-gui`,
//! `truce-egui`, `truce-slint`) agree on the class-name hash and
//! ivar-offset lookup their dynamically-allocated Obj-C subclasses
//! depend on.

use objc2::runtime::AnyClass;

/// Touch lifecycle phase delivered by UIKit's
/// `touchesBegan/Moved/Ended/Cancelled`. Each backend translates
/// this into its own widget-input event type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchPhase {
    Began,
    Moved,
    Ended,
}

/// Tiny non-crypto hash for class-name uniqueness. `std` doesn't
/// expose a stable hash without a key, so we hand-roll FNV-1a.
/// Collisions would keep the previously registered class active for
/// both monomorphizations, which is benign here - the methods are
/// identical instantiations.
#[must_use]
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Resolve an instance variable's byte offset on `cls`.
///
/// # Safety
///
/// `cls` must be a valid registered Obj-C class and `name` must
/// reference an ivar that was added via `class_addIvar` before any
/// instance was allocated. A null lookup is treated as a logic bug
/// and panics; callers should ensure registration happened first.
pub unsafe fn ivar_offset(cls: &AnyClass, name: &core::ffi::CStr) -> usize {
    unsafe extern "C" {
        fn class_getInstanceVariable(
            cls: *const AnyClass,
            name: *const core::ffi::c_char,
        ) -> *mut core::ffi::c_void;
        fn ivar_getOffset(ivar: *mut core::ffi::c_void) -> isize;
    }
    unsafe {
        let ivar = class_getInstanceVariable(core::ptr::from_ref::<AnyClass>(cls), name.as_ptr());
        assert!(!ivar.is_null(), "ivar {name:?} not registered");
        let off = ivar_getOffset(ivar);
        usize::try_from(off).expect("non-negative ivar offset")
    }
}

#[cfg(test)]
mod tests {
    use super::fnv1a_64;

    #[test]
    fn fnv1a_64_known_vectors() {
        // FNV-1a 64-bit initial state, empty input.
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        // RFC test vector for "a".
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
