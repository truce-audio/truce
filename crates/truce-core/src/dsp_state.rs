//! Structural fingerprint for hot-reload state preservation.
//!
//! A plugin's DSP state lives in a `State` type owned by the shell, not
//! the reloadable dylib. When the dylib is swapped, the shell can keep
//! the existing state and run the new code on it - but only if the two
//! builds agree on the `State` memory layout. Running new code over a
//! differently-laid-out allocation is undefined behavior, so the shell
//! compares a [`DspState::FINGERPRINT`] across the swap and preserves
//! state only on an exact match.
//!
//! Preservation is opt-in: a `State` that doesn't derive `DspState`
//! reports [`NO_PRESERVE`], which never matches, so the shell always
//! re-initializes. Derive `DspState` on the `State` struct to keep the
//! sound alive across a code-only reload. The fingerprint is
//! structural-*shallow* - it covers `State`'s own layout but not layout
//! changes hidden behind a `Box` / `Vec` / `Arc` field, which stay a UB
//! footgun in the dev loop; see [`DspState`] for the details.

/// Fingerprint value that never compares equal to a real one, forcing a
/// fresh `init` on every reload. It is the default for any `State` that
/// hasn't opted into preservation.
pub const NO_PRESERVE: u64 = 0;

/// A DSP `State` whose memory layout can be fingerprinted, so the
/// hot-reload shell can decide whether a state allocated by an older
/// dylib is safe to reuse under freshly loaded code.
///
/// Derive it with `#[derive(DspState)]`; the derive folds each of the
/// `State` struct's own field name+type tokens, plus the struct's
/// `size_of` / `align_of`, into [`FINGERPRINT`](Self::FINGERPRINT). A
/// change to `State` itself - a new / removed / reordered field, a
/// swapped field type, a `repr` or padding change - moves the
/// fingerprint, so the shell re-initializes instead of reinterpreting
/// mismatched bytes.
///
/// # The fingerprint is structural-*shallow*
///
/// It sees only `State`'s **own** layout. It does **not** follow
/// indirection into a pointee's definition. If a field is
/// `Box<Filter>` / `Vec<Filter>` / `Arc<Filter>` / `&Filter` / `String`
/// / `Rc<_>`, adding or reordering a field *inside `Filter`* leaves
/// `State`'s size, align, and the `Box<Filter>` token all unchanged - so
/// the fingerprint still matches, the shell reuses the old heap
/// allocation, and freshly loaded code reads the old `Filter` layout
/// through the pointer. **That is undefined behavior.**
///
/// This is a hot-reload dev-loop feature, and preservation is a
/// deliberate opt-in (the derive), so the practical guidance is: while
/// iterating on a plugin whose `State` holds boxed / heap-indirected
/// sub-structs, edit the top-level `State` (touch any field) when you
/// change a pointee's layout to force a clean re-init, or drop the
/// derive to disable preservation for that session.
#[diagnostic::on_unimplemented(
    message = "`{Self}` can't be a plugin's `type DspState` without a layout fingerprint",
    label = "this type has no `DspState` fingerprint",
    note = "add `#[derive(DspState)]` to it, or use `type DspState = ()` if the plugin has no DSP state"
)]
pub trait DspState {
    /// Structural identity of this `State`'s layout. Two builds that
    /// produce the same value guarantee (as far as the derive can see)
    /// an identical layout; a different value means "don't reuse".
    const FINGERPRINT: u64;
}

/// A stateless plugin (`type DspState = ()`) carries nothing to preserve,
/// so it reports [`NO_PRESERVE`] - the shell re-inits the empty state on
/// every reload (a no-op). This impl is what lets `type DspState = ()`
/// satisfy the `State: DspState` bound without a derive.
impl DspState for () {
    const FINGERPRINT: u64 = NO_PRESERVE;
}

/// Fold a field descriptor string plus the struct's size and alignment
/// into a fingerprint. The `#[derive(DspState)]` macro passes a string
/// built from each field's name and *surface* type tokens (so a rename,
/// reorder, or type swap changes it) together with `size_of` /
/// `align_of` of the whole struct (so a padding or repr change changes
/// it too). Const, so the fingerprint is a compile-time constant.
///
/// The fold sees only the tokens it is handed - it can't follow a
/// `Box<T>` / `Vec<T>` / `Arc<T>` token into `T`'s own definition, so a
/// layout change hidden behind indirection does not move the
/// fingerprint. See [`DspState`] for why that is a UB footgun and how
/// to work around it.
///
/// Never returns [`NO_PRESERVE`]: a real derived layout must not collide
/// with the "always re-init" sentinel, so a zero fold is nudged to 1.
#[must_use]
pub const fn fold_fingerprint(fields: &str, size: u64, align: u64) -> u64 {
    // FNV-1a over the field descriptor bytes.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let bytes = fields.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    // Mix in the concrete layout so a same-tokens/different-layout build
    // (unlikely, but a repr or generic-param change could) still differs.
    hash ^= size.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    hash = hash.rotate_left(17) ^ align.wrapping_mul(0xff51_afd7_ed55_8ccd);
    if hash == NO_PRESERVE { 1 } else { hash }
}

/// Whether live DSP state born under `held` may be reused under a
/// reloaded dylib reporting `reloaded`.
///
/// True only when two *real* fingerprints match exactly. [`NO_PRESERVE`]
/// never preserves - not even against itself: an opt-out state carries no
/// layout guarantee, so on a swap the old bytes can't be trusted (the
/// author may have changed the layout, which is the whole point of the
/// edit-and-reload loop) and the shell must re-initialize. Guarding the
/// sentinel here, at the one comparison site, is what makes the derive's
/// `0 -> 1` nudge in [`fold_fingerprint`] actually pay off.
#[must_use]
pub fn may_preserve(reloaded: u64, held: u64) -> bool {
    reloaded != NO_PRESERVE && reloaded == held
}

#[cfg(test)]
mod tests {
    use super::{NO_PRESERVE, may_preserve};

    #[test]
    fn no_preserve_never_matches_itself() {
        // The self-collision the guard exists to prevent: an opt-out
        // state whose layout changed between builds must re-init, not
        // reuse the old allocation under new code.
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
    fn different_real_fingerprints_reinit() {
        assert!(!may_preserve(0xdead_beef, 0xfeed_face));
    }
}
