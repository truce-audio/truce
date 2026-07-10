use crate::config::ProcessMode;
use crate::events::{EventList, TransportInfo};
use crate::tasks::{AnyTaskSpawner, TaskSpawner};

/// Per-block context handed to `process()`. Construct via
/// [`Self::new`] + the `with_*` builders. Marked `#[non_exhaustive]`
/// so adding host-populated fields in future (e.g. `host_latency`,
/// `bus_routing`) isn't a `SemVer` break for downstream pre-1.0 callers.
#[non_exhaustive]
pub struct ProcessContext<'a> {
    pub transport: &'a TransportInfo,
    /// How the host is driving audio this block. Tracks host toggles
    /// that don't force a re-prepare (VST3 `kRealtime` <-> `kPrefetch`,
    /// an LV2 freewheel port). A plugin that reallocates for offline
    /// keys off `AudioConfig::process_mode` at `reset` instead; this
    /// field is for "may I relax realtime discipline right now?".
    pub process_mode: ProcessMode,
    pub sample_rate: f64,
    pub block_size: usize,
    pub output_events: &'a mut EventList,
    params_fn: Option<&'a dyn Fn(u32) -> f64>,
    meters_fn: Option<&'a dyn Fn(u32, f32)>,
    /// Type-erased handle to this instance's background-task spawner,
    /// installed by the shell when the plugin wired `tasks:` on
    /// `plugin!`. Recover a typed spawner with [`Self::tasks`].
    tasks: Option<&'a AnyTaskSpawner>,
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
            process_mode: ProcessMode::Realtime,
            sample_rate,
            block_size,
            output_events,
            params_fn: None,
            meters_fn: None,
            tasks: None,
        }
    }

    /// Set the processing mode for this block. Defaults to
    /// [`ProcessMode::Realtime`]; wrappers stamp the live host mode.
    #[must_use]
    pub fn with_process_mode(mut self, mode: ProcessMode) -> Self {
        self.process_mode = mode;
        self
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

    /// Install the background-task spawner for this block. The shell
    /// stamps it when the plugin wired `tasks:` on `plugin!`.
    #[must_use]
    pub fn with_tasks(mut self, tasks: &'a AnyTaskSpawner) -> Self {
        self.tasks = Some(tasks);
        self
    }

    /// The background-task spawner for task type `T`, or `None` if the
    /// plugin declared no `tasks:` lane of that type. Scheduling with the
    /// returned spawner is wait-free (see
    /// [`TaskSpawner::try_spawn`] / [`TaskSpawner::spawn_coalescing`]), so
    /// it is safe to call from the audio thread.
    #[must_use]
    pub fn tasks<T: Send + 'static>(&self) -> Option<TaskSpawner<T>> {
        self.tasks.and_then(AnyTaskSpawner::downcast::<T>)
    }

    /// Read a parameter's plain value by ID.
    ///
    /// Returns `None` when no params callback is wired up (e.g. when a
    /// plugin runs under the bare test driver without a `with_params`
    /// closure). Callers that always run inside a real format wrapper
    /// can `.unwrap_or_default()`. Distinguishing "no callback" from
    /// "value is zero" lets test harnesses notice when they forgot to
    /// wire up params rather than masking the misconfiguration as
    /// "host set the value to zero".
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
