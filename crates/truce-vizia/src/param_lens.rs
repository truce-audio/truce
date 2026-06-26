//! Bridge between truce's atomic `Params` store and vizia's
//! reactive runtime.
//!
//! Each param is backed by a single shared `Signal<f32>` carrying the
//! current normalized value. Widgets for the same `id` retrieve the
//! same Signal via [`ParamLens::value_signal`], so e.g. an XY pad
//! that writes `K::Mix` instantly updates a knob bound to the same
//! param without any extra wiring. The Signal map is lazily populated
//! on first use - widgets call `value_signal(id)` during view build
//! (inside vizia's setup context) and cache the returned handle.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use truce_core::editor::{PluginContext, PluginContextReadF32};
use truce_params::Params;
use vizia::prelude::{Signal, SignalUpdate};

/// Reactive handle to the plugin's `Params` for vizia views.
///
/// Cheap to clone (wraps a couple of `Arc`s). Hand out clones freely
/// to widgets - they all share the same underlying store *and* the
/// same `Signal` per param, so edits from one widget are visible to
/// the rest on the next frame.
pub struct ParamLens<P: Params + ?Sized> {
    ctx: PluginContext<P>,
    /// Per-param normalized-value signals (knob / slider / toggle /
    /// selector / dropdown / xy-pad). Mutating any one notifies every
    /// widget bound to the same id through vizia's reactive graph.
    signals: Arc<Mutex<HashMap<u32, Signal<f32>>>>,
    /// Per-meter display-value signals (`level_meter`). Updated from
    /// the editor's root polling timer (see `ViziaEditor::open`).
    meter_signals: Arc<Mutex<HashMap<u32, Signal<f32>>>>,
}

impl<P: Params + ?Sized> Clone for ParamLens<P> {
    fn clone(&self) -> Self {
        Self {
            ctx: self.ctx.clone(),
            signals: Arc::clone(&self.signals),
            meter_signals: Arc::clone(&self.meter_signals),
        }
    }
}

impl<P: Params + 'static> ParamLens<P> {
    pub(crate) fn new(ctx: PluginContext<P>) -> Self {
        Self {
            ctx,
            signals: Arc::new(Mutex::new(HashMap::new())),
            meter_signals: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Shared `Signal<f32>` for the param's normalized value. First
    /// access creates the Signal seeded from the current store value;
    /// subsequent accesses return the same handle, so widgets sharing
    /// an `id` see each other's writes through the vizia reactive
    /// graph. Must be called from inside a vizia setup context
    /// (i.e. while building a view tree).
    ///
    /// # Panics
    /// Panics if the internal signal-map `Mutex` was poisoned by an
    /// earlier panic on another thread. The lens is used from the
    /// single UI thread in practice, so poisoning would itself imply
    /// a prior crash and propagating it here is the right behaviour.
    #[must_use]
    pub fn value_signal(&self, id: impl Into<u32>) -> Signal<f32> {
        let id_u32: u32 = id.into();
        let mut map = self.signals.lock().expect("ParamLens signal map poisoned");
        if let Some(signal) = map.get(&id_u32) {
            return *signal;
        }
        let initial = self.ctx.get_param(id_u32);
        let signal = Signal::new(initial);
        map.insert(id_u32, signal);
        signal
    }

    /// Read the normalized `[0.0, 1.0]` value for a param.
    #[must_use]
    pub fn get(&self, id: impl Into<u32>) -> f32 {
        self.ctx.get_param(id)
    }

    /// Read the plain (unit-scaled) value.
    #[must_use]
    pub fn get_plain(&self, id: impl Into<u32>) -> f32 {
        self.ctx.get_param_plain(id)
    }

    /// Formatted display string for the current plain value.
    #[must_use]
    pub fn format(&self, id: impl Into<u32>) -> String {
        self.ctx.format_param(id)
    }

    /// Read the current peak / meter value (display-only, 0.0-1.0).
    #[must_use]
    pub fn meter(&self, id: impl Into<u32>) -> f32 {
        self.ctx.get_meter(id)
    }

    /// Shared `Signal<f32>` for a meter id. First access creates the
    /// Signal seeded from the current store value; subsequent accesses
    /// return the same handle. The editor's root polling timer (set up
    /// in `ViziaEditor::open` via [`Self::refresh_meters`]) updates
    /// every registered meter signal once per tick so vizia's reactive
    /// graph re-renders bars in real time.
    ///
    /// Must be called from inside a vizia setup context (i.e. while
    /// building a view tree).
    ///
    /// # Panics
    /// Panics if the internal signal-map `Mutex` was poisoned.
    #[must_use]
    pub fn meter_signal(&self, id: impl Into<u32>) -> Signal<f32> {
        let id_u32: u32 = id.into();
        let mut map = self
            .meter_signals
            .lock()
            .expect("ParamLens meter-signal map poisoned");
        if let Some(signal) = map.get(&id_u32) {
            return *signal;
        }
        let initial = self.ctx.get_meter(id_u32);
        let signal = Signal::new(initial);
        map.insert(id_u32, signal);
        signal
    }

    /// Push the current store meter values into every registered meter
    /// signal. Called once per timer tick by `ViziaEditor::open`.
    ///
    /// # Panics
    /// Panics if the internal signal-map `Mutex` was poisoned.
    pub fn refresh_meters(&self) {
        let map = self
            .meter_signals
            .lock()
            .expect("ParamLens meter-signal map poisoned");
        for (id, signal) in map.iter() {
            signal.set(self.ctx.get_meter(*id));
        }
    }

    /// Number of *steps* the param's range advertises (one less than
    /// the number of distinct values), or `None` for continuous
    /// ranges. Drives quantisation in `widgets::param_slider` /
    /// `param_knob`: discrete params snap on each gesture so the
    /// visual position and the audio-thread value stay aligned.
    #[must_use]
    pub fn step_count(&self, id: impl Into<u32>) -> Option<u32> {
        let id_u32: u32 = id.into();
        self.ctx
            .params()
            .param_infos()
            .into_iter()
            .find(|info| info.id == id_u32)
            .and_then(|info| info.range.step_count())
            .map(std::num::NonZeroU32::get)
    }

    /// Longest formatted value the param can ever display, in
    /// characters. For discrete params iterates every step; for
    /// continuous params samples eleven evenly-spaced normalized
    /// points (good enough to catch suffix changes like
    /// "999.0 Hz" -> "1.0 kHz" at scale boundaries).
    ///
    /// Widgets that don't want their cell width to jitter as the
    /// value changes (`widgets::param_knob`) size the value-label
    /// slot from this so the cell never grows past the floor set
    /// here.
    #[must_use]
    pub fn widest_format_chars(&self, id: impl Into<u32>) -> usize {
        let id_u32: u32 = id.into();
        let Some(info) = self
            .ctx
            .params()
            .param_infos()
            .into_iter()
            .find(|i| i.id == id_u32)
        else {
            return 0;
        };
        let sample_at = |normalized: f64| -> usize {
            let plain = info.range.denormalize(normalized.clamp(0.0, 1.0));
            self.ctx
                .params()
                .format_value(id_u32, plain)
                .map_or(0, |s| s.chars().count())
        };
        if let Some(steps) = info.range.step_count() {
            #[allow(clippy::cast_precision_loss)]
            let denom = f64::from(steps.get());
            (0..=steps.get())
                .map(|s| sample_at(f64::from(s) / denom))
                .max()
                .unwrap_or(0)
        } else {
            (0..=10)
                .map(|i| sample_at(f64::from(i) / 10.0))
                .max()
                .unwrap_or(0)
        }
    }

    /// Formatted display string for an *arbitrary* step on a discrete
    /// range, without mutating the live param. Used by
    /// `widgets::param_dropdown` to label the individual options
    /// without temporarily setting the param to each step.
    ///
    /// Dispatches through [`Params::format_value`] so `EnumParam`
    /// variant names come back as their `name`s (e.g. "Sine") rather
    /// than the underlying index. Out-of-range / continuous params
    /// return an empty string.
    #[must_use]
    pub fn step_label(&self, id: impl Into<u32>, step: usize) -> String {
        let id_u32: u32 = id.into();
        let Some(info) = self
            .ctx
            .params()
            .param_infos()
            .into_iter()
            .find(|i| i.id == id_u32)
        else {
            return String::new();
        };
        let Some(steps) = info.range.step_count() else {
            return String::new();
        };
        #[allow(clippy::cast_precision_loss)]
        let denom = f64::from(steps.get());
        if denom <= 0.0 {
            return String::new();
        }
        #[allow(clippy::cast_precision_loss)]
        let normalized = (step as f64 / denom).clamp(0.0, 1.0);
        let plain = info.range.denormalize(normalized);
        self.ctx
            .params()
            .format_value(id_u32, plain)
            .unwrap_or_default()
    }

    /// Emit a host-automation gesture: `begin_edit`, set, `end_edit`
    /// in one call. Use for one-shot edits like clicking a toggle.
    pub fn automate(&self, id: impl Into<u32>, normalized: f64) {
        self.ctx.automate(id, normalized);
    }

    /// Start a continuous drag (knob, slider, XY pad). Call once on
    /// pointer-down; pair with `set` per pointer-move and `end_edit`
    /// on pointer-up.
    pub fn begin_edit(&self, id: impl Into<u32>) {
        self.ctx.begin_edit(id);
    }

    /// Mid-drag value write. Caller must have called `begin_edit`
    /// first; otherwise host automation gates may reject the edit.
    pub fn set(&self, id: impl Into<u32>, normalized: f64) {
        self.ctx.set_param(id, normalized);
    }

    /// End a continuous drag.
    pub fn end_edit(&self, id: impl Into<u32>) {
        self.ctx.end_edit(id);
    }
}
