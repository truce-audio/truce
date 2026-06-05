use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use crate::info::ParamInfo;
use crate::sample::Float;
use crate::smooth::{Smoother, SmoothingStyle};

/// Atomic f64 - wraps `AtomicU64` with f64 load/store.
pub struct AtomicF64 {
    bits: AtomicU64,
}

impl AtomicF64 {
    pub fn new(value: f64) -> Self {
        Self {
            bits: AtomicU64::new(value.to_bits()),
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
    #[must_use]
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

    /// Set the plain value (used by host automation).
    #[inline]
    pub fn set_value(&self, v: f64) {
        self.value.store(v);
    }

    /// Internal: raw target value at `f64` precision (host-side
    /// surface, before any narrowing for DSP use). Plugin authors
    /// don't call this directly - they go through the prelude's
    /// `read` / `value` / `current` instead, which have no
    /// precision-suffix decisions at the call site.
    #[doc(hidden)]
    #[inline]
    pub fn raw_target(&self) -> f64 {
        self.value.load()
    }

    /// Internal: next smoother step at `f32` (the smoother's native
    /// precision). See [`Self::raw_target`].
    #[doc(hidden)]
    #[inline]
    pub fn raw_smoothed_next(&self) -> f32 {
        let target = self.value.load();
        self.smoother.next(target)
    }

    /// Internal: current smoother value at `f32`. See
    /// [`Self::raw_target`].
    #[doc(hidden)]
    #[inline]
    pub fn raw_smoothed_current(&self) -> f32 {
        self.smoother.current()
    }

    /// Internal: advance the smoother by `N` samples in one call.
    /// Plugin authors reach this through [`FloatParamReadF32::read_block`]
    /// / [`FloatParamReadF64::read_block`] in the prelude.
    #[doc(hidden)]
    #[inline]
    pub fn raw_smoothed_next_block<const N: usize>(&self) -> [f32; N] {
        let target = self.value.load();
        self.smoother.next_block::<N>(target)
    }

    /// Internal: advance the smoother by `out.len()` samples,
    /// writing each step to `out`. Plugin authors reach this through
    /// [`FloatParamReadF32::read_into`] /
    /// [`FloatParamReadF64::read_into`] in the prelude.
    #[doc(hidden)]
    #[inline]
    pub fn raw_smoothed_next_into(&self, out: &mut [f32]) {
        let target = self.value.load();
        self.smoother.next_into(target, out);
    }

    /// Internal: advance the smoother by `n_samples` and return only
    /// the final value. Plugin authors reach this through
    /// [`FloatParamReadF32::read_after`] /
    /// [`FloatParamReadF64::read_after`] in the prelude.
    #[doc(hidden)]
    #[inline]
    pub fn raw_smoothed_next_after(&self, n_samples: usize) -> f32 {
        let target = self.value.load();
        self.smoother.next_after(target, n_samples)
    }

    /// Read the value rounded to the nearest non-negative `usize`.
    /// Use this for discrete-range params consumed as array indices.
    /// Negatives, NaN, and infinities saturate at `0` / `usize::MAX`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    #[inline]
    pub fn value_usize(&self) -> usize {
        let v = self.value.load().round();
        if v <= 0.0 { 0 } else { v as usize }
    }

    /// Read the value rounded to the nearest `i32`. Out-of-range
    /// values saturate at `i32::MIN` / `i32::MAX`; NaN → 0.
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub fn value_i32(&self) -> i32 {
        self.value.load().round() as i32
    }

    /// Read the value rounded to the nearest `u8`. Negatives clamp to
    /// `0`; values above `255` saturate at `u8::MAX`; NaN → 0.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    #[inline]
    pub fn value_u8(&self) -> u8 {
        let v = self.value.load().round();
        if v <= 0.0 {
            0
        } else if v >= 255.0 {
            255
        } else {
            v as u8
        }
    }

    /// True when the smoother is mid-step toward a new target.
    /// Inverse of [`Smoother::is_converged`].
    ///
    /// Use to branch in `process()` between a constant-gain fast
    /// path (smoothers at target, gain identical across the whole
    /// block, one `gain_block` per channel) and the envelope slow
    /// path (`read_block` + per-sample envelope + `chunks_mut`).
    /// `SmoothingStyle::None` always reports `false` here, so the
    /// fast path is unconditional for plugins that disable
    /// smoothing.
    ///
    /// ```ignore
    /// if !self.params.gain.is_smoothing() && !self.params.pan.is_smoothing() {
    ///     // fast path: gain is constant for the whole block.
    /// } else {
    ///     // slow path: envelope precompute + chunked apply.
    /// }
    /// ```
    #[inline]
    #[must_use]
    pub fn is_smoothing(&self) -> bool {
        !self.smoother.is_converged(self.value.load())
    }

    /// Parameter ID.
    pub fn id(&self) -> u32 {
        self.info.id
    }
}

/// Precision-routed read accessors for [`FloatParam`] at `f32`.
///
/// The plugin prelude (`truce::prelude` / `truce::prelude32`) imports
/// this trait via `pub use … as _;`, so plugin code reads:
///
/// ```ignore
/// use truce::prelude::*;
/// let gain = self.params.gain.read();   // f32 - no annotation needed
/// ```
///
/// The trait's methods shadow nothing - `FloatParam` has no inherent
/// `read` / `value` / `current`, so name resolution picks the one
/// (and only one) trait that's in scope. Importing `prelude64`
/// instead brings [`FloatParamReadF64`] into scope and the same
/// source resolves to `f64`. Importing **both** preludes is a
/// compile error (`multiple applicable items in scope`) - which is
/// the right error for a file that hasn't committed to a precision.
pub trait FloatParamReadF32 {
    /// Next smoothed value. Call once per sample in `process()`.
    #[must_use]
    fn read(&self) -> f32;

    /// Advance the smoother by exactly `N` samples in one call,
    /// returning the per-sample values as a stack array.
    ///
    /// **Deprecated in favour of [`Self::read_into`]**, which takes a
    /// runtime-length slice and so always advances the smoother by
    /// the number of samples the caller actually consumes. The
    /// const-`N` shape is a silent footgun: a `total.min(MAX_BLOCK)`
    /// chunk ladder pulls `N` samples from the smoother every iteration
    /// but only consumes `n ≤ N`, leaving the smoother `N - n` samples
    /// ahead - audible as clicks at every block boundary on delay,
    /// LFO-rate, and any other timing-sensitive smoothed param.
    /// `read_into(&mut scratch[..N])` is the same code shape with the
    /// hazard removed.
    #[must_use]
    #[deprecated(
        since = "0.53.0",
        note = "use `read_into(&mut scratch[..n])` instead; \
                `read_block::<N>` advances the smoother by N regardless \
                of how many samples the caller consumes, which steps the \
                value at the next block boundary when the host's block \
                size isn't a multiple of N"
    )]
    fn read_block<const N: usize>(&self) -> [f32; N];

    /// Fill `out` with the next `out.len()` smoothed samples; advance
    /// the smoother by `out.len()` (not by the slice's capacity).
    /// Same one atomic load + one atomic store amortization as
    /// [`Self::read_block`]; runtime length instead of const-generic
    /// `N`. The right primitive when chunking `process()`'s block
    /// dynamically:
    ///
    /// ```ignore
    /// let mut delay = [0.0_f32; MAX_BLOCK];
    /// while offset < total {
    ///     let n = (total - offset).min(MAX_BLOCK);
    ///     self.params.delay.read_into(&mut delay[..n]);
    ///     // ... consume delay[..n] for n samples ...
    ///     offset += n;
    /// }
    /// ```
    fn read_into(&self, out: &mut [f32]);

    /// Advance the smoother by `n_samples` in one call, returning
    /// only the final value. Use for **block-rate** DSP - hard
    /// gates, mode switches, anything that needs one smoothed value
    /// per audio block. Pass `buffer.num_samples()` to keep the
    /// smoother's wall-clock convergence time matching the smoother
    /// declaration (`smooth = "exp(20)"` then actually settles in
    /// ~20 ms instead of ~20 blocks). One atomic load + one atomic
    /// store; the intermediate envelope from [`Self::read_block`]
    /// is skipped.
    #[must_use]
    fn read_after(&self, n_samples: usize) -> f32;

    /// Current smoothed value without advancing.
    #[must_use]
    fn current(&self) -> f32;

    /// Raw target value (post-`set_normalized` / host automation),
    /// not the smoothed output. Use [`Self::read`] / [`Self::current`]
    /// in the DSP loop.
    #[must_use]
    fn value(&self) -> f32;
}

/// Precision-routed read accessors for [`FloatParam`] at `f64`. See
/// [`FloatParamReadF32`] for the contract.
pub trait FloatParamReadF64 {
    #[must_use]
    fn read(&self) -> f64;
    /// f64 view of [`FloatParamReadF32::read_block`]; same
    /// deprecation rationale - use [`Self::read_into`] for any
    /// runtime-length chunk.
    #[must_use]
    #[deprecated(
        since = "0.53.0",
        note = "use `read_into(&mut scratch[..n])` instead; \
                `read_block::<N>` advances the smoother by N regardless \
                of how many samples the caller consumes"
    )]
    fn read_block<const N: usize>(&self) -> [f64; N];
    /// f64 view of [`FloatParamReadF32::read_into`]; one widen per
    /// slot on top of the same one-atomic-pair fast path.
    fn read_into(&self, out: &mut [f64]);
    /// f64 view of [`FloatParamReadF32::read_after`]; one widen
    /// on top of the same one-atomic-pair fast path.
    #[must_use]
    fn read_after(&self, n_samples: usize) -> f64;
    #[must_use]
    fn current(&self) -> f64;
    #[must_use]
    fn value(&self) -> f64;
}

impl FloatParamReadF32 for FloatParam {
    #[inline]
    fn read(&self) -> f32 {
        self.raw_smoothed_next()
    }

    #[inline]
    fn read_block<const N: usize>(&self) -> [f32; N] {
        self.raw_smoothed_next_block::<N>()
    }

    #[inline]
    fn read_into(&self, out: &mut [f32]) {
        self.raw_smoothed_next_into(out);
    }

    #[inline]
    fn read_after(&self, n_samples: usize) -> f32 {
        self.raw_smoothed_next_after(n_samples)
    }

    #[inline]
    fn current(&self) -> f32 {
        self.raw_smoothed_current()
    }

    #[inline]
    fn value(&self) -> f32 {
        f32::from_f64(self.raw_target())
    }
}

impl FloatParamReadF64 for FloatParam {
    #[inline]
    fn read(&self) -> f64 {
        f64::from(self.raw_smoothed_next())
    }

    #[inline]
    fn read_block<const N: usize>(&self) -> [f64; N] {
        let block = self.raw_smoothed_next_block::<N>();
        let mut out = [0.0_f64; N];
        for (i, &v) in block.iter().enumerate() {
            out[i] = f64::from(v);
        }
        out
    }

    #[inline]
    fn read_into(&self, out: &mut [f64]) {
        // Reuse the f32 fill via a transient stack scratch sized to
        // the largest chunk a plugin typically passes (cap to 1024 -
        // beyond that the caller almost certainly wants `read` per
        // sample). The widen is the same per-slot widen the const-N
        // `read_block::<N>` already does.
        const SCRATCH: usize = 1024;
        let mut scratch = [0.0_f32; SCRATCH];
        let mut remaining = out;
        while !remaining.is_empty() {
            let take = remaining.len().min(SCRATCH);
            self.raw_smoothed_next_into(&mut scratch[..take]);
            for (dst, &src) in remaining[..take].iter_mut().zip(&scratch[..take]) {
                *dst = f64::from(src);
            }
            remaining = &mut remaining[take..];
        }
    }

    #[inline]
    fn read_after(&self, n_samples: usize) -> f64 {
        f64::from(self.raw_smoothed_next_after(n_samples))
    }

    #[inline]
    fn current(&self) -> f64 {
        f64::from(self.raw_smoothed_current())
    }

    #[inline]
    fn value(&self) -> f64 {
        self.raw_target()
    }
}

/// A boolean parameter.
pub struct BoolParam {
    pub info: ParamInfo,
    value: AtomicBool,
}

impl BoolParam {
    /// # Panics
    ///
    /// Panics if `info.default_plain` isn't exactly `0.0` or `1.0`.
    /// Bool params have no halfway value; the derive emits `0.0` /
    /// `1.0` only, so this fires only when a user constructs a
    /// `BoolParam` from hand-rolled `ParamInfo`.
    #[must_use]
    pub fn new(info: ParamInfo) -> Self {
        let default = match info.default_plain {
            0.0 => false,
            1.0 => true,
            other => panic!(
                "BoolParam '{}' default {} must be exactly 0.0 (false) \
                 or 1.0 (true) - bool params have no halfway value",
                info.name, other,
            ),
        };
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
    /// silently - `default_plain = -1.0` lands on `-1` (fine), but
    /// `default_plain = 1e30` saturates to `i64::MAX` and `f64::NAN`
    /// becomes `0`. The derive populates `default_plain` from
    /// `#[param(default = ...)]`; a user-supplied float there is a
    /// programmer error, not a runtime condition we should
    /// silently absorb.
    // `truncated as f64 == default` is the integer round-trip
    // exactness check - epsilon would defeat its purpose. The
    // `as i64` truncation is the round-trip's whole point.
    #[allow(
        clippy::float_cmp,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss
    )]
    #[must_use]
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
             - supply an integer-valued default in the derive attribute",
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

    /// Read the value widened to `f32`. Useful when an int param feeds
    /// a per-sample DSP loop that runs in `f32`.
    #[allow(clippy::cast_precision_loss)]
    #[inline]
    pub fn value_f32(&self) -> f32 {
        self.value.load(Ordering::Relaxed) as f32
    }

    /// Read the value widened to `f64`.
    #[allow(clippy::cast_precision_loss)]
    #[inline]
    pub fn value_f64(&self) -> f64 {
        self.value.load(Ordering::Relaxed) as f64
    }

    /// Read the value as a non-negative `usize`. Negatives clamp to 0;
    /// values above `usize::MAX` saturate.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    #[inline]
    pub fn value_usize(&self) -> usize {
        let v = self.value.load(Ordering::Relaxed);
        if v <= 0 { 0 } else { v as usize }
    }

    /// Read the value clamped to `i32` range.
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub fn value_i32(&self) -> i32 {
        self.value
            .load(Ordering::Relaxed)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    }

    /// Read the value clamped to `u8` range (`0..=255`).
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    #[inline]
    pub fn value_u8(&self) -> u8 {
        self.value.load(Ordering::Relaxed).clamp(0, 255) as u8
    }

    pub fn set_value(&self, v: i64) {
        self.value.store(v, Ordering::Relaxed);
    }

    pub fn id(&self) -> u32 {
        self.info.id
    }
}

/// Trait for enums used as parameters.
pub trait ParamEnum: crate::__private::Sealed + Clone + Copy + Send + Sync + 'static {
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
    /// silently - a user-supplied `#[param(default = -1)]` would
    /// land on variant 0 without any signal that the default was
    /// invalid. Validate up front so the bug surfaces at plugin
    /// construction time.
    // `f64::from(idx) == default` is the integer round-trip
    // exactness check - epsilon would defeat its purpose. The
    // `as u32` truncation is the round-trip's whole point.
    #[allow(
        clippy::float_cmp,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    #[must_use]
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
            f64::from(idx) == default,
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
        // u32 → usize widens on 64-bit, narrows nowhere we ship to;
        // the lint trips because `usize` is target-dependent.
        #[allow(clippy::cast_possible_truncation)]
        let idx = self.value.load(Ordering::Relaxed) as usize;
        E::from_index(idx)
    }

    pub fn set_value(&self, v: E) {
        // Enum variant indices come from `ParamEnum::to_index`, whose
        // valid range is `0..variant_count()`; truncation past `u32::MAX`
        // would mean a > 4-billion-variant enum.
        #[allow(clippy::cast_possible_truncation)]
        let idx = v.to_index() as u32;
        self.value.store(idx, Ordering::Relaxed);
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
    /// Associated function - the dispatch is purely on `E`, no instance
    /// state is read. The `#[derive(Params)]` macro calls it as
    /// `<EnumParam<E>>::format_by_index(value)` so the field type
    /// supplies `E`.
    #[must_use]
    pub fn format_by_index(value: f64) -> String {
        // `value` is a normalized f64 in `[0, count - 1]`; the round
        // → usize cast is bounded by the variant count.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let idx = value.round() as usize;
        E::from_index(idx).name().to_string()
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
    #[doc(hidden)]
    pub id: u32,
}

impl MeterSlot {
    #[must_use]
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
    use crate::info::{ParamFlags, ParamUnit, ParamValueKind};
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
            kind: ParamValueKind::Float,
        }
    }

    #[derive(Clone, Copy)]
    enum E4 {
        A,
        B,
        C,
        D,
    }
    impl crate::__private::Sealed for E4 {}
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
