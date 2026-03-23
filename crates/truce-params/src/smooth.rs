use std::sync::atomic::{AtomicU64, Ordering};

/// Smoothing style for a parameter.
#[derive(Clone, Copy, Debug)]
pub enum SmoothingStyle {
    None,
    Linear(f64),
    Exponential(f64),
}

/// Atomic f64 for smoother fields (same pattern as AtomicF64 in types.rs).
struct SmoothAtomic {
    bits: AtomicU64,
}

impl SmoothAtomic {
    fn new(value: f64) -> Self {
        Self {
            bits: AtomicU64::new(value.to_bits()),
        }
    }

    #[inline]
    fn get(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed))
    }

    #[inline]
    fn set(&self, value: f64) {
        self.bits.store(value.to_bits(), Ordering::Relaxed);
    }
}

/// Per-parameter smoother. All methods take `&self` for interior mutability,
/// enabling use through `Arc<Params>`. Thread safety relies on the host
/// guarantee that `process()` and `reset()` never run concurrently.
pub struct Smoother {
    style: SmoothingStyle,
    current: SmoothAtomic,
    target: SmoothAtomic,
    coeff: SmoothAtomic,
    sample_rate: SmoothAtomic,
}

impl Smoother {
    pub fn new(style: SmoothingStyle) -> Self {
        Self {
            style,
            current: SmoothAtomic::new(0.0),
            target: SmoothAtomic::new(0.0),
            coeff: SmoothAtomic::new(0.0),
            sample_rate: SmoothAtomic::new(44100.0),
        }
    }

    pub fn set_sample_rate(&self, sr: f64) {
        self.sample_rate.set(sr);
        self.recalculate_coeff();
    }

    /// Snap to a value immediately (used on reset/init).
    pub fn snap(&self, value: f64) {
        self.current.set(value);
        self.target.set(value);
    }

    /// Get next smoothed value, advancing one sample.
    #[inline]
    pub fn next(&self, target: f64) -> f32 {
        self.target.set(target);
        let current = self.current.get();
        let coeff = self.coeff.get();

        let new_current = match self.style {
            SmoothingStyle::None => target,
            SmoothingStyle::Linear(_) => {
                let diff = target - current;
                if diff.abs() < 1e-8 {
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

        self.current.set(new_current);
        new_current as f32
    }

    /// Current smoothed value without advancing.
    #[inline]
    pub fn current(&self) -> f32 {
        self.current.get() as f32
    }

    fn recalculate_coeff(&self) {
        let sr = self.sample_rate.get();
        let new_coeff = match self.style {
            SmoothingStyle::None => 1.0,
            SmoothingStyle::Linear(ms) => {
                let samples = (ms / 1000.0) * sr;
                if samples > 1.0 {
                    1.0 / samples
                } else {
                    1.0
                }
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
        self.coeff.set(new_coeff);
    }
}
