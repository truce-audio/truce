use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};

use crate::info::ParamInfo;
use crate::smooth::{Smoother, SmoothingStyle};

/// Atomic f64 — wraps AtomicU64 with f64 load/store.
pub struct AtomicF64 {
    bits: std::sync::atomic::AtomicU64,
}

impl AtomicF64 {
    pub fn new(value: f64) -> Self {
        Self {
            bits: std::sync::atomic::AtomicU64::new(value.to_bits()),
        }
    }

    #[inline]
    pub fn load(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn store(&self, value: f64) {
        self.bits.store(value.to_bits(), Ordering::Relaxed);
    }
}

/// A continuous floating-point parameter.
pub struct FloatParam {
    pub info: ParamInfo,
    value: AtomicF64,
    pub smoother: Smoother,
}

impl FloatParam {
    pub fn new(info: ParamInfo, smoothing: SmoothingStyle) -> Self {
        let default = info.default_plain;
        let smoother = Smoother::new(smoothing);
        smoother.snap(default);
        Self {
            info,
            value: AtomicF64::new(default),
            smoother,
        }
    }

    /// Current raw value. Safe from any thread.
    #[inline]
    pub fn value(&self) -> f32 {
        self.value.load() as f32
    }

    /// Set the plain value (used by host automation).
    #[inline]
    pub fn set_value(&self, v: f64) {
        self.value.store(v);
    }

    /// Next smoothed value. Call once per sample in process().
    #[inline]
    pub fn smoothed_next(&self) -> f32 {
        let target = self.value.load();
        self.smoother.next(target)
    }

    /// Current smoothed value without advancing.
    #[inline]
    pub fn smoothed(&self) -> f32 {
        self.smoother.current()
    }

    /// Parameter ID.
    pub fn id(&self) -> u32 {
        self.info.id
    }
}

/// A boolean parameter.
pub struct BoolParam {
    pub info: ParamInfo,
    value: AtomicBool,
}

impl BoolParam {
    pub fn new(info: ParamInfo) -> Self {
        let default = info.default_plain != 0.0;
        Self {
            info,
            value: AtomicBool::new(default),
        }
    }

    pub fn value(&self) -> bool {
        self.value.load(Ordering::Relaxed)
    }

    pub fn set_value(&self, v: bool) {
        self.value.store(v, Ordering::Relaxed);
    }

    pub fn id(&self) -> u32 {
        self.info.id
    }
}

/// An integer parameter.
pub struct IntParam {
    pub info: ParamInfo,
    value: AtomicI64,
}

impl IntParam {
    pub fn new(info: ParamInfo) -> Self {
        let default = info.default_plain as i64;
        Self {
            info,
            value: AtomicI64::new(default),
        }
    }

    pub fn value(&self) -> i64 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn set_value(&self, v: i64) {
        self.value.store(v, Ordering::Relaxed);
    }

    pub fn id(&self) -> u32 {
        self.info.id
    }
}

/// Trait for enums used as parameters.
pub trait ParamEnum: Clone + Copy + Send + Sync + 'static {
    fn from_index(index: usize) -> Self;
    fn to_index(&self) -> usize;
    fn name(&self) -> &'static str;
    fn variant_count() -> usize;
    fn variant_names() -> &'static [&'static str];
}

/// An enum parameter.
pub struct EnumParam<E: ParamEnum> {
    pub info: ParamInfo,
    value: AtomicU32,
    _phantom: std::marker::PhantomData<E>,
}

impl<E: ParamEnum> EnumParam<E> {
    pub fn new(info: ParamInfo) -> Self {
        let default = info.default_plain as u32;
        Self {
            info,
            value: AtomicU32::new(default),
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn value(&self) -> E {
        E::from_index(self.value.load(Ordering::Relaxed) as usize)
    }

    pub fn set_value(&self, v: E) {
        self.value.store(v.to_index() as u32, Ordering::Relaxed);
    }

    pub fn set_index(&self, idx: u32) {
        self.value.store(idx, Ordering::Relaxed);
    }

    pub fn index(&self) -> u32 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn id(&self) -> u32 {
        self.info.id
    }

    /// Format a plain value (index as f64) to the variant name string.
    ///
    /// Used by the `#[derive(Params)]` macro for default `format_value` on enum fields.
    pub fn format_by_index(&self, value: f64) -> String {
        E::from_index(value.round() as usize).name().to_string()
    }
}
