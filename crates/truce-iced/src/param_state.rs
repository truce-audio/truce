//! Cached parameter state bridge between atomic params and iced widgets.
//!
//! `ParamState` reads parameter values from the atomic `Params` store once
//! per tick (~60fps) and caches them as plain values that iced widgets can
//! read without atomic loads on every frame.

use std::collections::HashMap;
use std::sync::Arc;

use truce_core::editor::EditorContext;
use truce_params::Params;

/// Cached parameter values for iced widget consumption.
pub struct ParamState<P: Params> {
    params: Arc<P>,
    /// Cached normalized values, indexed by param ID.
    values: HashMap<u32, f64>,
    /// Cached formatted display strings.
    labels: HashMap<u32, String>,
    /// Meter values (0.0–1.0).
    meters: HashMap<u32, f32>,
    /// Font for canvas-drawn widget labels. Set via the editor's `with_font()`.
    font: iced::Font,
}

impl<P: Params> ParamState<P> {
    /// Create a new ParamState, populating initial values from the params.
    pub fn new(params: Arc<P>) -> Self {
        let mut state = Self {
            params,
            values: HashMap::new(),
            labels: HashMap::new(),
            meters: HashMap::new(),
            font: iced::Font::DEFAULT,
        };
        // Initial population
        for info in state.params.param_infos() {
            if let Some(v) = state.params.get_normalized(info.id) {
                state.values.insert(info.id, v);
            }
            let plain = state.params.get_plain(info.id).unwrap_or(0.0);
            if let Some(label) = state.params.format_value(info.id, plain) {
                state.labels.insert(info.id, label);
            }
        }
        state
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
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    /// Read a meter value (0.0–1.0).
    pub fn meter(&self, id: impl Into<u32>) -> f32 {
        self.meters.get(&id.into()).copied().unwrap_or(0.0)
    }

    /// The font set via the editor's `with_font()`, or `Font::DEFAULT`.
    pub fn font(&self) -> iced::Font {
        self.font
    }

    /// Set the font (called by the editor runtime).
    pub fn set_font(&mut self, font: iced::Font) {
        self.font = font;
    }

    /// Access the underlying params (for info lookups).
    pub fn params(&self) -> &P {
        &self.params
    }

    /// Poll all params from the editor context, return IDs that changed.
    pub(crate) fn sync(&mut self, ctx: &EditorContext) -> Vec<u32> {
        let mut changed = Vec::new();
        for info in self.params.param_infos() {
            let new_val = (ctx.get_param)(info.id);
            let old_val = self.values.get(&info.id).copied().unwrap_or(-1.0);
            if (new_val - old_val).abs() > 1e-10 {
                self.values.insert(info.id, new_val);
                self.labels.insert(info.id, (ctx.format_param)(info.id));
                changed.push(info.id);
            }
        }
        changed
    }

    /// Poll meter values from the editor context.
    pub(crate) fn sync_meters(&mut self, ctx: &EditorContext, meter_ids: &[u32]) {
        for &id in meter_ids {
            self.meters.insert(id, (ctx.get_meter)(id));
        }
    }
}
