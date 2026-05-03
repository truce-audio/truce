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

    /// Current raw value, narrowed to `f32` for direct DSP use.
    /// Safe from any thread.
    ///
    /// **Precision.** Storage is `f64` (preserved across the
    /// `set_normalized` / `set_plain` / host-automation path); this
    /// accessor narrows to `f32` because the typical caller is a
    /// per-sample DSP loop that runs in `f32`. If you need full host
    /// precision (parameter export, state serialization, the format
    /// wrappers' read-back paths), call [`Self::value_f64`] instead.
    #[inline]
    pub fn value(&self) -> f32 {
        self.value.load() as f32
    }

    /// Current raw value at full `f64` precision. Safe from any
    /// thread. Prefer this over [`Self::value`] when interoperating
    /// with the host's `f64` automation channel or the
    /// `Params::get_plain` API surface.
    #[inline]
    pub fn value_f64(&self) -> f64 {
        self.value.load()
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
    /// # Panics
    ///
    /// Panics if `info.default_plain` is non-finite or doesn't
    /// round-trip through `i64`. The cast `f64 as i64` saturates
    /// silently — `default_plain = -1.0` lands on `-1` (fine), but
    /// `default_plain = 1e30` saturates to `i64::MAX` and `f64::NAN`
    /// becomes `0`. The derive populates `default_plain` from
    /// `#[param(default = ...)]`; a user-supplied float there is a
    /// programmer error, not a runtime condition we should
    /// silently absorb.
    pub fn new(info: ParamInfo) -> Self {
        let default = info.default_plain;
        assert!(
            default.is_finite(),
            "IntParam '{}' default {} is not finite",
            info.name,
            default,
        );
        let truncated = default as i64;
        assert!(
            truncated as f64 == default,
            "IntParam '{}' default {} doesn't round-trip through i64 \
             — supply an integer-valued default in the derive attribute",
            info.name,
            default,
        );
        let (lo, hi) = (info.range.min() as i64, info.range.max() as i64);
        assert!(
            truncated >= lo && truncated <= hi,
            "IntParam '{}' default {} is outside range [{}, {}]",
            info.name,
            truncated,
            lo,
            hi,
        );
        Self {
            info,
            value: AtomicI64::new(truncated),
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
    /// # Panics
    ///
    /// Panics if `info.default_plain` is non-finite, negative, or
    /// `>= E::variant_count()`. The cast `f64 as u32` saturates
    /// silently — a user-supplied `#[param(default = -1)]` would
    /// land on variant 0 without any signal that the default was
    /// invalid. Validate up front so the bug surfaces at plugin
    /// construction time.
    pub fn new(info: ParamInfo) -> Self {
        let default = info.default_plain;
        let count = E::variant_count();
        assert!(
            default.is_finite(),
            "EnumParam '{}' default {} is not finite",
            info.name,
            default,
        );
        assert!(
            default >= 0.0,
            "EnumParam '{}' default {} is negative; enum variants are \
             0-indexed",
            info.name,
            default,
        );
        let idx = default as u32;
        assert!(
            (idx as f64) == default,
            "EnumParam '{}' default {} is non-integer; supply a 0-indexed \
             variant index",
            info.name,
            default,
        );
        assert!(
            (idx as usize) < count,
            "EnumParam '{}' default {} is out of range; only {} variant(s) \
             defined",
            info.name,
            idx,
            count,
        );
        Self {
            info,
            value: AtomicU32::new(idx),
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
    /// Associated function — the dispatch is purely on `E`, no instance
    /// state is read. The `#[derive(Params)]` macro calls it as
    /// `<EnumParam<E>>::format_by_index(value)` so the field type
    /// supplies `E`.
    pub fn format_by_index(value: f64) -> String {
        E::from_index(value.round() as usize).name().to_string()
    }
}

// ---------------------------------------------------------------------------
// MeterSlot
// ---------------------------------------------------------------------------

/// A meter slot with an auto-assigned ID.
///
/// Declare in your params struct with `#[meter]`:
/// ```ignore
/// #[derive(Params)]
/// pub struct MyParams {
///     #[meter]
///     pub meter_left: MeterSlot,
/// }
/// ```
///
/// `id` is `pub` so the `#[derive(Params)]` macro can construct a
/// `MeterSlot { id: <auto-assigned> }` directly without going through
/// a `pub fn new(id)` constructor that would let user code mint
/// arbitrary slots and break the auto-assignment contract.
pub struct MeterSlot {
    pub id: u32,
}

impl MeterSlot {
    pub fn id(&self) -> u32 {
        self.id
    }
}

impl From<MeterSlot> for u32 {
    fn from(m: MeterSlot) -> u32 {
        m.id
    }
}

impl From<&MeterSlot> for u32 {
    fn from(m: &MeterSlot) -> u32 {
        m.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::info::{ParamFlags, ParamUnit};
    use crate::range::ParamRange;

    fn info(name: &'static str, range: ParamRange, default_plain: f64) -> ParamInfo {
        ParamInfo {
            id: 0,
            name,
            short_name: name,
            group: "",
            range,
            default_plain,
            flags: ParamFlags::AUTOMATABLE,
            unit: ParamUnit::None,
        }
    }

    #[derive(Clone, Copy)]
    enum E4 {
        A,
        B,
        C,
        D,
    }
    impl ParamEnum for E4 {
        fn from_index(i: usize) -> Self {
            match i {
                0 => Self::A,
                1 => Self::B,
                2 => Self::C,
                _ => Self::D,
            }
        }
        fn to_index(&self) -> usize {
            *self as usize
        }
        fn name(&self) -> &'static str {
            match self {
                Self::A => "A",
                Self::B => "B",
                Self::C => "C",
                Self::D => "D",
            }
        }
        fn variant_count() -> usize {
            4
        }
        fn variant_names() -> &'static [&'static str] {
            &["A", "B", "C", "D"]
        }
    }

    #[test]
    fn enum_param_accepts_in_range_default() {
        let p: EnumParam<E4> = EnumParam::new(info("Mode", ParamRange::Enum { count: 4 }, 2.0));
        assert_eq!(p.index(), 2);
    }

    #[test]
    #[should_panic(expected = "negative")]
    fn enum_param_rejects_negative_default() {
        let _: EnumParam<E4> = EnumParam::new(info("Mode", ParamRange::Enum { count: 4 }, -1.0));
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn enum_param_rejects_overflow_default() {
        let _: EnumParam<E4> = EnumParam::new(info("Mode", ParamRange::Enum { count: 4 }, 99.0));
    }

    #[test]
    #[should_panic(expected = "non-integer")]
    fn enum_param_rejects_fractional_default() {
        let _: EnumParam<E4> = EnumParam::new(info("Mode", ParamRange::Enum { count: 4 }, 1.5));
    }

    #[test]
    fn int_param_accepts_negative_default() {
        let p = IntParam::new(info("N", ParamRange::Discrete { min: -10, max: 10 }, -3.0));
        assert_eq!(p.value(), -3);
    }

    #[test]
    #[should_panic(expected = "round-trip")]
    fn int_param_rejects_fractional_default() {
        let _ = IntParam::new(info("N", ParamRange::Discrete { min: 0, max: 10 }, 1.5));
    }

    #[test]
    #[should_panic(expected = "outside range")]
    fn int_param_rejects_out_of_range_default() {
        let _ = IntParam::new(info("N", ParamRange::Discrete { min: 0, max: 5 }, 10.0));
    }
}
