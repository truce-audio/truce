/// Defines how a parameter maps between plain and normalized values.
#[derive(Clone, Debug)]
pub enum ParamRange {
    Linear { min: f64, max: f64 },
    Logarithmic { min: f64, max: f64 },
    Discrete { min: i64, max: i64 },
    Enum { count: usize },
}

impl ParamRange {
    /// Map a plain value to 0.0–1.0.
    pub fn normalize(&self, plain: f64) -> f64 {
        match self {
            Self::Linear { min, max } => ((plain - min) / (max - min)).clamp(0.0, 1.0),
            Self::Logarithmic { min, max } => {
                if *min <= 0.0 || *max <= 0.0 {
                    return 0.0;
                }
                let min_log = min.ln();
                let max_log = max.ln();
                ((plain.ln() - min_log) / (max_log - min_log)).clamp(0.0, 1.0)
            }
            Self::Discrete { min, max } => {
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
    pub fn denormalize(&self, normalized: f64) -> f64 {
        let n = normalized.clamp(0.0, 1.0);
        match self {
            Self::Linear { min, max } => min + n * (max - min),
            Self::Logarithmic { min, max } => {
                if *min <= 0.0 || *max <= 0.0 {
                    return *min;
                }
                let min_log = min.ln();
                let max_log = max.ln();
                (min_log + n * (max_log - min_log)).exp()
            }
            Self::Discrete { min, max } => {
                ((*min as f64) + n * (*max as f64 - *min as f64)).round()
            }
            Self::Enum { count } => (n * (*count as f64 - 1.0)).round(),
        }
    }

    /// Plain-value minimum.
    pub fn min(&self) -> f64 {
        match self {
            Self::Linear { min, .. } | Self::Logarithmic { min, .. } => *min,
            Self::Discrete { min, .. } => *min as f64,
            Self::Enum { .. } => 0.0,
        }
    }

    /// Plain-value maximum.
    pub fn max(&self) -> f64 {
        match self {
            Self::Linear { max, .. } | Self::Logarithmic { max, .. } => *max,
            Self::Discrete { max, .. } => *max as f64,
            Self::Enum { count } => (*count as f64 - 1.0).max(0.0),
        }
    }

    /// Number of discrete steps (0 = continuous).
    pub fn step_count(&self) -> u32 {
        match self {
            Self::Linear { .. } | Self::Logarithmic { .. } => 0,
            Self::Discrete { min, max } => (*max - *min) as u32,
            Self::Enum { count } => (*count as u32).saturating_sub(1),
        }
    }
}

#[cfg(test)]
mod tests {
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
}
