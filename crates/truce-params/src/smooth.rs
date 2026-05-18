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
