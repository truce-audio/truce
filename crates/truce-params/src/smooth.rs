use crate::types::AtomicF64;

/// Smoothing style for a parameter.
#[derive(Clone, Copy, Debug)]
pub enum SmoothingStyle {
    None,
    /// Straight-line ramp over the given milliseconds: a constant
    /// per-sample delta that reaches the target in exactly that time,
    /// whatever the distance. The predictable choice for click-free gain
    /// fades and crossfades where a fixed-length ramp is what you want.
    Linear(f64),
    /// One-pole exponential over the given milliseconds: fast at first,
    /// asymptotic near the target (it lands only within the snap
    /// threshold, never exactly). The natural feel for most controls.
    Exponential(f64),
    /// Multiplicative (log-domain) exponential smoothing over the given
    /// milliseconds. Ramps geometrically rather than additively, so the
    /// perceived rate of change is constant - the right choice for
    /// frequency and linear-gain params where a fixed ratio, not a fixed
    /// delta, reads as "smooth". Requires strictly positive endpoints; a
    /// non-positive `current` or `target` snaps (a log ramp can't cross
    /// or touch zero).
    Logarithmic(f64),
}

/// Per-parameter smoother. All methods take `&self` for interior
/// mutability, enabling use through `Arc<Params>`.
///
/// **Threading.** `current` is advanced by the audio thread via
/// [`Self::next`] (a `Relaxed` load-modify-store) and jumped via
/// [`Self::snap`] from whichever thread applies a value: the audio
/// thread on reset / state restore, and the main thread on activate
/// and on a host state load (`snap_smoothers` under `apply_params`).
/// The `Relaxed` accesses can't tear, but a main-thread `snap` racing
/// an audio-thread `next` can be lost - so a preset load may ramp
/// toward the restored target over the next block instead of jumping
/// to it. That's benign: the target itself is already published, so
/// the value still converges within the smoothing window. `coeff` is
/// read only by the audio thread; the main thread writes `sample_rate`
/// and `coeff` via [`Self::set_sample_rate`], which computes the new
/// coefficient locally from the supplied `sr` before storing - so a
/// concurrent audio block sees either the old (`sample_rate`, `coeff`)
/// pair or the new one, never a mid-update split. The stored
/// `sample_rate` field is informational; it isn't read in the audio
/// path, only by future writers as a freshness check.
///
/// **Linear ramp state.** `Linear` needs a *constant* per-sample
/// increment to trace a straight line, but this smoother is handed the
/// target afresh each call rather than owning it, so it caches the
/// increment (`ramp_step`) and the target it was armed for
/// (`ramp_target`). A step re-arms only when the incoming target differs
/// from `ramp_target`, so a ramp spanning several blocks stays straight;
/// `snap` / `set_sample_rate` store `NaN` into `ramp_target` to force a
/// re-arm. These are touched only by the stepping methods (audio thread)
/// and invalidated from the writer thread - a lost race just re-arms one
/// step later, the same benign outcome as a lost `snap`.
pub struct Smoother {
    style: SmoothingStyle,
    current: AtomicF64,
    coeff: AtomicF64,
    sample_rate: AtomicF64,
    /// Constant per-sample increment for the active `Linear` ramp.
    ramp_step: AtomicF64,
    /// Target `ramp_step` was armed against; `NaN` forces a re-arm.
    ramp_target: AtomicF64,
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
            ramp_step: AtomicF64::new(0.0),
            // NaN so the first Linear step arms the ramp from live state.
            ramp_target: AtomicF64::new(f64::NAN),
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
        // The coefficient (and thus the Linear step size) just changed;
        // force the next step to re-arm the ramp against the new rate.
        self.ramp_target.store(f64::NAN);
    }

    /// Snap to a value immediately (used on reset/init).
    pub fn snap(&self, value: f64) {
        self.current.store(value);
        // Jumping `current` invalidates any in-flight Linear ramp: the
        // next step must re-arm from the new position, not keep the old
        // increment (which was sized for a different start).
        self.ramp_target.store(f64::NAN);
    }

    /// Arm (or re-arm) the `Linear` ramp toward `target` and return its
    /// constant per-sample increment.
    ///
    /// A straight-line ramp adds a fixed amount `(target - start) / N`
    /// each sample (`coeff == 1/N`), where `start` is where the ramp
    /// began - *not* the shrinking `diff * coeff`, which decays
    /// geometrically and is indistinguishable from `Exponential`. The
    /// increment is cached in `ramp_step` and reused until the target
    /// changes (or `snap` / `set_sample_rate` store `NaN` into
    /// `ramp_target`), so a ramp that spans several process blocks stays
    /// straight and still lands on `target` in `N` samples total.
    #[inline]
    fn arm_linear_ramp(&self, target: f64, current: f64, coeff: f64) -> f64 {
        // Exact compare on purpose: an unchanged target is bit-identical
        // across calls, and a `NaN` armed target never matches, forcing
        // the re-arm.
        #[allow(clippy::float_cmp)]
        if self.ramp_target.load() == target {
            self.ramp_step.load()
        } else {
            let step = (target - current) * coeff;
            self.ramp_step.store(step);
            self.ramp_target.store(target);
            step
        }
    }

    /// Short-circuit for any advance that can't step normally. `None` means
    /// proceed. Shared by `next` / `next_after` / `next_into` so every
    /// advance path is NaN-safe, not just per-sample `next`. Returns
    /// `Some(v)`:
    /// - when `target` is non-finite: `v = current()`, and the accumulator
    ///   is left untouched - the `Smoother` is public through the prelude,
    ///   so an author can call `next()` with their own NaN/Inf, and letting
    ///   it reach `current` would latch (or make the self-heal below
    ///   re-latch it every sample);
    /// - else when `current` is non-finite: `v = target` after snapping to
    ///   it, self-healing a NaN/Inf that slipped in (e.g. a corrupt preset).
    ///   It would otherwise latch forever, since `NaN + coeff * (target -
    ///   NaN)` stays NaN and every arm's comparisons are false against NaN.
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    fn advance_guard(&self, target: f64) -> Option<f32> {
        if !target.is_finite() {
            return Some(self.current());
        }
        if !self.current.load().is_finite() {
            self.snap(target);
            return Some(target as f32);
        }
        None
    }

    /// Get next smoothed value, advancing one sample.
    // Smoothed param values stay in `[-1e10, 1e10]`; f32 precision
    // is enough for the per-sample DSP path.
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub fn next(&self, target: f64) -> f32 {
        if let Some(v) = self.advance_guard(target) {
            return v;
        }
        let current = self.current.load();
        let coeff = self.coeff.load();

        let new_current = match self.style {
            SmoothingStyle::None => target,
            SmoothingStyle::Linear(_) => {
                // Scale the snap threshold to the value magnitude so
                // very-small-range params don't snap prematurely and
                // very-large-range params (e.g. 20 kHz cutoffs) don't
                // burn cycles on differences they can't perceive.
                // Floor at 1e-8 for targets near zero.
                let threshold = (target.abs() * 1e-6).max(1e-8);
                let step = self.arm_linear_ramp(target, current, coeff);
                linear_advance(current, target, step, threshold)
            }
            SmoothingStyle::Exponential(_) => current + coeff * (target - current),
            // One-pole exponential in the log domain: equivalent to
            // `current *= (target / current)^coeff`. Undefined for a
            // non-positive endpoint, so snap there.
            SmoothingStyle::Logarithmic(_) => {
                if current <= 0.0 || target <= 0.0 {
                    target
                } else {
                    (current.ln() + coeff * (target.ln() - current.ln())).exp()
                }
            }
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
    /// `Logarithmic` is multiplicative, so it converges on a *ratio*:
    /// the log-domain distance `|ln(current) - ln(target)|` against the
    /// same `1e-6` relative tolerance (equivalent to the linear check
    /// near convergence, but in the spirit of the log-domain step). It
    /// falls back to the linear threshold for a non-positive endpoint,
    /// which `next()` snaps rather than steps.
    ///
    /// Costs one atomic load. Plugin authors typically reach this
    /// through [`crate::types::FloatParam::is_smoothing`] which
    /// loads the target and inverts the answer.
    #[inline]
    #[must_use]
    pub fn is_converged(&self, target: f64) -> bool {
        let current = self.current.load();
        let linear_converged = || {
            let threshold = (target.abs() * 1e-6).max(1e-8);
            (target - current).abs() < threshold
        };
        match self.style {
            SmoothingStyle::None => true,
            SmoothingStyle::Linear(_) | SmoothingStyle::Exponential(_) => linear_converged(),
            SmoothingStyle::Logarithmic(_) => {
                if current > 0.0 && target > 0.0 {
                    (current.ln() - target.ln()).abs() < 1e-6
                } else {
                    linear_converged()
                }
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
        if let Some(v) = self.advance_guard(target) {
            return v;
        }

        let mut current = self.current.load();
        let coeff = self.coeff.load();

        match self.style {
            SmoothingStyle::None => {
                current = target;
            }
            SmoothingStyle::Linear(_) => {
                // Same per-step math as `next_block`: a constant increment
                // (armed once, since the target is fixed across the call)
                // added each sample, snapping on the final step. Looped
                // because the snap check wrecks any closed-form derivation.
                let threshold = (target.abs() * 1e-6).max(1e-8);
                let step = self.arm_linear_ramp(target, current, coeff);
                for _ in 0..n_samples {
                    current = linear_advance(current, target, step, threshold);
                }
            }
            SmoothingStyle::Exponential(_) => {
                // Closed form: N iterations of `current += coeff *
                // (target - current)` converge to
                // `target + (current - target) * (1 - coeff)^N`.
                let decay = (1.0 - coeff).powf(n_samples as f64);
                current = target + (current - target) * decay;
            }
            SmoothingStyle::Logarithmic(_) => {
                if current <= 0.0 || target <= 0.0 {
                    current = target;
                } else {
                    // Closed form of the log-domain one-pole, mirroring
                    // the `Exponential` arm above in log space.
                    let decay = (1.0 - coeff).powf(n_samples as f64);
                    let log_target = target.ln();
                    current = (log_target + (current.ln() - log_target) * decay).exp();
                }
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
        if let Some(v) = self.advance_guard(target) {
            out.fill(v);
            return;
        }
        let mut current = self.current.load();
        let coeff = self.coeff.load();

        match self.style {
            SmoothingStyle::None => {
                // Snap immediately; every output is `target`.
                out.fill(target as f32);
                current = target;
            }
            SmoothingStyle::Linear(_) => {
                // Threshold matches `next()`'s per-step floor. Armed once
                // (target is fixed across the block), then a constant
                // increment per sample - a straight line, not the geometric
                // decay a re-derived `diff * coeff` would trace.
                let threshold = (target.abs() * 1e-6).max(1e-8);
                let step = self.arm_linear_ramp(target, current, coeff);
                for slot in out.iter_mut() {
                    current = linear_advance(current, target, step, threshold);
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
            SmoothingStyle::Logarithmic(_) => {
                if current <= 0.0 || target <= 0.0 {
                    out.fill(target as f32);
                    current = target;
                } else {
                    // Step the one-pole in log space, exponentiating each
                    // sample back to the linear value the DSP consumes.
                    let log_target = target.ln();
                    let mut log_current = current.ln();
                    for slot in out.iter_mut() {
                        log_current += coeff * (log_target - log_current);
                        current = log_current.exp();
                        *slot = current as f32;
                    }
                }
            }
        }

        self.current.store(current);
    }
}

/// One `Linear` step: add the constant `step` toward `target`, landing
/// exactly on `target` on the final step - once the remaining distance no
/// longer exceeds one step - or once within the convergence `threshold`.
/// The overshoot clamp is what terminates the ramp on `target` instead of
/// stepping past it, and (with a constant `step`) is the guard the geometric
/// version could never trip.
#[inline]
fn linear_advance(current: f64, target: f64, step: f64, threshold: f64) -> f64 {
    let diff = target - current;
    if diff.abs() < threshold || step.abs() >= diff.abs() {
        target
    } else {
        current + step
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
        // Same one-pole coefficient as `Exponential`; `Logarithmic`
        // applies it in the log domain (see `next`).
        SmoothingStyle::Exponential(ms) | SmoothingStyle::Logarithmic(ms) => {
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
    fn linear_ramp_is_straight_and_settles_on_time() {
        // 10 ms at 48 kHz = 480 samples. A true linear ramp 0 -> 1 traces
        // a straight line (constant per-sample delta), passes 0.5 at the
        // half-way sample, and lands on the target at ~480 samples - not
        // the geometric one-pole the shrinking `diff * coeff` used to
        // produce (midpoint ~0.63, and settling ~14x later).
        let s = Smoother::new(SmoothingStyle::Linear(10.0));
        s.set_sample_rate(48_000.0);
        s.snap(0.0);

        let vals: Vec<f64> = (0..480).map(|_| f64::from(s.next(1.0))).collect();

        // Midpoint is linear (~0.5), decisively not the exponential ~0.63.
        let mid = vals[239];
        assert!(
            (mid - 0.5).abs() < 0.01,
            "midpoint {mid} should be ~0.5 (linear), not ~0.63 (exponential)"
        );

        // Every consecutive delta is the same constant ~1/480.
        let expected = 1.0 / 480.0;
        for w in vals.windows(2) {
            let d = w[1] - w[0];
            assert!(
                (d - expected).abs() < 1e-4,
                "step {d} not constant ~{expected}"
            );
        }

        // Reaches the target by the declared time, and stays.
        assert!(
            (vals[479] - 1.0).abs() < 1e-3,
            "should reach target by ~480 samples, got {}",
            vals[479]
        );
    }

    #[test]
    fn linear_ramp_stays_straight_across_blocks() {
        // Regression guard for the constant-step cache: two 100-sample
        // blocks of a 480-sample ramp must continue the same straight
        // line, not re-arm a fresh 480-sample ramp from each block's
        // start value (which would bend the slope at the boundary and
        // stretch the total time).
        let s = Smoother::new(SmoothingStyle::Linear(10.0));
        s.set_sample_rate(48_000.0);
        s.snap(0.0);

        let mut b1 = [0.0_f32; 100];
        let mut b2 = [0.0_f32; 100];
        s.next_into(1.0, &mut b1);
        s.next_into(1.0, &mut b2);

        // After 200 samples the value is ~200/480, i.e. the ramp kept
        // going rather than restarting.
        let after_200 = f64::from(b2[99]);
        assert!(
            (after_200 - 200.0 / 480.0).abs() < 1e-3,
            "after 200 samples got {after_200}, expected {}",
            200.0 / 480.0
        );

        // Slope is unchanged across the block boundary.
        let last_b1 = f64::from(b1[99] - b1[98]);
        let first_b2 = f64::from(b2[0] - b1[99]);
        assert!(
            (last_b1 - first_b2).abs() < 1e-4,
            "slope changed at block boundary: {last_b1} vs {first_b2}"
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
    fn logarithmic_converges_multiplicatively() {
        let s = Smoother::new(SmoothingStyle::Logarithmic(5.0));
        s.set_sample_rate(48_000.0);
        s.snap(100.0);
        // Ramp toward 1 kHz; the value stays positive the whole way and
        // converges to the target.
        let mut last = 0.0_f32;
        for _ in 0..4096 {
            last = s.next(1000.0);
            assert!(last > 0.0, "log smoothing must stay positive, got {last}");
        }
        assert!((last - 1000.0).abs() < 1.0, "did not converge: {last}");
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn logarithmic_snaps_on_nonpositive_endpoint() {
        // A log ramp can't touch or cross zero, so a non-positive current
        // or target snaps straight to the target.
        let s = Smoother::new(SmoothingStyle::Logarithmic(5.0));
        s.snap(-1.0);
        assert_eq!(s.next(2.0), 2.0);
        s.snap(1.0);
        assert_eq!(s.next(0.0), 0.0);
    }

    #[test]
    fn next_after_matches_next_block_logarithmic() {
        const N: usize = 512;
        let stepwise = Smoother::new(SmoothingStyle::Logarithmic(20.0));
        stepwise.set_sample_rate(48_000.0);
        stepwise.snap(100.0);
        let block = stepwise.next_block::<N>(2000.0);

        let closed = Smoother::new(SmoothingStyle::Logarithmic(20.0));
        closed.set_sample_rate(48_000.0);
        closed.snap(100.0);
        let after = closed.next_after(2000.0, N);

        assert!(
            (block[N - 1] - after).abs() < 1.0,
            "block last = {}, after = {after}",
            block[N - 1]
        );
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

    #[test]
    fn next_self_heals_from_non_finite_current() {
        // A NaN accumulator would latch forever without the recovery guard.
        let s = Smoother::new(SmoothingStyle::Exponential(5.0));
        s.snap(f64::NAN);
        let first = s.next(0.5);
        assert!(first.is_finite(), "recovers to a finite value");
        // And keeps converging normally afterward.
        for _ in 0..64 {
            assert!(s.next(0.5).is_finite());
        }
        assert!((s.current() - 0.5).abs() < 1e-3);
    }

    /// The block-rate paths (`next_into`, `next_after`, and `next_block`
    /// via `next_into`) share the same self-heal as `next` - a NaN in the
    /// accumulator can't fill a whole block with NaN.
    #[test]
    fn block_paths_self_heal_from_non_finite_current() {
        for style in [
            SmoothingStyle::Exponential(5.0),
            SmoothingStyle::Linear(5.0),
            SmoothingStyle::Logarithmic(5.0),
        ] {
            let s = Smoother::new(style);
            s.snap(f64::NAN);
            let mut out = [0.0f32; 16];
            s.next_into(0.5, &mut out);
            assert!(out.iter().all(|v| v.is_finite()), "next_into: {style:?}");

            let s = Smoother::new(style);
            s.snap(f64::INFINITY);
            assert!(s.next_after(0.5, 32).is_finite(), "next_after: {style:?}");
        }
    }

    /// A non-finite *target* (an author calling the prelude-exported
    /// `Smoother` with their own NaN) must not poison a healthy
    /// accumulator: bail to the last good value, leave `current` finite.
    #[test]
    #[allow(clippy::float_cmp)]
    fn non_finite_target_bails_without_poisoning() {
        let s = Smoother::new(SmoothingStyle::Exponential(5.0));
        s.snap(0.5);

        assert_eq!(s.next(f64::NAN), 0.5, "next keeps the last value");
        assert!(s.current().is_finite());

        let mut out = [1.0f32; 8];
        s.next_into(f64::NAN, &mut out);
        assert!(out.iter().all(|&v| v == 0.5), "next_into fills last value");
        assert!(s.current().is_finite());

        assert_eq!(s.next_after(f64::INFINITY, 64), 0.5);
        assert!(s.current().is_finite(), "accumulator stays finite");
    }
}
