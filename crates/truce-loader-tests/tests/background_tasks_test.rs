//! Integration test: a task type's `BackgroundTask::run` runs on the
//! shared pool when scheduled from `process` through the shell-wired
//! `TaskSpawner`. Exercises the full static-shell path: `from_parts`
//! stores the spawner, `process` stamps it into the `ProcessContext`,
//! `ctx.tasks::<T>()` recovers it, and the pool runs the handler
//! off-thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use truce::prelude::BackgroundTask;
use truce_core::AudioConfig;
use truce_core::buffer::AudioBuffer;
use truce_core::events::{EventList, TransportInfo};
use truce_core::plugin::PluginRuntime;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_core::tasks::{AnyTaskSpawner, TaskSpawner, TaskSpawnerBundle};
use truce_derive::Params;
use truce_gui::PluginLogic;

#[derive(Params)]
struct TaskParams {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)")]
    gain: truce_params::FloatParam,
    // Not a parameter: the worker bumps this so the test can observe
    // that the handler ran. Reached through the shared `Arc<Params>`.
    #[skip]
    ran: Arc<AtomicU32>,
}

struct TaskPlugin;

#[derive(Clone, Copy, Debug)]
struct Ping;

impl PluginLogic for TaskPlugin {
    type Params = TaskParams;
    type DspState = ();

    fn process(
        _state: &mut (),
        _params: &TaskParams,
        _buffer: &mut AudioBuffer,
        _events: &EventList,
        ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        if let Some(tasks) = ctx.tasks::<Ping>() {
            let _ = tasks.try_spawn(Ping);
        }
        ProcessStatus::Normal
    }

    fn editor(_params: Arc<TaskParams>) -> Box<dyn truce::prelude::Editor> {
        Box::new(NoEditor)
    }
}

impl BackgroundTask for Ping {
    type Params = TaskParams;

    fn run(self, params: &TaskParams) {
        params.ran.fetch_add(1, Ordering::Relaxed);
    }
}

struct NoEditor;
impl truce::prelude::Editor for NoEditor {
    fn size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn open(&mut self, _: truce_core::editor::RawWindowHandle, _: truce::prelude::PluginContext) {}
    fn close(&mut self) {}
    fn idle(&mut self) {}
}

#[test]
fn background_task_runs_when_scheduled_from_process() {
    let params = Arc::new(TaskParams::new());
    let ran = Arc::clone(&params.ran);

    // Build the spawner exactly as the `tasks:` key on `plugin!` does.
    let spawner = {
        let params = Arc::clone(&params);
        TaskSpawner::<Ping>::new(move |task| task.run(&params))
    };
    let tasks = AnyTaskSpawner::new(&spawner);

    let mut shell = truce_loader::static_shell::StaticShell::<TaskParams, TaskPlugin>::from_parts(
        params,
        Some(tasks),
    );
    shell.reset(&AudioConfig::new(44100.0, 64));

    // One block: `process` schedules a Ping through `ctx.tasks::<Ping>()`.
    let input = vec![0.0f32; 64];
    let mut output = vec![0.0f32; 64];
    let inputs: Vec<&[f32]> = vec![&input];
    let mut outputs: Vec<&mut [f32]> = vec![&mut output];
    let mut buffer = unsafe { AudioBuffer::from_slices(&inputs, &mut outputs, 64) };
    let transport = TransportInfo::default();
    let mut output_events = EventList::default();
    let mut ctx = ProcessContext::new(&transport, 44100.0, 64, &mut output_events);
    shell.process(&mut buffer, &EventList::default(), &mut ctx);

    // The pool runs the handler asynchronously; wait for it.
    let start = Instant::now();
    while ran.load(Ordering::Relaxed) == 0 && start.elapsed() < Duration::from_secs(2) {
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        ran.load(Ordering::Relaxed),
        1,
        "the handler ran once, off the audio thread"
    );
}

// A second, serialized lane in the same plugin. Adds 100 so the test can
// tell the two lanes' handlers apart in one counter.
#[derive(Debug)]
struct Rebuild;
impl BackgroundTask for Rebuild {
    type Params = TaskParams;
    const SERIALIZED: bool = true;
    fn run(self, params: &TaskParams) {
        params.ran.fetch_add(100, Ordering::Relaxed);
    }
}

// A task type the plugin never declares - `downcast` must not resolve it.
struct Absent;
impl BackgroundTask for Absent {
    type Params = TaskParams;
    fn run(self, _params: &TaskParams) {}
}

#[test]
fn multiple_lanes_resolve_by_type_and_run_independently() {
    let params = Arc::new(TaskParams::new());
    let ran = Arc::clone(&params.ran);

    // Two lanes with different modes, bundled as `plugin! { tasks: [..] }`
    // does: `Ping` concurrent, `Rebuild` serialized.
    let mut bundle = TaskSpawnerBundle::new();
    {
        let params = Arc::clone(&params);
        bundle.push(TaskSpawner::<Ping>::new(move |t| t.run(&params)));
    }
    {
        let params = Arc::clone(&params);
        let run = move |t: Rebuild| t.run(&params);
        bundle.push(TaskSpawner::<Rebuild>::new_serialized(run));
    }
    let any = bundle.into_any().expect("two lanes bundled");

    // Each type resolves to its own lane; an undeclared type does not.
    any.downcast::<Ping>()
        .expect("ping lane")
        .try_spawn(Ping)
        .expect("queue has room");
    any.downcast::<Rebuild>()
        .expect("rebuild lane")
        .try_spawn(Rebuild)
        .expect("queue has room");
    assert!(
        any.downcast::<Absent>().is_none(),
        "an undeclared task type resolves to no lane"
    );

    // Ping adds 1, Rebuild adds 100: 101 total proves each ran its own
    // handler, off its own lane.
    let start = Instant::now();
    while ran.load(Ordering::Relaxed) < 101 && start.elapsed() < Duration::from_secs(2) {
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        ran.load(Ordering::Relaxed),
        101,
        "both lanes ran their own handler"
    );
}

#[test]
fn background_task_runs_when_scheduled_from_editor_context() {
    use truce_core::editor::{PluginContext, for_test_params};

    let params = Arc::new(TaskParams::new());
    let ran = Arc::clone(&params.ran);

    let spawner = {
        let params = Arc::clone(&params);
        TaskSpawner::<Ping>::new(move |task| task.run(&params))
    };
    let tasks = AnyTaskSpawner::new(&spawner);

    // The format wrappers stamp the spawner into the editor's context via
    // `with_tasks`; the editor recovers it with `ctx.tasks::<T>()`.
    let dyn_params: Arc<dyn truce_params::Params> = params;
    let ctx: PluginContext = for_test_params(dyn_params).with_tasks(Some(tasks));
    ctx.tasks::<Ping>()
        .expect("editor context carries the spawner")
        .try_spawn(Ping)
        .expect("queue has room");

    let start = Instant::now();
    while ran.load(Ordering::Relaxed) == 0 && start.elapsed() < Duration::from_secs(2) {
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(ran.load(Ordering::Relaxed), 1, "editor-scheduled task ran");
}
