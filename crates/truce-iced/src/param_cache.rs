//! Cached parameter state for iced widgets.
//!
//! `ParamCache` reads parameter values from the atomic `Params` store
//! once per tick (~60fps) and caches them as plain values that iced
//! widgets can read without atomic loads on every frame. The cache is
//! polled from `IcedProgram::update(Message::Tick)` against the
//! `PluginContext` the editor was opened with.

use std::collections::HashMap;
use std::sync::Arc;

use truce_core::editor::{PluginContext, PluginContextReadF64};
use truce_params::Params;

/// Cached parameter values for iced widget consumption.
///
/// Distinct from `PluginContext<P>`: that is the host-plugin protocol
/// surface (live atomic reads, host gestures); this is a per-tick
/// snapshot used inside `Canvas::draw` closures where iced doesn't
/// allow side effects.
pub struct ParamCache<P: Params + ?Sized> {
    params: Arc<P>,
    /// Param IDs (cached at construction so each `sync` doesn't reallocate
    /// `Vec<ParamInfo>`). The set is fixed for the lifetime of the editor -
    /// `param_infos()` returns the same list every call.
    ids: Vec<u32>,
    /// Cached normalized values, indexed by param ID.
    values: HashMap<u32, f64>,
    /// Cached formatted display strings.
    labels: HashMap<u32, String>,
    /// Meter values (0.0–1.0).
    meters: HashMap<u32, f32>,
    /// Font for canvas-drawn widget labels. Set via the editor's `with_font()`.
    font: crate::iced::Font,
}

impl<P: Params + ?Sized> ParamCache<P> {
    /// Create a new `ParamCache`, populating initial values from the params.
    pub fn new(params: Arc<P>) -> Self {
        let infos = params.param_infos();
        let ids: Vec<u32> = infos.iter().map(|i| i.id).collect();
        let mut values = HashMap::with_capacity(ids.len());
        let mut labels = HashMap::with_capacity(ids.len());
        for info in &infos {
            if let Some(v) = params.get_normalized(info.id) {
                values.insert(info.id, v);
            }
            let plain = params.get_plain(info.id).unwrap_or(0.0);
            if let Some(label) = params.format_value(info.id, plain) {
                labels.insert(info.id, label);
            }
        }
        Self {
            params,
            ids,
            values,
            labels,
            meters: HashMap::new(),
            font: crate::iced::Font::DEFAULT,
        }
    }

    /// Read a param's normalized value (0.0–1.0).
    pub fn get(&self, id: impl Into<u32>) -> f64 {
        self.values.get(&id.into()).copied().unwrap_or(0.0)
    }

    /// Read a param's plain value.
    pub fn get_plain(&self, id: impl Into<u32>) -> f64 {
        self.params.get_plain(id.into()).unwrap_or(0.0)
    }

    /// Read a param's formatted display string.
    pub fn label(&self, id: impl Into<u32>) -> &str {
        self.labels
            .get(&id.into())
            .map_or("", std::string::String::as_str)
    }

    /// Read a meter value (0.0–1.0).
    pub fn meter(&self, id: impl Into<u32>) -> f32 {
        self.meters.get(&id.into()).copied().unwrap_or(0.0)
    }

    /// The font set via the editor's `with_font()`, or `Font::DEFAULT`.
    #[must_use]
    pub fn font(&self) -> crate::iced::Font {
        self.font
    }

    /// Set the font (called by the editor runtime).
    pub fn set_font(&mut self, font: crate::iced::Font) {
        self.font = font;
    }

    /// Access the underlying params (for info lookups).
    #[must_use]
    pub fn params(&self) -> &P {
        &self.params
    }

    /// Poll all params from the editor context, return IDs that changed.
    pub(crate) fn sync<Q: ?Sized>(&mut self, ctx: &PluginContext<Q>) -> Vec<u32> {
        let mut changed = Vec::new();
        for &id in &self.ids {
            let new_val = ctx.get_param(id);
            let old_val = self.values.get(&id).copied().unwrap_or(-1.0);
            if (new_val - old_val).abs() > 1e-10 {
                self.values.insert(id, new_val);
                // Reuse the existing label slot's capacity instead of
                // dropping it on every change. `entry().or_default()`
                // returns the slot's `&mut String` (or inserts an
                // empty one); `format_param_into` clears + writes.
                // The bridge's default impl still allocates a
                // temporary internally, but bridges can override for
                // a fully alloc-free path. Either way the cache's
                // own storage no longer churns.
                let slot = self.labels.entry(id).or_default();
                ctx.format_param_into(id, slot);
                changed.push(id);
            }
        }
        changed
    }

    /// Poll meter values from the editor context. Returns whether any
    /// meter moved, so the editor's idle gate can repaint live meters
    /// without forcing a repaint when every meter is steady.
    pub(crate) fn sync_meters<Q: ?Sized>(
        &mut self,
        ctx: &PluginContext<Q>,
        meter_ids: &[u32],
    ) -> bool {
        let mut changed = false;
        for &id in meter_ids {
            let new_val = ctx.get_meter(id);
            // Bit-compare (not `==`) to sidestep clippy's float_cmp and
            // treat any distinct value - including a settle to a new
            // exact level - as a change worth repainting.
            let old_val = self.meters.insert(id, new_val);
            if old_val.map(f32::to_bits) != Some(new_val.to_bits()) {
                changed = true;
            }
        }
        changed
    }
}
