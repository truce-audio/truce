#![forbid(unsafe_code)]

mod info;
mod range;
mod smooth;
mod types;

pub use info::{ParamFlags, ParamInfo, ParamUnit};
pub use range::ParamRange;
pub use smooth::{Smoother, SmoothingStyle};
pub use types::{BoolParam, EnumParam, FloatParam, IntParam, MeterSlot, ParamEnum};

/// Implementation detail — not part of the stable public API.
/// Used by `truce-loader` to index into meter storage.
#[doc(hidden)]
pub const METER_ID_BASE: u32 = 1 << 24;

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
            // Convention: pan params are normalized to [-1.0, 1.0]. Round
            // to nearest integer percent first so the dead-zone test and
            // L/R label agree (e.g. -0.004 → 0% → "C", -0.006 → -1% → "1L").
            let pct = (value * 100.0).round() as i32;
            match pct.cmp(&0) {
                std::cmp::Ordering::Equal => "C".to_string(),
                std::cmp::Ordering::Less => format!("{}L", -pct),
                std::cmp::Ordering::Greater => format!("{}R", pct),
            }
        }
        ParamUnit::None => format!("{:.2}", value),
    }
}

/// Trait implemented by #[derive(Params)] on a struct.
/// Format wrappers use this to enumerate, read, and write parameters.
///
/// Stays dyn-compatible (every method dispatches through `&self`) so
/// editors can pass `Arc<dyn Params>` into the screenshot pipeline
/// without naming the concrete type. Generic code that needs to
/// *construct* a fresh `Params` value should add a `Default` bound
/// rather than expecting one on the trait — `#[derive(Params)]` emits
/// `impl Default` alongside the trait impl, so that bound is free for
/// derive users.
pub trait Params: Send + Sync + 'static {
    /// All parameter infos, in declaration order.
    fn param_infos(&self) -> Vec<ParamInfo>;

    /// Append parameter infos onto an existing buffer. Default impl
    /// delegates to [`Self::param_infos`] and `extend`s; the derive
    /// macro overrides for nested structs so deep trees don't pay
    /// O(depth) intermediate `Vec` allocations per outer call.
    fn append_param_infos(&self, into: &mut Vec<ParamInfo>) {
        into.extend(self.param_infos());
    }

    /// Static parameter metadata, available without an instance.
    ///
    /// Format wrappers' `register_*` paths (see
    /// [`crate::PluginExport::param_infos_static`] in `truce-core`)
    /// call this to learn the parameter set without constructing a
    /// full plugin — historically each format built a throwaway
    /// instance just to read `&self.param_infos()`, paying for any
    /// allocation the constructor did (DSP buffers, FFT plans, image
    /// atlases, etc.) at static-init time. The derive macro overrides
    /// this with a `LazyLock`-cached `Vec<ParamInfo>` built from the
    /// same compile-time metadata it uses for [`Self::param_infos`],
    /// so registration becomes allocation-free after the first call.
    ///
    /// Default impl returns an empty vec — hand-written `Params` impls
    /// that don't override fall through to the runtime path inside
    /// `PluginExport::param_infos_static`. Gated by `Self: Sized` so
    /// adding the method preserves dyn-compatibility for the existing
    /// `&self`-method shape (`&dyn Params` skips this slot).
    fn param_infos_static() -> Vec<ParamInfo>
    where
        Self: Sized,
    {
        Vec::new()
    }

    /// Number of parameters.
    fn count(&self) -> usize;

    /// IDs of every `#[meter]` slot declared on the params struct
    /// (including nested subtrees), in declaration order. Default impl
    /// returns empty — only structs that declare meters need to
    /// override. The derive macro implements it automatically.
    ///
    /// Format wrappers that expose DSP-side meters back to the UI
    /// (LV2's output control ports, for instance) use this to know
    /// which IDs to poll each `process()`.
    fn meter_ids(&self) -> Vec<u32> {
        Vec::new()
    }

    /// Get normalized value (0.0–1.0) by ID.
    fn get_normalized(&self, id: u32) -> Option<f64>;

    /// Set normalized value (0.0–1.0) by ID.
    ///
    /// Takes `&self`, not `&mut self` — the per-param storage in
    /// `FloatParam` / `BoolParam` / `IntParam` / `EnumParam` is built
    /// on `AtomicU32` / `AtomicU64`, so writes go through interior
    /// mutability. Format wrappers, GUI editors, and the audio thread
    /// all hold `&Params` (or `Arc<Params>`) concurrently and write
    /// without coordination — every implementation must be sound under
    /// concurrent `&self` writes from multiple threads.
    fn set_normalized(&self, id: u32, value: f64);

    /// Set normalized value and read back the resulting plain value in
    /// one call. CLAP / AU forward the plain value to the host's
    /// automation channel after a GUI write. The default impl is the
    /// obvious `set_normalized` then `get_plain`; concrete `Params`
    /// implementations that can compute both in one trait dispatch
    /// (e.g. the `#[derive(Params)]` output) should override for a
    /// single match-arm walk.
    fn set_normalized_returning_plain(&self, id: u32, value: f64) -> f64 {
        self.set_normalized(id, value);
        self.get_plain(id).unwrap_or(0.0)
    }

    /// Set normalized value and read back the (post-clamp / post-step)
    /// normalized value in one call. VST3 / VST2 / AAX forward
    /// normalized values to the host's automation channel. Same
    /// override-for-single-dispatch contract as
    /// [`Self::set_normalized_returning_plain`].
    fn set_normalized_returning_normalized(&self, id: u32, value: f64) -> f64 {
        self.set_normalized(id, value);
        self.get_normalized(id).unwrap_or(0.0)
    }

    /// Get plain value by ID.
    fn get_plain(&self, id: u32) -> Option<f64>;

    /// Set plain value by ID.
    ///
    /// Same `&self` interior-mutability contract as
    /// [`Self::set_normalized`].
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

    /// Walk every parameter and meter ID reachable from `self`
    /// (including nested `#[nested]` substructs) and panic on the
    /// first duplicate.
    ///
    /// Why this isn't just a compile-time check: the
    /// `#[derive(Params)]` collision check at expansion time only
    /// sees IDs declared in the *current* struct. A parent param
    /// `id = 5` and a nested-substruct param `id = 5` both compile,
    /// because the parent derive doesn't see into the nested type.
    /// At runtime, the `set_plain` / `get_plain` dispatcher matches
    /// at the outer level first and silently never reaches the
    /// nested one — preset round-trips would corrupt the nested
    /// value. This method makes that bug surface as a panic at
    /// plugin construction instead of as quiet state loss.
    ///
    /// Called automatically by the derive-generated `Self::new()`.
    /// Plugin code shouldn't need to invoke it directly.
    fn assert_no_id_collisions(&self) {
        let mut all = self.param_infos();
        // Borrow the names from the existing infos so the panic
        // message can identify *which* IDs collided.
        let mut seen: Vec<(u32, &'static str)> = Vec::with_capacity(all.len());
        for info in all.drain(..) {
            for (prev_id, prev_name) in &seen {
                if *prev_id == info.id {
                    panic!(
                        "duplicate parameter ID {}: '{}' and '{}' (likely a \
                         parent / nested-struct collision; the per-struct \
                         compile-time check can't see across nested types)",
                        info.id, prev_name, info.name,
                    );
                }
            }
            seen.push((info.id, info.name));
        }
        for meter_id in self.meter_ids() {
            for (prev_id, prev_name) in &seen {
                if *prev_id == meter_id {
                    panic!(
                        "meter ID {} collides with parameter ID for '{}'",
                        meter_id, prev_name,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range::ParamRange;

    fn pan_info() -> ParamInfo {
        ParamInfo {
            id: 0,
            name: "Pan",
            short_name: "Pan",
            group: "",
            range: ParamRange::Linear {
                min: -1.0,
                max: 1.0,
            },
            default_plain: 0.0,
            flags: ParamFlags::empty(),
            unit: ParamUnit::Pan,
        }
    }

    #[test]
    fn pan_centre() {
        let info = pan_info();
        assert_eq!(format_param_value(&info, 0.0), "C");
        assert_eq!(format_param_value(&info, 0.004), "C");
        assert_eq!(format_param_value(&info, -0.004), "C");
    }

    #[test]
    fn pan_left() {
        let info = pan_info();
        assert_eq!(format_param_value(&info, -0.5), "50L");
        assert_eq!(format_param_value(&info, -1.0), "100L");
        assert_eq!(format_param_value(&info, -0.006), "1L");
    }

    #[test]
    fn pan_right() {
        let info = pan_info();
        assert_eq!(format_param_value(&info, 0.5), "50R");
        assert_eq!(format_param_value(&info, 1.0), "100R");
        assert_eq!(format_param_value(&info, 0.006), "1R");
    }
}
