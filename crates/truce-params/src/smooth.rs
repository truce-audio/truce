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
/// `coeff` (via `set_sample_rate` / `recalculate_coeff`). The four
/// `AtomicF64` accesses are individually atomic, but a
/// `set_sample_rate` call from the editor thread that lands
/// mid-block can leave `coeff` momentarily inconsistent with
/// `sample_rate` from the audio thread's point of view (one
/// updated, the other not). This produces at most one block of
/// "smooth in the wrong cadence" output and self-corrects on the
/// next sample. `reset()` and `process()` never run concurrently
/// per the host contract; sample-rate changes outside `reset` are
/// tolerated as best-effort.
pub struct Smoother {
    style: SmoothingStyle,
    current: AtomicF64,
    coeff: AtomicF64,
    sample_rate: AtomicF64,
}

impl Smoother {
    pub fn new(style: SmoothingStyle) -> Self {
        let s = Self {
            style,
            current: AtomicF64::new(0.0),
            coeff: AtomicF64::new(0.0),
            sample_rate: AtomicF64::new(44100.0),
        };
        // Compute the coefficient up-front against the placeholder
        // sample rate so unit tests that exercise `FloatParam` /
        // `Smoother` directly (without calling `set_sample_rate` first)
        // still produce non-zero output. The host re-runs this when
        // it calls `set_sample_rate(sr)` at activate time.
        s.recalculate_coeff();
        s
    }

    pub fn set_sample_rate(&self, sr: f64) {
        self.sample_rate.store(sr);
        self.recalculate_coeff();
    }

    /// Snap to a value immediately (used on reset/init).
    pub fn snap(&self, value: f64) {
        self.current.store(value);
    }

    /// Get next smoothed value, advancing one sample.
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
    #[inline]
    pub fn current(&self) -> f32 {
        self.current.load() as f32
    }

    fn recalculate_coeff(&self) {
        let sr = self.sample_rate.load();
        let new_coeff = match self.style {
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
        };
        self.coeff.store(new_coeff);
    }
}
