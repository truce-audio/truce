/// Defines how a parameter maps between plain and normalized values.
///
/// `Copy` because every variant is POD (two scalar fields). Lets format
/// wrappers pass `info.range` by value without `clone()` noise.
#[derive(Clone, Copy, Debug)]
pub enum ParamRange {
    Linear { min: f64, max: f64 },
    Logarithmic { min: f64, max: f64 },
    Discrete { min: i64, max: i64 },
    Enum { count: usize },
}

impl ParamRange {
    /// Map a plain value to 0.0–1.0.
    ///
    /// Degenerate bounds - `min == max` for `Linear` / `Discrete`,
    /// non-positive or empty for `Logarithmic`, `count <= 1` for
    /// `Enum` - collapse to `0.0`. Combined with [`Self::denormalize`]
    /// returning `min` on the same inputs, the pair is round-trip
    /// stable: the result always converges to the bottom of the
    /// (degenerate) range rather than producing NaN or wrapping into
    /// nonsense.
    // `min == max` detects mathematically zero-width ranges; an epsilon
    // would mis-route a user-defined `Linear { 1.0, 1.0 + EPSILON }`.
    // `i64 → f64` casts on `Discrete` bounds are lossless in practice
    // (no sane param has > 2^52 steps).
    #[allow(clippy::float_cmp, clippy::cast_precision_loss)]
    #[must_use]
    pub fn normalize(&self, plain: f64) -> f64 {
        match self {
            Self::Linear { min, max } => {
                if max == min {
                    return 0.0;
                }
                ((plain - min) / (max - min)).clamp(0.0, 1.0)
            }
            Self::Logarithmic { min, max } => {
                if *min <= 0.0 || *max <= 0.0 || min == max {
                    return 0.0;
                }
                // `plain.ln()` returns NaN for `plain <= 0`; the
                // post-clamp leaves the NaN intact and a host that
                // briefly overshoots automation below `min` ends up
                // with a NaN normalized value flowing into saved
                // state and the GUI round-trip.
                if plain <= *min {
                    return 0.0;
                }
                if plain >= *max {
                    return 1.0;
                }
                let min_log = min.ln();
                let max_log = max.ln();
                ((plain.ln() - min_log) / (max_log - min_log)).clamp(0.0, 1.0)
            }
            Self::Discrete { min, max } => {
                if max == min {
                    return 0.0;
                }
                ((plain - *min as f64) / (*max as f64 - *min as f64)).clamp(0.0, 1.0)
            }
            Self::Enum { count } => {
                if *count <= 1 {
                    return 0.0;
                }
                (plain / (*count as f64 - 1.0)).clamp(0.0, 1.0)
            }
        }
    }

    /// Map 0.0–1.0 back to a plain value.
    ///
    /// Degenerate bounds collapse to `min` (or `0.0` for `Enum` with
    /// `count <= 1`). See [`Self::normalize`] for the round-trip
    /// semantics.
    // `min == max` detects mathematically zero-width ranges; matches
    // `normalize`'s asymmetric handling so the pair stays stable.
    // `i64 → f64` and `usize → f64` casts on `Discrete` / `Enum`
    // bounds are lossless in practice (no sane param has > 2^52 steps).
    #[allow(clippy::float_cmp, clippy::cast_precision_loss)]
    #[must_use]
    pub fn denormalize(&self, normalized: f64) -> f64 {
        let n = normalized.clamp(0.0, 1.0);
        match self {
            Self::Linear { min, max } => min + n * (max - min),
            Self::Logarithmic { min, max } => {
                // Match `normalize`'s asymmetric handling of bad bounds:
                // if either end is non-positive or the range is empty,
                // both directions collapse to `min` (round-trip stable).
                if *min <= 0.0 || *max <= 0.0 || min == max {
                    return *min;
                }
                let min_log = min.ln();
                let max_log = max.ln();
                (min_log + n * (max_log - min_log)).exp()
            }
            Self::Discrete { min, max } => {
                ((*min as f64) + n * (*max as f64 - *min as f64)).round()
            }
            Self::Enum { count } => {
                if *count <= 1 {
                    return 0.0;
                }
                (n * (*count as f64 - 1.0)).round()
            }
        }
    }

    /// Plain-value minimum.
    // `i64 → f64` is lossless for the bounds in practice (no sane
    // param has > 2^52 steps).
    #[allow(clippy::cast_precision_loss)]
    #[must_use]
    pub fn min(&self) -> f64 {
        match self {
            Self::Linear { min, .. } | Self::Logarithmic { min, .. } => *min,
            Self::Discrete { min, .. } => *min as f64,
            Self::Enum { .. } => 0.0,
        }
    }

    /// Plain-value maximum.
    // `i64 → f64` and `usize → f64` are lossless for the bounds in
    // practice.
    #[allow(clippy::cast_precision_loss)]
    #[must_use]
    pub fn max(&self) -> f64 {
        match self {
            Self::Linear { max, .. } | Self::Logarithmic { max, .. } => *max,
            Self::Discrete { max, .. } => *max as f64,
            Self::Enum { count } => (*count as f64 - 1.0).max(0.0),
        }
    }

    /// Number of discrete steps for a quantized range.
    ///
    /// `None` means continuous (Linear / Logarithmic). `Some(n)` means
    /// the range covers `n + 1` distinct values (a step count of 3 →
    /// 4 picker positions). Cross-format wrappers that serialize a
    /// `0 = continuous` sentinel into a C struct should call
    /// `.map(NonZeroU32::get).unwrap_or(0)` at the FFI boundary.
    ///
    /// Discrete / Enum variants with degenerate bounds (`min > max`,
    /// or `count <= 1`) return `None` - semantically continuous,
    /// because there's nothing to step through.
    #[must_use]
    pub fn step_count(&self) -> Option<std::num::NonZeroU32> {
        let raw: u32 = match self {
            Self::Linear { .. } | Self::Logarithmic { .. } => 0,
            // `max - min` as `i64` is fine, but `as u32` wraps for
            // `min > max` or steps > u32::MAX. Saturate instead so a
            // mis-specified `Discrete` range can't produce a bogus
            // step count that callers might index with.
            Self::Discrete { min, max } => {
                // Result is `min`-clamped to `0..=u32::MAX`.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let n = (max.saturating_sub(*min)).max(0).min(i64::from(u32::MAX)) as u32;
                n
            }
            // Enum variant counts are well below `u32::MAX` in practice
            // (typical < 100); the saturating_sub keeps `count = 0` honest.
            #[allow(clippy::cast_possible_truncation)]
            Self::Enum { count } => (*count as u32).saturating_sub(1),
        };
        std::num::NonZeroU32::new(raw)
    }

    /// `step_count` widened to `usize` with the continuous case
    /// flattened to `1`. Convenience for UI code that loops over
    /// discrete values and falls back to a single step for continuous
    /// ranges.
    #[must_use]
    pub fn step_count_usize(&self) -> usize {
        self.step_count().map_or(1, |n| n.get() as usize)
    }
}

#[cfg(test)]
mod tests {
    // Round-trip and degenerate-bounds tests assert exact float
    // results (0.0, midpoints, fixed points) - equality is the
    // contract being verified. Cast truncations in this module are
    // bounded by the literal `count: 4` test fixtures.
    #![allow(
        clippy::float_cmp,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]

    use super::*;

    #[test]
    fn linear_round_trip() {
        let range = ParamRange::Linear {
            min: -60.0,
            max: 24.0,
        };
        for plain in [-60.0, -30.0, 0.0, 12.0, 24.0] {
            let norm = range.normalize(plain);
            let back = range.denormalize(norm);
            assert!(
                (back - plain).abs() < 1e-10,
                "plain={plain}, norm={norm}, back={back}"
            );
        }
    }

    #[test]
    fn log_round_trip() {
        let range = ParamRange::Logarithmic {
            min: 20.0,
            max: 20000.0,
        };
        for plain in [20.0, 100.0, 1000.0, 10000.0, 20000.0] {
            let norm = range.normalize(plain);
            let back = range.denormalize(norm);
            assert!(
                (back - plain).abs() < 0.01,
                "plain={plain}, norm={norm}, back={back}"
            );
        }
    }

    #[test]
    fn enum_round_trip() {
        let range = ParamRange::Enum { count: 4 };
        for idx in 0..4 {
            let norm = range.normalize(idx as f64);
            let back = range.denormalize(norm);
            assert_eq!(back as usize, idx);
        }
    }

    /// Degenerate bounds (empty/non-positive/single-step) collapse the
    /// round trip to a fixed point at `min` rather than producing NaN
    /// or wrapping. Locks in `normalize → 0.0`, `denormalize(0.0) →
    /// min`, and `normalize(min) → 0.0` for every range variant so a
    /// future maintainer simplifying one branch can't accidentally
    /// reintroduce divergent behavior.
    #[test]
    fn degenerate_bounds_round_trip_stable() {
        let cases = [
            ParamRange::Linear { min: 5.0, max: 5.0 },
            ParamRange::Logarithmic {
                min: 100.0,
                max: 100.0,
            },
            ParamRange::Logarithmic {
                min: -1.0,
                max: 10.0,
            },
            ParamRange::Logarithmic { min: 1.0, max: 0.0 },
            ParamRange::Discrete { min: 7, max: 7 },
            ParamRange::Enum { count: 0 },
            ParamRange::Enum { count: 1 },
        ];
        for range in cases {
            let bottom = range.min();
            assert_eq!(range.normalize(bottom), 0.0, "normalize(min) for {range:?}");
            assert_eq!(
                range.normalize(42.0),
                0.0,
                "normalize(arbitrary) for {range:?}"
            );
            assert_eq!(
                range.denormalize(0.0),
                bottom,
                "denormalize(0.0) for {range:?}"
            );
            assert_eq!(
                range.denormalize(0.5),
                bottom,
                "denormalize(mid) for {range:?}"
            );
            // Double round trip lands at the same fixed point.
            let once = range.denormalize(range.normalize(42.0));
            let twice = range.denormalize(range.normalize(once));
            assert_eq!(once, twice, "round-trip not stable for {range:?}");
        }
    }

    /// `normalize` must never return NaN - a host that briefly
    /// overshoots automation below `min` (or hands us a fresh
    /// uninitialized -1.0) used to flow `(-1.0).ln()` (= NaN) into
    /// saved state and the editor round-trip.
    #[test]
    fn logarithmic_normalize_never_nan() {
        let range = ParamRange::Logarithmic {
            min: 20.0,
            max: 20000.0,
        };
        for plain in [-1.0, 0.0, 0.5, 19.99, f64::NEG_INFINITY] {
            let n = range.normalize(plain);
            assert!(!n.is_nan(), "NaN from normalize({plain})");
            assert_eq!(n, 0.0, "normalize({plain}) should clamp to 0.0");
        }
        for plain in [20000.0, 20001.0, 1e9, f64::INFINITY] {
            let n = range.normalize(plain);
            assert!(!n.is_nan(), "NaN from normalize({plain})");
            assert_eq!(n, 1.0, "normalize({plain}) should clamp to 1.0");
        }
    }
}
