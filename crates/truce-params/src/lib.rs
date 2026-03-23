#![forbid(unsafe_code)]

mod info;
mod range;
mod smooth;
mod types;

pub use info::{ParamFlags, ParamInfo, ParamUnit};
pub use range::ParamRange;
pub use smooth::{Smoother, SmoothingStyle};
pub use types::{BoolParam, EnumParam, FloatParam, IntParam, ParamEnum};

/// Format a plain parameter value as a display string based on the parameter's unit.
///
/// Used by the `#[derive(Params)]` macro for default `format_value` implementations
/// on `FloatParam` and `IntParam` fields.
pub fn format_param_value(info: &ParamInfo, value: f64) -> String {
    match info.unit {
        ParamUnit::Db => format!("{:.1} dB", value),
        ParamUnit::Hz => {
            if value >= 1000.0 {
                format!("{:.1} kHz", value / 1000.0)
            } else {
                format!("{:.0} Hz", value)
            }
        }
        ParamUnit::Milliseconds => format!("{:.1} ms", value),
        ParamUnit::Seconds => {
            if value >= 1.0 {
                format!("{:.2} s", value)
            } else {
                format!("{:.0} ms", value * 1000.0)
            }
        }
        ParamUnit::Percent => format!("{:.0}%", value * 100.0),
        ParamUnit::Semitones => format!("{:.1} st", value),
        ParamUnit::Pan => {
            if value.abs() < 0.01 {
                "C".to_string()
            } else if value < 0.0 {
                format!("{:.0}L", -value * 100.0)
            } else {
                format!("{:.0}R", value * 100.0)
            }
        }
        ParamUnit::None => format!("{:.2}", value),
    }
}

/// Trait implemented by #[derive(Params)] on a struct.
/// Format wrappers use this to enumerate, read, and write parameters.
pub trait Params: Send + Sync + 'static {
    /// All parameter infos, in declaration order.
    fn param_infos(&self) -> Vec<ParamInfo>;

    /// Number of parameters.
    fn count(&self) -> usize;

    /// Get normalized value (0.0–1.0) by ID.
    fn get_normalized(&self, id: u32) -> Option<f64>;

    /// Set normalized value (0.0–1.0) by ID.
    fn set_normalized(&self, id: u32, value: f64);

    /// Get plain value by ID.
    fn get_plain(&self, id: u32) -> Option<f64>;

    /// Set plain value by ID.
    fn set_plain(&self, id: u32, value: f64);

    /// Format a plain value to display string.
    fn format_value(&self, id: u32, value: f64) -> Option<String>;

    /// Parse a display string to plain value.
    fn parse_value(&self, id: u32, text: &str) -> Option<f64>;

    /// Reset all smoothers to current values.
    fn snap_smoothers(&self);

    /// Update smoother sample rates.
    fn set_sample_rate(&self, sample_rate: f64);

    /// Collect all parameter IDs and their current plain values.
    fn collect_values(&self) -> (Vec<u32>, Vec<f64>);

    /// Restore parameter values from a list of (id, value) pairs.
    fn restore_values(&self, values: &[(u32, f64)]);

    /// Create a default instance for GUI parameter display.
    /// The GUI reads values via atomic reads from this instance.
    fn default_for_gui() -> Self
    where
        Self: Sized;
}
