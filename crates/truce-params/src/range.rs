/// Defines how a parameter maps between plain and normalized values.
///
/// `Copy` because every variant is POD (scalars, or a `&'static` for
/// [`Self::Reversed`]). Lets format wrappers pass `info.range` by value
/// without `clone()` noise.
#[derive(Clone, Copy, Debug)]
pub enum ParamRange {
    Linear {
        min: f64,
        max: f64,
    },
    Logarithmic {
        min: f64,
        max: f64,
    },
    /// Power-law taper over `[min, max]`: `normalize(plain) = t^factor`
    /// where `t` is the linear proportion. `factor < 1.0` gives the low
    /// end of the range more of the knob (a log-like taper); `factor >
    /// 1.0` gives the high end more; `factor == 1.0` is `Linear`.
    Skewed {
        min: f64,
        max: f64,
        factor: f64,
    },
    /// Skew anchored at `center`, which sits at the knob's midpoint with
    /// each half a mirror of the other. The idiomatic shape for
    /// center-detented knobs - pan (`center = 0`) and EQ gain
    /// (`center = 0` dB) - where the two directions should feel
    /// symmetric regardless of where `center` falls in `[min, max]`.
    SymmetricalSkewed {
        min: f64,
        max: f64,
        factor: f64,
        center: f64,
    },
    Discrete {
        min: i64,
        max: i64,
    },
    Enum {
        count: usize,
    },
    /// Wraps another range with its normalized axis flipped: the inner
    /// range's plain `max` sits at the bottom of the knob and `min` at
    /// the top. Plain bounds and step count are the inner range's.
    Reversed(&'static ParamRange),
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
            Self::Skewed { min, max, factor } => {
                if max == min {
                    return 0.0;
                }
                let t = ((plain - min) / (max - min)).clamp(0.0, 1.0);
                t.powf(*factor)
            }
            Self::SymmetricalSkewed {
                min,
                max,
                factor,
                center,
            } => {
                if max == min {
                    return 0.0;
                }
                let unscaled = ((plain - min) / (max - min)).clamp(0.0, 1.0);
                let center_prop = ((center - min) / (max - min)).clamp(0.0, 1.0);
                // A center pinned to an edge has no symmetric half to
                // mirror; fall back to the linear proportion so the pair
                // stays round-trip stable.
                if center_prop <= 0.0 || center_prop >= 1.0 {
                    return unscaled;
                }
                if unscaled > center_prop {
                    let scaled = (unscaled - center_prop) / (1.0 - center_prop);
                    (scaled.powf(*factor) / 2.0) + 0.5
                } else {
                    let scaled = (center_prop - unscaled) / center_prop;
                    (1.0 - scaled.powf(*factor)) / 2.0
                }
            }
            Self::Reversed(inner) => 1.0 - inner.normalize(plain),
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
            Self::Skewed { min, max, factor } => {
                if max == min {
                    return *min;
                }
                min + n.powf(factor.recip()) * (max - min)
            }
            Self::SymmetricalSkewed {
                min,
                max,
                factor,
                center,
            } => {
                if max == min {
                    return *min;
                }
                let center_prop = ((center - min) / (max - min)).clamp(0.0, 1.0);
                if center_prop <= 0.0 || center_prop >= 1.0 {
                    return min + n * (max - min);
                }
                let skewed_prop = if n > 0.5 {
                    let scaled = (n - 0.5) * 2.0;
                    (scaled.powf(factor.recip()) * (1.0 - center_prop)) + center_prop
                } else {
                    let inverse = (1.0 - n * 2.0).powf(factor.recip());
                    (1.0 - inverse) * center_prop
                };
                min + skewed_prop * (max - min)
            }
            Self::Reversed(inner) => inner.denormalize(1.0 - n),
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
            Self::Linear { min, .. }
            | Self::Logarithmic { min, .. }
            | Self::Skewed { min, .. }
            | Self::SymmetricalSkewed { min, .. } => *min,
            Self::Discrete { min, .. } => *min as f64,
            Self::Enum { .. } => 0.0,
            Self::Reversed(inner) => inner.min(),
        }
    }

    /// Plain-value maximum.
    // `i64 → f64` and `usize → f64` are lossless for the bounds in
    // practice.
    #[allow(clippy::cast_precision_loss)]
    #[must_use]
    pub fn max(&self) -> f64 {
        match self {
            Self::Linear { max, .. }
            | Self::Logarithmic { max, .. }
            | Self::Skewed { max, .. }
            | Self::SymmetricalSkewed { max, .. } => *max,
            Self::Discrete { max, .. } => *max as f64,
            Self::Enum { count } => (*count as f64 - 1.0).max(0.0),
            Self::Reversed(inner) => inner.max(),
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
            Self::Linear { .. }
            | Self::Logarithmic { .. }
            | Self::Skewed { .. }
            | Self::SymmetricalSkewed { .. } => 0,
            // Reversing doesn't change how many steps the inner range
            // has, only their order.
            Self::Reversed(inner) => return inner.step_count(),
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

    /// The underlying range with any [`Self::Reversed`] wrapper peeled
    /// off. Reversing only flips the axis direction; the base range
    /// decides the parameter's *shape* (a reversed enum is still an enum).
    /// Match on this when classifying by shape - picking a widget, a taper
    /// - so a reversed enum / toggle isn't misread as a continuous knob.
    #[must_use]
    pub fn base(&self) -> &Self {
        match self {
            Self::Reversed(inner) => inner.base(),
            other => other,
        }
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

    #[test]
    fn skewed_round_trip() {
        let range = ParamRange::Skewed {
            min: 0.0,
            max: 100.0,
            factor: 0.5,
        };
        for plain in [0.0, 10.0, 50.0, 90.0, 100.0] {
            let back = range.denormalize(range.normalize(plain));
            assert!((back - plain).abs() < 1e-9, "plain={plain}, back={back}");
        }
    }

    #[test]
    fn skewed_factor_one_matches_linear() {
        let skewed = ParamRange::Skewed {
            min: -60.0,
            max: 24.0,
            factor: 1.0,
        };
        let linear = ParamRange::Linear {
            min: -60.0,
            max: 24.0,
        };
        for plain in [-60.0, -30.0, 0.0, 12.0, 24.0] {
            assert!((skewed.normalize(plain) - linear.normalize(plain)).abs() < 1e-12);
        }
    }

    #[test]
    fn skewed_low_factor_gives_low_end_more_knob() {
        // `factor < 1.0` puts more of the knob on the low end: the plain
        // value at the knob midpoint sits below the linear midpoint.
        let range = ParamRange::Skewed {
            min: 0.0,
            max: 100.0,
            factor: 0.5,
        };
        assert!(range.denormalize(0.5) < 50.0);
    }

    #[test]
    fn symmetrical_skewed_center_at_half() {
        // The center always maps to the knob midpoint, wherever it falls
        // in `[min, max]`.
        let range = ParamRange::SymmetricalSkewed {
            min: -24.0,
            max: 6.0,
            factor: 0.5,
            center: 0.0,
        };
        assert!((range.normalize(0.0) - 0.5).abs() < 1e-12);
        assert!((range.denormalize(0.5) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn symmetrical_skewed_round_trip() {
        let range = ParamRange::SymmetricalSkewed {
            min: -1.0,
            max: 1.0,
            factor: 2.0,
            center: 0.0,
        };
        for plain in [-1.0, -0.5, -0.1, 0.0, 0.1, 0.5, 1.0] {
            let back = range.denormalize(range.normalize(plain));
            assert!((back - plain).abs() < 1e-9, "plain={plain}, back={back}");
        }
    }

    #[test]
    fn symmetrical_skewed_is_symmetric_about_a_centered_center() {
        // With `center` at the arithmetic midpoint, equal plain offsets
        // map to equal knob offsets on either side of 0.5.
        let range = ParamRange::SymmetricalSkewed {
            min: -1.0,
            max: 1.0,
            factor: 0.6,
            center: 0.0,
        };
        for d in [0.25, 0.5, 0.75] {
            let above = range.normalize(d) - 0.5;
            let below = 0.5 - range.normalize(-d);
            assert!((above - below).abs() < 1e-12, "asymmetric at d={d}");
        }
    }

    #[test]
    fn reversed_flips_the_axis() {
        static INNER: ParamRange = ParamRange::Linear {
            min: 0.0,
            max: 100.0,
        };
        let range = ParamRange::Reversed(&INNER);
        assert!((range.normalize(0.0) - 1.0).abs() < 1e-12, "min -> top");
        assert!((range.normalize(100.0)).abs() < 1e-12, "max -> bottom");
        assert!((range.denormalize(0.0) - 100.0).abs() < 1e-9);
        assert!((range.denormalize(1.0)).abs() < 1e-9);
        // Plain bounds and step count come from the inner range.
        assert_eq!(range.min(), 0.0);
        assert_eq!(range.max(), 100.0);
        assert!(range.step_count().is_none());
    }

    #[test]
    fn base_peels_reversed_so_shape_survives() {
        static ENUM: ParamRange = ParamRange::Enum { count: 4 };
        static ONCE: ParamRange = ParamRange::Reversed(&ENUM);

        // A reversed enum is still an enum - `base()` unwraps to it so
        // widget / taper classification doesn't misread it as continuous.
        let reversed = ParamRange::Reversed(&ENUM);
        assert!(matches!(reversed.base(), ParamRange::Enum { count: 4 }));

        // Nested reversing peels all the way down.
        let twice = ParamRange::Reversed(&ONCE);
        assert!(matches!(twice.base(), ParamRange::Enum { count: 4 }));

        // A non-reversed range is its own base.
        let linear = ParamRange::Linear { min: 0.0, max: 1.0 };
        assert!(matches!(linear.base(), ParamRange::Linear { .. }));
    }

    #[test]
    fn reversed_round_trip_over_log() {
        static INNER: ParamRange = ParamRange::Logarithmic {
            min: 20.0,
            max: 20000.0,
        };
        let range = ParamRange::Reversed(&INNER);
        for plain in [20.0, 200.0, 2000.0, 20000.0] {
            let back = range.denormalize(range.normalize(plain));
            assert!((back - plain).abs() < 0.01, "plain={plain}, back={back}");
        }
    }

    #[test]
    fn reversed_discrete_keeps_step_count() {
        static INNER: ParamRange = ParamRange::Discrete { min: 0, max: 3 };
        let range = ParamRange::Reversed(&INNER);
        assert_eq!(range.step_count_usize(), 3);
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

    /// `normalize` must never return NaN. A host that briefly
    /// overshoots automation below `min` (or hands us a fresh
    /// uninitialized -1.0) would feed `(-1.0).ln()` (= NaN) into
    /// saved state and the editor round-trip without the clamp.
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
