//! Read-only view of parameter state consumed by the pure-library
//! rendering and interaction functions (`widgets::draw`,
//! `interaction::dispatch`).
//!
//! Callers build a `ParamSnapshot` per frame from their own parameter
//! store — typically `BuiltinEditor` forwards `EditorContext` + `Params`
//! accesses, but any plugin that owns its own frame can populate it
//! from whatever source it likes.

use crate::widgets::WidgetType;

/// Immutable view of one frame's worth of parameter state.
///
/// Each field is a borrowed closure, so the snapshot itself is cheap
/// to construct — no copying of values up front. Closures are read
/// on demand as widgets/dispatch need them.
pub struct ParamSnapshot<'a> {
    /// Read normalized value (0.0–1.0).
    pub get_param: &'a dyn Fn(u32) -> f32,

    /// Read plain (unit-scaled) value.
    pub get_param_plain: &'a dyn Fn(u32) -> f32,

    /// Format the current plain value as a display string.
    pub format_param: &'a dyn Fn(u32) -> String,

    /// Read a meter value (0.0–1.0).
    pub get_meter: &'a dyn Fn(u32) -> f32,

    /// Enumerate option display strings for a discrete/enum parameter.
    /// Returns empty if the parameter is continuous or not dropdown-shaped.
    pub get_options: &'a dyn Fn(u32) -> Vec<String>,

    /// Default normalized value, used for reset-on-double-click.
    pub default_normalized: &'a dyn Fn(u32) -> f32,

    /// Compute the next normalized value after a click-to-cycle selector
    /// advance. Wraps around at the end of the range.
    pub next_discrete_normalized: &'a dyn Fn(u32) -> f32,

    /// Parameter display name. Used as a fallback label when a layout
    /// entry did not supply its own (e.g. XY-pad axis names).
    pub param_name: &'a dyn Fn(u32) -> String,

    /// Auto-detected widget type from the parameter's range. `widgets::draw`
    /// uses this when the layout entry did not specify an explicit widget
    /// kind.
    pub widget_type: &'a dyn Fn(u32) -> WidgetType,
}
