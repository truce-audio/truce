//! Best-effort layout probe for hot-reload state preservation.
//!
//! A plugin's DSP state lives in a `State` type owned by the shell, not
//! the reloadable dylib. When the dylib is swapped, the shell can keep
//! the existing state and run the new code on it - but only if the two
//! builds agree on the `State` memory layout. Running new code over a
//! differently-laid-out allocation is undefined behavior, so the shell
//! compares a fingerprint across the swap and preserves state only on an
//! exact match.
//!
//! The fingerprint is a *heuristic*, not a proof, computed automatically
//! at dylib-load time from the state type's full path + `size_of` +
//! `align_of` ([`layout_fingerprint`]). No derive, no trait, no
//! annotation. It catches every edit that moves the state's size, align,
//! or type identity - add / remove / resize a field, swap the type -
//! which is the large majority of real edits. It does **not** catch a
//! same-size field reorder, or a layout change hidden behind a `Box` /
//! `Vec` / `Arc` (the pointer's own size is unchanged). Because this only
//! runs in `--shell` dev builds and never ships, those stay a dev-loop
//! caveat: when you change a boxed sub-struct's layout, touch a `State`
//! field or do one clean rebuild to force a re-init.

/// Fingerprint value that never compares equal to a real one, forcing a
/// fresh `init` on every reload. A zero-sized state (`type DspState =
/// ()`) reports it - there is nothing to preserve.
pub const NO_PRESERVE: u64 = 0;

const FINGERPRINT_SEED: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Best-effort layout fingerprint for a DSP state type `S`. Computed at
/// dylib-load time (a runtime call, so it can use [`std::any::type_name`])
/// by folding the type's full path plus its `size_of` / `align_of`.
///
/// Returns [`NO_PRESERVE`] for a zero-sized `S` (nothing to preserve).
///
/// This is a heuristic. Two builds that agree on `S`'s path, size, and
/// align get the same value and the shell reuses the live state; any
/// difference re-inits. It therefore catches add / remove / resize of a
/// field (size moves) and a type swap or rename (path moves), but not a
/// same-size reorder or a change *behind* a pointer - see the module
/// docs.
#[must_use]
pub fn layout_fingerprint<S>() -> u64 {
    let size = std::mem::size_of::<S>();
    if size == 0 {
        return NO_PRESERVE;
    }
    let mut hash = FINGERPRINT_SEED;
    // `type_name` includes the full path and any generic arguments, so
    // swapping the state type (or a top-level generic param) moves it.
    for b in std::any::type_name::<S>().bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash ^= (size as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    hash = hash.rotate_left(17)
        ^ (std::mem::align_of::<S>() as u64).wrapping_mul(0xff51_afd7_ed55_8ccd);
    if hash == NO_PRESERVE { 1 } else { hash }
}

/// Whether live DSP state born under `held` may be reused under a
/// reloaded dylib reporting `reloaded`.
///
/// True only when two *real* fingerprints match exactly. [`NO_PRESERVE`]
/// never preserves - not even against itself: a zero-sized or opted-out
/// state carries no layout to trust, so the shell always re-initializes.
#[must_use]
pub fn may_preserve(reloaded: u64, held: u64) -> bool {
    reloaded != NO_PRESERVE && reloaded == held
}

#[cfg(test)]
mod tests {
    use super::{NO_PRESERVE, layout_fingerprint, may_preserve};

    #[test]
    fn no_preserve_never_matches_itself() {
        // The self-collision the guard exists to prevent: an opt-out /
        // zero-sized state must always re-init, never reuse old bytes.
        assert!(!may_preserve(NO_PRESERVE, NO_PRESERVE));
    }

    #[test]
    fn no_preserve_never_matches_a_real_fingerprint() {
        assert!(!may_preserve(NO_PRESERVE, 0x1234));
        assert!(!may_preserve(0x1234, NO_PRESERVE));
    }

    #[test]
    fn equal_real_fingerprints_preserve() {
        assert!(may_preserve(0xdead_beef, 0xdead_beef));
    }

    #[test]
    fn zero_sized_state_never_preserves() {
        assert_eq!(layout_fingerprint::<()>(), NO_PRESERVE);
    }

    #[test]
    fn distinct_types_get_distinct_fingerprints() {
        // Different size, or different type path, moves the fingerprint.
        assert_ne!(layout_fingerprint::<u32>(), layout_fingerprint::<u64>());
        assert_ne!(
            layout_fingerprint::<[u8; 4]>(),
            layout_fingerprint::<[u8; 8]>()
        );
        assert_ne!(layout_fingerprint::<f64>(), layout_fingerprint::<i64>());
        assert_ne!(layout_fingerprint::<u32>(), NO_PRESERVE);
    }

    #[test]
    fn same_type_is_stable() {
        assert_eq!(layout_fingerprint::<u64>(), layout_fingerprint::<u64>());
    }
}
