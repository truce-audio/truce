#![forbid(unsafe_code)]

mod info;
mod range;
pub mod sample;
mod smooth;
mod types;

pub use info::{MidiSource, ParamFlags, ParamInfo, ParamUnit, ParamValueKind, map_source_to_param};
pub use range::ParamRange;
pub use sample::{Float, Sample};
pub use smooth::{Smoother, SmoothingStyle};
pub use types::{
    BoolParam, EnumParam, FloatParam, FloatParamReadF32, FloatParamReadF64, IntParam, MeterSlot,
    ParamEnum,
};

/// Implementation detail - not part of the stable public API.
/// Used by `truce-loader` to index into meter storage.
#[doc(hidden)]
pub const METER_ID_BASE: u32 = 1 << 24;

/// Sealing module: external crates cannot implement [`Params`] or
/// [`ParamEnum`] directly because they can't name `Sealed`. The
/// `#[derive(Params)]` and `#[derive(ParamEnum)]` macros emit the
/// `Sealed` impl alongside their trait impls, so derive users are
/// unaffected.
#[doc(hidden)]
pub mod __private {
    pub trait Sealed {}
}

/// Format a plain parameter value as a display string based on the parameter's unit.
///
/// Used by the `#[derive(Params)]` macro for default `format_value` implementations
/// on `FloatParam` and `IntParam` fields. `IntParam` is identified by
/// `ParamValueKind::Int`, set by the derive from the field type - its
/// value is always integer-valued, so the fractional `{:.1}` / `{:.2}`
/// formats float-typed params use would render "0.0 st" / "0.00"
/// instead of "0 st" / "0".
#[must_use]
pub fn format_param_value(info: &ParamInfo, value: f64) -> String {
    let is_int = info.kind == ParamValueKind::Int;
    // Round to nearest integer before display so a smoothed IntParam
    // that's mid-transition doesn't briefly render the rounded-down
    // half-step (e.g. an `i32::from(value)` of -1 when value is -0.5
    // mid-snap). `IntParam::value_i32` rounds the same way at the
    // audio-thread read site.
    #[allow(clippy::cast_possible_truncation)]
    let int_value = value.round() as i64;
    match info.unit {
        ParamUnit::Db => {
            if is_int {
                format!("{int_value} dB")
            } else {
                format!("{value:.1} dB")
            }
        }
        ParamUnit::Hz => {
            if value >= 1000.0 {
                format!("{:.1} kHz", value / 1000.0)
            } else {
                format!("{value:.0} Hz")
            }
        }
        ParamUnit::Milliseconds => {
            if is_int {
                format!("{int_value} ms")
            } else {
                format!("{value:.1} ms")
            }
        }
        ParamUnit::Seconds => {
            if value >= 1.0 {
                format!("{value:.2} s")
            } else {
                format!("{:.0} ms", value * 1000.0)
            }
        }
        ParamUnit::Percent => format!("{:.0}%", value * 100.0),
        ParamUnit::Semitones => {
            if is_int {
                format!("{int_value} st")
            } else {
                format!("{value:.1} st")
            }
        }
        ParamUnit::Degrees => {
            if is_int {
                format!("{int_value}°")
            } else {
                format!("{value:.1}°")
            }
        }
        ParamUnit::Pan => {
            // Convention: pan params are normalized to [-1.0, 1.0]. Round
            // to nearest integer percent first so the dead-zone test and
            // L/R label agree (e.g. -0.004 → 0% → "C", -0.006 → -1% → "1L").
            // Result is bounded by `[-100, 100]` after clamp to `[-1, 1]`.
            #[allow(clippy::cast_possible_truncation)]
            let pct = (value * 100.0).round() as i32;
            match pct.cmp(&0) {
                std::cmp::Ordering::Equal => "C".to_string(),
                std::cmp::Ordering::Less => format!("{}L", -pct),
                std::cmp::Ordering::Greater => format!("{pct}R"),
            }
        }
        ParamUnit::None => {
            if is_int {
                format!("{int_value}")
            } else {
                format!("{value:.2}")
            }
        }
    }
}

/// Trait implemented by #[derive(Params)] on a struct.
/// Format wrappers use this to enumerate, read, and write parameters.
///
/// Stays dyn-compatible (every method dispatches through `&self`) so
/// editors can pass `Arc<dyn Params>` into the screenshot pipeline
/// without naming the concrete type. Generic code that needs to
/// *construct* a fresh `Params` value should add a `Default` bound
/// rather than expecting one on the trait - `#[derive(Params)]` emits
/// `impl Default` alongside the trait impl, so that bound is free for
/// derive users.
pub trait Params: __private::Sealed + Send + Sync + 'static {
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
    /// Format wrappers' `register_*` paths call this to learn the
    /// parameter set without constructing a full plugin. The
    /// instance-based alternative would pay for any allocation the
    /// constructor does (DSP buffers, FFT plans, image atlases, etc.)
    /// at static-init time, which is fragile under AAX's `Describe`
    /// running before main. The derive macro overrides this with a
    /// `LazyLock`-cached `Vec<ParamInfo>` built from the same
    /// compile-time metadata it uses for [`Self::param_infos`], so
    /// registration becomes allocation-free after the first call.
    ///
    /// Default impl returns an empty vec - hand-written `Params` impls
    /// that don't override fall through to the runtime path inside
    /// `PluginExport::param_infos_static`. Gated by `Self: Sized` so
    /// adding the method preserves dyn-compatibility for the existing
    /// `&self`-method shape (`&dyn Params` skips this slot).
    #[must_use]
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
    /// returns empty - only structs that declare meters need to
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
    /// Takes `&self`, not `&mut self` - the per-param storage in
    /// `FloatParam` / `BoolParam` / `IntParam` / `EnumParam` is built
    /// on `AtomicU32` / `AtomicU64`, so writes go through interior
    /// mutability. Format wrappers, GUI editors, and the audio thread
    /// all hold `&Params` (or `Arc<Params>`) concurrently and write
    /// without coordination - every implementation must be sound under
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

    /// Serialize this store's `#[persist]` fields into a keyed blob the
    /// host saves alongside the parameter values. Default: empty (no
    /// persist fields). The `#[derive(Params)]` macro overrides it when
    /// any field carries `#[persist]`.
    #[must_use]
    fn serialize_persist(&self) -> Vec<u8> {
        Vec::new()
    }

    /// Restore this store's `#[persist]` fields from a blob produced by
    /// [`Self::serialize_persist`]. Unknown / missing keys are skipped,
    /// leaving those fields at their current value. Default: no-op.
    fn load_persist(&self, data: &[u8]) {
        let _ = data;
    }

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
    /// nested one - preset round-trips would corrupt the nested
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
                assert!(
                    *prev_id != info.id,
                    "duplicate parameter ID {}: '{}' and '{}' (likely a \
                     parent / nested-struct collision; the per-struct \
                     compile-time check can't see across nested types)",
                    info.id,
                    prev_name,
                    info.name,
                );
            }
            seen.push((info.id, info.name));
        }
        let mut seen_meters: Vec<u32> = Vec::new();
        for meter_id in self.meter_ids() {
            for (prev_id, prev_name) in &seen {
                assert!(
                    *prev_id != meter_id,
                    "meter ID {meter_id} collides with parameter ID for '{prev_name}'",
                );
            }
            // Meter IDs auto-assign per struct from a shared base, so two
            // `#[nested]` structs that each declare a meter hand back the
            // same ID and would alias in meter storage. The per-struct
            // compile-time check can't see across nested types; surface it
            // as a construction panic instead of silent aliasing.
            assert!(
                !seen_meters.contains(&meter_id),
                "duplicate meter ID {meter_id} (two #[nested] structs each \
                 declare a meter; nested meters aren't supported - keep \
                 meters in a single Params struct)",
            );
            seen_meters.push(meter_id);
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
            kind: ParamValueKind::Float,
            midi_map: None,
            midi_channel: None,
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

    fn int_info(unit: ParamUnit) -> ParamInfo {
        ParamInfo {
            id: 0,
            name: "n",
            short_name: "n",
            group: "",
            range: ParamRange::Discrete { min: -12, max: 12 },
            default_plain: 0.0,
            flags: ParamFlags::empty(),
            unit,
            kind: ParamValueKind::Int,
            midi_map: None,
            midi_channel: None,
        }
    }

    #[test]
    fn int_param_no_fractional_zero() {
        // IntParam values must render with no decimal places.
        // A hard-coded `{:.1}` formatter (regardless of param kind)
        // would render "0.0 st" / "-5.0 st" for semitone values.
        assert_eq!(
            format_param_value(&int_info(ParamUnit::Semitones), 0.0),
            "0 st"
        );
        assert_eq!(
            format_param_value(&int_info(ParamUnit::Semitones), -5.0),
            "-5 st"
        );
        assert_eq!(format_param_value(&int_info(ParamUnit::None), 0.0), "0");
        assert_eq!(format_param_value(&int_info(ParamUnit::Db), 6.0), "6 dB");
        assert_eq!(
            format_param_value(&int_info(ParamUnit::Milliseconds), 50.0),
            "50 ms"
        );
    }
}
