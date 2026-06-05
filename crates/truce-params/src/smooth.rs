use crate::types::AtomicF64;

/// Smoothing style for a parameter.
#[derive(Clone, Copy, Debug)]
pub enum SmoothingStyle {
    None,
    Linear(f64),
    Exponential(f64),
}

/// Per-parameter smoother. All methods take `&self` for interior
/// mutability, enabling use through `Arc<Params>`.
///
/// **Threading.** The audio thread is the sole writer of `current`
/// (via `next` / `snap`) and the sole reader of `coeff`. The
/// editor / main thread is the sole writer of `sample_rate` and
/// `coeff` via [`Self::set_sample_rate`], which computes the new
/// coefficient locally from the supplied `sr` before storing -
/// so a concurrent audio block sees either the old (`sample_rate`,
/// `coeff`) pair or the new one, never a mid-update split. The
/// stored `sample_rate` field is informational; it isn't read in
/// the audio path, only by future writers as a freshness check.
pub struct Smoother {
    style: SmoothingStyle,
    current: AtomicF64,
    coeff: AtomicF64,
    sample_rate: AtomicF64,
}

impl Smoother {
    #[must_use]
    pub fn new(style: SmoothingStyle) -> Self {
        // Pre-compute the coefficient against a placeholder sample
        // rate so unit tests that exercise `FloatParam` / `Smoother`
        // directly (without calling `set_sample_rate` first) still
        // produce non-zero output. The host re-runs this when it
        // calls `set_sample_rate(sr)` at activate time.
        let coeff = compute_coeff(style, 44100.0);
        Self {
            style,
            current: AtomicF64::new(0.0),
            coeff: AtomicF64::new(coeff),
            sample_rate: AtomicF64::new(44100.0),
        }
    }

    pub fn set_sample_rate(&self, sr: f64) {
        // Compute coeff from the local `sr` (not from a re-loaded
        // `self.sample_rate`) so the (sample_rate, coeff) pair the
        // audio thread observes via `coeff` is always self-consistent -
        // even if a second `set_sample_rate` from a different thread
        // races. Order: stash the informational sample_rate first,
        // then publish the audio-visible coeff last.
        let new_coeff = compute_coeff(self.style, sr);
        self.sample_rate.store(sr);
        self.coeff.store(new_coeff);
    }

    /// Snap to a value immediately (used on reset/init).
    pub fn snap(&self, value: f64) {
        self.current.store(value);
    }

    /// Get next smoothed value, advancing one sample.
    // Smoothed param values stay in `[-1e10, 1e10]`; f32 precision
    // is enough for the per-sample DSP path.
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub fn next(&self, target: f64) -> f32 {
        let current = self.current.load();
        let coeff = self.coeff.load();

        let new_current = match self.style {
            SmoothingStyle::None => target,
            SmoothingStyle::Linear(_) => {
                let diff = target - current;
                // Scale the snap threshold to the value magnitude so
                // very-small-range params don't snap prematurely and
                // very-large-range params (e.g. 20 kHz cutoffs) don't
                // burn cycles on differences they can't perceive.
                // Floor at 1e-8 for targets near zero.
                let threshold = (target.abs() * 1e-6).max(1e-8);
                if diff.abs() < threshold {
                    target
                } else {
                    let step = diff * coeff;
                    if step.abs() >= diff.abs() {
                        target
                    } else {
                        current + step
                    }
                }
            }
            SmoothingStyle::Exponential(_) => current + coeff * (target - current),
        };

        self.current.store(new_current);
        new_current as f32
    }

    /// Current smoothed value without advancing.
    // See `next` for why narrowing to f32 here is invisible.
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub fn current(&self) -> f32 {
        self.current.load() as f32
    }

    /// True when the smoother's internal state matches `target`
    /// closely enough that further smoothing would be a no-op.
    ///
    /// `SmoothingStyle::None` always returns `true`. For `Linear`
    /// / `Exponential`, the comparison uses the same snap threshold
    /// `next()` applies: `(target.abs() * 1e-6).max(1e-8)`.
    /// Exponential smoothing asymptotes but never lands exactly
    /// on `target`; the threshold gates "close enough that any
    /// further step is denormal-territory".
    ///
    /// Costs one atomic load. Plugin authors typically reach this
    /// through [`crate::types::FloatParam::is_smoothing`] which
    /// loads the target and inverts the answer.
    #[inline]
    #[must_use]
    pub fn is_converged(&self, target: f64) -> bool {
        match self.style {
            SmoothingStyle::None => true,
            SmoothingStyle::Linear(_) | SmoothingStyle::Exponential(_) => {
                let current = self.current.load();
                let threshold = (target.abs() * 1e-6).max(1e-8);
                (target - current).abs() < threshold
            }
        }
    }

    /// Advance the smoother by `n_samples` samples in one call,
    /// returning only the final value. Use for **block-rate**
    /// consumers (hard gates, mode switches, anything that needs a
    /// single smoothed value per audio block) where the intermediate
    /// envelope from [`Self::next_block`] is wasted work.
    ///
    /// One atomic load and one atomic store regardless of
    /// `n_samples`. For `Exponential`, uses the closed-form
    /// `current + (target - current) * (1 - (1 - coeff)^N)` (one
    /// `powf` per call) instead of looping; for `Linear`, loops
    /// because the snap-when-close-enough check breaks any clean
    /// closed form.
    ///
    /// Semantics match `next` step-for-step: equivalent to calling
    /// `next(target)` `n_samples` times and returning the last
    /// result, but without paying per-sample atomic costs.
    // Smoother state stays in `[-1e10, 1e10]`; the f32 narrowing
    // matches `next` / `next_block`.
    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_precision_loss)]
    #[inline]
    pub fn next_after(&self, target: f64, n_samples: usize) -> f32 {
        if n_samples == 0 {
            return self.current.load() as f32;
        }

        let mut current = self.current.load();
        let coeff = self.coeff.load();

        match self.style {
            SmoothingStyle::None => {
                current = target;
            }
            SmoothingStyle::Linear(_) => {
                // Same per-step math as `next_block`, including the
                // snap-when-close-enough check. Looped because the
                // snap branch wrecks any closed-form derivation.
                let threshold = (target.abs() * 1e-6).max(1e-8);
                for _ in 0..n_samples {
                    let diff = target - current;
                    if diff.abs() < threshold {
                        current = target;
                        break;
                    }
                    let step = diff * coeff;
                    current = if step.abs() >= diff.abs() {
                        target
                    } else {
                        current + step
                    };
                }
            }
            SmoothingStyle::Exponential(_) => {
                // Closed form: N iterations of `current += coeff *
                // (target - current)` converge to
                // `target + (current - target) * (1 - coeff)^N`.
                let decay = (1.0 - coeff).powf(n_samples as f64);
                current = target + (current - target) * decay;
            }
        }

        self.current.store(current);
        current as f32
    }

    /// Advance the smoother by `N` samples in one call, returning the
    /// intermediate per-sample values as a stack-allocated array.
    ///
    /// Issues exactly **one** atomic load and **one** atomic store
    /// against `current`, regardless of `N`. The inner stepping runs
    /// in a register-resident loop the optimizer can unroll and (for
    /// `Exponential` / `None`) vectorize. Compare with [`Self::next`]
    /// which costs one load + one store *per sample* and therefore
    /// forces the compiler to keep `current` in memory across
    /// iterations.
    ///
    /// Semantics match `next` step-for-step: the i-th element of the
    /// returned array is what `next(target)` would have produced if
    /// called for the i-th time in sequence.
    // Smoother state stays in `[-1e10, 1e10]`; the f32 narrowing
    // matches the per-sample `next()` contract.
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub fn next_block<const N: usize>(&self, target: f64) -> [f32; N] {
        let mut out = [0.0_f32; N];
        self.next_into(target, &mut out);
        out
    }

    /// Advance the smoother by `out.len()` samples in one call,
    /// writing each intermediate value to `out`. Slice-based variant
    /// of [`Self::next_block`] - same single-atomic-pair amortization,
    /// runtime length. Use this when the chunk size depends on
    /// `process()`'s actual block (the common case for plugins
    /// chunking the host's buffer into a `MAX_BLOCK` ladder); the
    /// const-generic `next_block::<N>` always advances by `N` even
    /// when the caller only consumes a shorter prefix.
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub fn next_into(&self, target: f64, out: &mut [f32]) {
        let mut current = self.current.load();
        let coeff = self.coeff.load();

        match self.style {
            SmoothingStyle::None => {
                // Snap immediately; every output is `target`.
                out.fill(target as f32);
                current = target;
            }
            SmoothingStyle::Linear(_) => {
                // Threshold matches `next()`'s per-step floor. Hoisted
                // out of the loop because it depends only on `target`.
                let threshold = (target.abs() * 1e-6).max(1e-8);
                for slot in out.iter_mut() {
                    let diff = target - current;
                    if diff.abs() < threshold {
                        current = target;
                    } else {
                        let step = diff * coeff;
                        current = if step.abs() >= diff.abs() {
                            target
                        } else {
                            current + step
                        };
                    }
                    *slot = current as f32;
                }
            }
            SmoothingStyle::Exponential(_) => {
                // Standard one-pole exponential. `current` is a local
                // (no atomic), so LLVM keeps it in a register and the
                // body auto-vectorizes for large enough slices.
                for slot in out.iter_mut() {
                    current += coeff * (target - current);
                    *slot = current as f32;
                }
            }
        }

        self.current.store(current);
    }
}

/// Pure coefficient calculation: smoothing style + sample rate →
/// per-sample step coefficient. Lifted out of `Smoother` so
/// `set_sample_rate` can compute the new coefficient against its
/// local `sr` argument without re-loading any shared state - the
/// audio thread then sees a single atomic publish of `coeff`
/// instead of a two-step (`sample_rate`, `coeff`) write.
fn compute_coeff(style: SmoothingStyle, sr: f64) -> f64 {
    match style {
        SmoothingStyle::None => 1.0,
        SmoothingStyle::Linear(ms) => {
            let samples = (ms / 1000.0) * sr;
            if samples > 1.0 { 1.0 / samples } else { 1.0 }
        }
        SmoothingStyle::Exponential(ms) => {
            let samples = (ms / 1000.0) * sr;
            if samples > 0.0 {
                1.0 - (-1.0 / samples).exp()
            } else {
                1.0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_converged_none_always_true() {
        let s = Smoother::new(SmoothingStyle::None);
        assert!(s.is_converged(0.0));
        assert!(s.is_converged(42.0));
        assert!(s.is_converged(-1e6));
    }

    #[test]
    fn is_converged_linear_after_snap() {
        let s = Smoother::new(SmoothingStyle::Linear(5.0));
        s.snap(2.5);
        assert!(s.is_converged(2.5));
        assert!(!s.is_converged(2.6));
    }

    #[test]
    fn is_converged_exponential_at_target() {
        let s = Smoother::new(SmoothingStyle::Exponential(5.0));
        s.snap(1.0);
        assert!(s.is_converged(1.0));
        // Step partway toward 2.0: still smoothing.
        let _ = s.next(2.0);
        assert!(!s.is_converged(2.0));
    }

    #[test]
    fn is_converged_threshold_scales_with_magnitude() {
        // Target near zero: floor at 1e-8.
        let s = Smoother::new(SmoothingStyle::Linear(5.0));
        s.snap(0.0);
        assert!(s.is_converged(1e-9));
        assert!(!s.is_converged(1e-7));

        // Large target: threshold scales by 1e-6.
        s.snap(20_000.0);
        assert!(s.is_converged(20_000.01));
        assert!(!s.is_converged(20_001.0));
    }

    #[test]
    fn next_after_matches_next_block_exponential() {
        // The closed-form path for Exponential should land on the
        // same value the step-by-step `next_block` produces (within
        // f32 rounding).
        const N: usize = 512;
        let stepwise = Smoother::new(SmoothingStyle::Exponential(20.0));
        stepwise.set_sample_rate(48_000.0);
        stepwise.snap(0.0);
        let block = stepwise.next_block::<N>(1.0);

        let closed = Smoother::new(SmoothingStyle::Exponential(20.0));
        closed.set_sample_rate(48_000.0);
        closed.snap(0.0);
        let after = closed.next_after(1.0, N);

        let diff = (block[N - 1] - after).abs();
        assert!(
            diff < 1e-6,
            "block last = {}, after = {}",
            block[N - 1],
            after
        );
    }

    #[test]
    fn next_into_matches_next_block_prefix() {
        // `next_into(&mut [_; n])` must produce the same per-sample
        // sequence as `next_block::<N>` for `i < n`, and must advance
        // the smoother by exactly `n` steps. Regression guard for the
        // bug that motivated `next_into`: callers chunking the host
        // buffer into a `MAX_BLOCK`-sized ladder were calling
        // `next_block::<MAX_BLOCK>` and consuming only `n` samples,
        // which silently advanced the smoother by `MAX_BLOCK` and
        // stepped the value at the next block boundary.
        const FULL: usize = 64;
        const PARTIAL: usize = 17;

        let reference = Smoother::new(SmoothingStyle::Exponential(20.0));
        reference.set_sample_rate(48_000.0);
        reference.snap(0.0);
        let block = reference.next_block::<FULL>(1.0);

        let mut buf = [0.0_f32; FULL];
        let partial = Smoother::new(SmoothingStyle::Exponential(20.0));
        partial.set_sample_rate(48_000.0);
        partial.snap(0.0);
        partial.next_into(1.0, &mut buf[..PARTIAL]);

        for i in 0..PARTIAL {
            let diff = (buf[i] - block[i]).abs();
            assert!(diff < 1e-6, "i={i}, into={}, block={}", buf[i], block[i]);
        }

        // Next sample from `partial` must equal `block[PARTIAL]` —
        // i.e. the smoother is positioned at sample PARTIAL, not at
        // sample FULL.
        let next = partial.next(1.0);
        let diff = (next - block[PARTIAL]).abs();
        assert!(diff < 1e-6, "next={next}, expected={}", block[PARTIAL]);
    }

    #[test]
    fn next_after_matches_next_block_linear() {
        const N: usize = 64;
        let stepwise = Smoother::new(SmoothingStyle::Linear(5.0));
        stepwise.set_sample_rate(48_000.0);
        stepwise.snap(0.0);
        let mut last = 0.0_f32;
        for _ in 0..N {
            last = stepwise.next(1.0);
        }

        let chunked = Smoother::new(SmoothingStyle::Linear(5.0));
        chunked.set_sample_rate(48_000.0);
        chunked.snap(0.0);
        let after = chunked.next_after(1.0, N);

        assert!(
            (last - after).abs() < 1e-6,
            "stepwise = {last}, after = {after}"
        );
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn next_after_zero_samples_is_no_op() {
        // n=0 must return current value and leave state untouched.
        // Float equality is the right check here: we want bit-exact
        // identity, not "close enough".
        let s = Smoother::new(SmoothingStyle::Exponential(5.0));
        s.set_sample_rate(48_000.0);
        s.snap(0.25);
        let before = s.current();
        let v = s.next_after(0.99, 0);
        assert_eq!(v, before);
        assert_eq!(s.current(), before);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn next_after_none_snaps_immediately() {
        let s = Smoother::new(SmoothingStyle::None);
        s.snap(0.0);
        let v = s.next_after(0.7, 1024);
        assert_eq!(v, 0.7);
        assert_eq!(s.current(), 0.7);
    }
}
