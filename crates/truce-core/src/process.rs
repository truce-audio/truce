use crate::events::{EventBody, EventList, TransportInfo};

pub struct ProcessContext<'a> {
    pub transport: &'a TransportInfo,
    pub sample_rate: f64,
    pub block_size: usize,
    pub output_events: &'a mut EventList,
    params_fn: Option<&'a dyn Fn(u32) -> f64>,
    meters_fn: Option<&'a dyn Fn(u32, f32)>,
}

impl<'a> ProcessContext<'a> {
    pub fn new(
        transport: &'a TransportInfo,
        sample_rate: f64,
        block_size: usize,
        output_events: &'a mut EventList,
    ) -> Self {
        Self {
            transport,
            sample_rate,
            block_size,
            output_events,
            params_fn: None,
            meters_fn: None,
        }
    }

    /// Set the parameter lookup callback.
    #[must_use]
    pub fn with_params(mut self, f: &'a dyn Fn(u32) -> f64) -> Self {
        self.params_fn = Some(f);
        self
    }

    /// Set the meter reporting callback.
    #[must_use]
    pub fn with_meters(mut self, f: &'a dyn Fn(u32, f32)) -> Self {
        self.meters_fn = Some(f);
        self
    }

    /// Read a parameter's plain value by ID.
    ///
    /// Returns `None` when no params callback is wired up (e.g. when a
    /// plugin runs under the bare test driver without a `with_params`
    /// closure). Callers that always run inside a real format wrapper
    /// can `.unwrap_or_default()`. The previous always-`0.0` return
    /// silently masked test-harness misconfiguration as "this param
    /// happens to be at zero", which is indistinguishable from the
    /// host actually setting it to zero.
    #[must_use] 
    pub fn param(&self, id: u32) -> Option<f64> {
        self.params_fn.map(|f| f(id))
    }

    /// Report a meter value (0.0 to 1.0).
    pub fn set_meter(&self, id: impl Into<u32>, value: f32) {
        let id = id.into();
        if let Some(f) = self.meters_fn {
            f(id, value);
        }
    }

    /// Apply `ParamChange` events to a `Params` struct, updating the
    /// smoother *targets* without snapping to them. Smoothed
    /// parameters will continue to ramp toward the new value over the
    /// configured smoothing window — the same as if the host pushed
    /// the values through the format wrapper's normal automation
    /// path. Use `params.snap_smoothers()` separately at activate /
    /// reset / sample-rate-change time, when you do want to forget
    /// in-flight smoothing state.
    pub fn sync_params<P: truce_params::Params>(&self, events: &EventList, params: &mut P) {
        for event in events.iter() {
            if let EventBody::ParamChange { id, value } = &event.body {
                params.set_normalized(*id, *value);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ProcessStatus {
    /// Plugin produced meaningful output.
    Normal,
    /// Plugin is producing tail. Value = remaining tail samples.
    Tail(u32),
    /// Keep alive even if input is silent.
    KeepAlive,
}
