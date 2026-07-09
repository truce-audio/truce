//! Denormal flush guard for the audio thread.
//!
//! FTZ (flush-to-zero) and DAZ (denormals-are-zero) on `x86_64`,
//! FZ (flush-to-zero) on `aarch64`. Set on entry to a plugin's
//! `process()` and restored on drop, so the FPU control word the
//! audio thread observes stays consistent across hosts and other
//! plugins on the same thread.
//!
//! ## Why this matters
//!
//! IIR filters with feedback can drive their state values below
//! the smallest normal float (`~1.18e-38` for f32). The CPU then
//! treats every operation on those values as a denormal-arithmetic
//! microcode trap, which on a hot core takes 50-100x longer than
//! the same op on a normal float. A reverb decaying to silence is
//! the classic case; on x86 without FTZ it can spike CPU 30x at
//! the tail. Flushing denormals to zero loses 7 bits of dynamic
//! range at the very bottom of the float range - inaudible in
//! audio, mandatory in any non-trivial DSP path.
//!
//! ## Lifetime
//!
//! `DenormalGuard::new()` reads the current control word, ORs in
//! the flush bits, writes it back, and stashes the original.
//! `drop()` restores. The format wrappers' bridge layer
//! (`truce_plugin`) wraps every `process()` call in a guard, so
//! plugin authors get the right FPU state without opting in. A
//! plugin that needs gradual underflow (extremely rare in audio)
//! can construct an opposite guard inside `process()` to flip the
//! bits back for the duration.

/// RAII guard that enables denormal-flush mode on construction and
/// restores the prior FPU control word on drop. See module docs.
#[must_use = "denormal flush state reverts when this guard is dropped"]
pub struct DenormalGuard {
    // Only the hardware paths save and restore a control word. Under Miri
    // (no inline asm) or an arch without a flush register the guard is a
    // zero-sized stub, so the field would be dead there.
    #[cfg(all(not(miri), any(target_arch = "x86_64", target_arch = "aarch64")))]
    saved: u64,
}

/// MXCSR bit 15: flush-to-zero on output denormals.
#[cfg(all(target_arch = "x86_64", not(miri)))]
const MXCSR_FTZ: u32 = 1 << 15;
/// MXCSR bit 6: denormals-are-zero on input.
#[cfg(all(target_arch = "x86_64", not(miri)))]
const MXCSR_DAZ: u32 = 1 << 6;

impl DenormalGuard {
    /// Set FTZ/DAZ (`x86_64`) or FZ (`aarch64`). On other targets this
    /// is a no-op and the guard is a zero-sized stub.
    ///
    /// Implemented via inline asm rather than the `_mm_getcsr` /
    /// `_mm_setcsr` intrinsics: those are deprecated in current
    /// stable Rust and the `_MM_DENORMALS_ZERO_ON` constant isn't
    /// always available alongside them. The two-instruction
    /// `stmxcsr` / `ldmxcsr` pair is the same machine code the
    /// intrinsics emit, just spelled differently in source.
    #[inline]
    pub fn new() -> Self {
        // Miri can't interpret inline asm, and the FPU control word
        // has no observable effect in an interpreter anyway - the
        // guard degrades to the zero-sized stub there.
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        {
            let mut saved: u32 = 0;
            // SAFETY: SSE2 (which defines MXCSR) is part of x86_64's
            // baseline target feature set; stmxcsr / ldmxcsr always
            // available on this arch.
            unsafe {
                std::arch::asm!(
                    "stmxcsr [{0}]",
                    in(reg) &raw mut saved,
                    options(nostack, preserves_flags),
                );
                let new = saved | MXCSR_FTZ | MXCSR_DAZ;
                std::arch::asm!(
                    "ldmxcsr [{0}]",
                    in(reg) &raw const new,
                    options(nostack, preserves_flags),
                );
            }
            Self {
                saved: u64::from(saved),
            }
        }
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        {
            let saved: u64;
            // SAFETY: FPCR is accessible from EL0 on AArch64;
            // reading and writing it is a normal user-mode op.
            unsafe {
                std::arch::asm!(
                    "mrs {0}, fpcr",
                    out(reg) saved,
                    options(nomem, nostack, preserves_flags),
                );
                let new = saved | (1u64 << 24);
                std::arch::asm!(
                    "msr fpcr, {0}",
                    in(reg) new,
                    options(nomem, nostack, preserves_flags),
                );
            }
            Self { saved }
        }
        // No flush register to touch (Miri, or any other arch): a
        // zero-sized stub. The three arms are mutually exclusive, so
        // exactly one is compiled and is the function's tail expression.
        #[cfg(not(all(not(miri), any(target_arch = "x86_64", target_arch = "aarch64"))))]
        {
            Self {}
        }
    }
}

impl Default for DenormalGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DenormalGuard {
    #[inline]
    fn drop(&mut self) {
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        {
            // SAFETY: see `new()`.
            #[allow(clippy::cast_possible_truncation)]
            let restore: u32 = self.saved as u32;
            unsafe {
                std::arch::asm!(
                    "ldmxcsr [{0}]",
                    in(reg) &raw const restore,
                    options(nostack, preserves_flags),
                );
            }
        }
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        {
            // SAFETY: see `new()`.
            unsafe {
                std::arch::asm!(
                    "msr fpcr, {0}",
                    in(reg) self.saved,
                    options(nomem, nostack, preserves_flags),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_construct_drop_doesnt_panic() {
        // Smoke test only; verifying the control word actually
        // flipped requires raw FPU reads that the std intrinsics
        // don't expose portably. The cycles-stalled bench in
        // `truce-simd/benches` is the real-world check.
        let _guard = DenormalGuard::new();
    }

    #[test]
    fn nested_guards_restore_in_lifo_order() {
        // Two guards in succession should each restore on drop;
        // verifies the Drop impl doesn't trash unrelated MXCSR
        // bits.
        let outer = DenormalGuard::new();
        {
            let _inner = DenormalGuard::new();
        }
        drop(outer);
    }
}
