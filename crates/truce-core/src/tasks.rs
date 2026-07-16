//! Managed background-task pool.
//!
//! A process-global pool of worker threads runs plugin
//! `BackgroundTask::run` handlers off the audio thread. Each plugin
//! instance owns a
//! preallocated, wait-free inbound queue via a [`TaskSpawner`]: the
//! audio thread (or the editor, or `init`) pushes tasks without
//! allocating or blocking, and a pool worker drains them. Feedback to
//! the audio thread stays the plugin's job through shared `#[skip]`
//! channels - the pool owns only the worker threads and the inbound
//! queue.
//!
//! ## Concurrency
//!
//! By default drains are **not** mutually exclusive: the stranding-
//! avoidance handshake clears a sink's `scheduled` flag before draining,
//! so a burst that re-arms an instance mid-drain can hand a second idle
//! worker the same sink - `run` can run concurrently with itself for
//! one instance. Handlers must therefore be reentrancy-safe: talk to the
//! audio thread only through lock-free / atomic channels (the reverb
//! example's MPMC handoff), or guard shared mutable state (the
//! `AudioTap::drain_with` `try_lock` idiom). A plugin that can't meet that
//! contract sets `BackgroundTask::SERIALIZED = true`, and the pool then
//! runs that instance's handler one at a time.
//!
//! The pool is shared across every instance in the process (one small
//! set of threads, not one thread per instance) and initializes lazily
//! the first time any instance actually schedules a task, so a plugin
//! that never declares a `BackgroundTask` spawns no threads.
//!
//! Because the pool is shared and small (`available_parallelism() - 1`,
//! as few as one thread), task handlers must stay short and
//! non-blocking: one plugin that blocks on I/O or a lock stalls every
//! other instance's background work. Long or blocking work belongs on a
//! plugin's own thread (`AudioTap::spawn_worker`), not the pool.

#[cfg(any(unix, windows))]
use std::ffi::c_void;
#[cfg(unix)]
use std::ffi::{c_char, c_int};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};
use std::sync::{Arc, OnceLock};
use std::thread::{self, Thread};
use std::time::Duration;

use crossbeam_queue::ArrayQueue;

use crate::snapshot::SnapshotPublisher;

/// Preallocated inbound-queue capacity per instance. Mirrors
/// `EVENT_LIST_PREALLOC`: a block that schedules more tasks than this
/// drops the overflow (`try_spawn` returns `Err`) rather than
/// allocating on the audio thread.
pub const TASK_QUEUE_PREALLOC: usize = 256;

/// How many instances can have pending work queued in the pool at once.
/// Sized well past any realistic simultaneous-instance count.
const INJECTOR_CAP: usize = 4096;

/// How long a worker parks before a defensive re-check. Wakes are
/// explicit (`unpark` after a push), so this only bounds the worst case
/// if an `unpark` is ever missed.
const PARK_TIMEOUT: Duration = Duration::from_secs(1);

/// A drainable instance queue, type-erased so the one pool holds many
/// task types at once.
trait Drain: Send + Sync {
    fn drain(&self);
}

/// Per-instance inbound queue plus the monomorphized handler. Shared
/// (`Arc`) between the schedulers (audio thread / editor / init, via
/// [`TaskSpawner`]) and the pool worker that drains it.
struct Sink<T: Send + 'static> {
    /// `try_spawn`: FIFO, every queued task runs.
    queue: ArrayQueue<T>,
    /// `spawn_coalescing`: a single slot. `force_push` keeps only the
    /// newest target, and `drain` runs it at most once, so a burst of
    /// requests between two drains collapses to one execution instead of
    /// running one build per intermediate target.
    coalesced: ArrayQueue<T>,
    /// Coalesces wake-ups: set when this sink is already queued in the
    /// injector, so a burst of pushes injects it once.
    scheduled: AtomicBool,
    /// Serialized ("one-slot") mode: when set, at most one worker runs
    /// `run` for this sink at a time. `false` (default) lets a second
    /// worker drain concurrently for throughput.
    serialized: bool,
    /// Exclusive-drain guard for [`Self::serialized`]. A worker that finds
    /// it already held bows out; the holder's re-check loop in `drain`
    /// picks up whatever the bower-out was injected for, so nothing is
    /// stranded. Unused in the concurrent (default) mode.
    draining: AtomicBool,
    /// `run(task)` is `move |task| task.run(&params)`, built
    /// once when the instance registers - never per task.
    run: Box<dyn Fn(T) + Send + Sync>,
}

impl<T: Send + 'static> Sink<T> {
    /// Run one task, catching panics so a bad handler can't kill the
    /// shared worker (which would strand every other instance's tasks).
    /// `run`/`task` are effectively unwind-safe: `run` is `&`-borrowed and
    /// a poisoned task is simply dropped.
    fn run_one(&self, task: T) {
        let run = &self.run;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(task)));
    }
}

impl<T: Send + 'static> Sink<T> {
    /// Clear `scheduled`, then run every currently-queued task once. The
    /// clear-before-drain + `SeqCst` fence is the stranding-avoidance
    /// handshake: a task pushed mid-drain re-arms the sink (its `arm` swap
    /// sees `scheduled == false`) instead of being stranded. Release/AcqRel
    /// don't order a store followed by a load of a *different* location, so
    /// without the `SeqCst` store + fence a worker could clear the flag, read
    /// the queue empty, and a concurrent producer could push a task and read
    /// the flag still `true` - stranding it. Only `SeqCst` forbids that.
    fn drain_queues(&self) {
        self.scheduled.store(false, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        // The coalesced slot held only the newest target, so a burst of
        // `spawn_coalescing` calls since the last drain runs once here, not
        // once per intermediate target.
        if let Some(task) = self.coalesced.pop() {
            self.run_one(task);
        }
        // FIFO tasks each run.
        while let Some(task) = self.queue.pop() {
            self.run_one(task);
        }
    }
}

impl<T: Send + 'static> Drain for Sink<T> {
    fn drain(&self) {
        // Concurrent (default) mode: a second worker may drain this sink at
        // the same time. Handlers must be reentrancy-safe (see
        // `BackgroundTask::SERIALIZED`).
        if !self.serialized {
            self.drain_queues();
            return;
        }
        // Serialized ("one-slot") mode: run the handler for this instance on
        // at most one worker at a time. A worker that finds the guard held
        // is inert and returns - it touches nothing, so the `scheduled`
        // handshake below stays the sole no-stranding signal, exactly as in
        // the concurrent path.
        if self.draining.swap(true, Ordering::Acquire) {
            return;
        }
        loop {
            self.drain_queues();
            self.draining.store(false, Ordering::Release);
            fence(Ordering::SeqCst);
            // Re-check the *scheduled flag*, not the queue: a producer that
            // armed during the drain set it with a SeqCst swap ordered after
            // `drain_queues`'s SeqCst clear, so we either observe it here and
            // re-drain, or its swap saw our clear and injected a fresh drain.
            // (Reading the queue instead would race - a plain queue load
            // isn't synchronized with the producer's push, so it could miss a
            // task a re-injection carried and strand it.) `drain_queues`
            // clears the flag each pass, so the loop makes progress and can't
            // spin: at most one extra empty drain after the last arm.
            if !self.scheduled.load(Ordering::SeqCst) {
                return;
            }
            // Work remains. Re-take the guard and drain again; if another
            // worker took it first, that worker now owns the remainder.
            if self.draining.swap(true, Ordering::Acquire) {
                return;
            }
        }
    }
}

/// Worker-visible pool state: the injector of ready sinks. Held in an
/// `Arc` so every worker closure can reach it.
struct Shared {
    injector: ArrayQueue<Arc<dyn Drain>>,
    /// Round-robin cursor for choosing which worker to wake.
    next: AtomicUsize,
}

struct Pool {
    shared: Arc<Shared>,
    workers: Vec<Thread>,
}

static POOL: OnceLock<Pool> = OnceLock::new();

fn pool() -> &'static Pool {
    POOL.get_or_init(|| {
        let shared = Arc::new(Shared {
            injector: ArrayQueue::new(INJECTOR_CAP),
            next: AtomicUsize::new(0),
        });
        // One fewer than the core count, floored at one, so the pool
        // never starves the audio and main threads on a small machine.
        let n = thread::available_parallelism().map_or(1, |p| p.get().saturating_sub(1).max(1));
        let mut workers = Vec::with_capacity(n);
        for _ in 0..n {
            let shared = Arc::clone(&shared);
            match thread::Builder::new()
                .name("truce-task-pool".into())
                .spawn(move || worker_loop(&shared))
            {
                Ok(handle) => workers.push(handle.thread().clone()),
                // A failed spawn (thread/memory exhaustion) must not panic:
                // pool init can run behind an `extern "C"` boundary in a
                // host that doesn't catch unwinds (VST3 / VST2 / AAX / LV2),
                // where an unwind aborts the whole DAW. Keep whatever
                // workers spawned; if none did, `schedule` drops tasks
                // instead of queueing work nothing will drain.
                Err(e) => {
                    eprintln!("[truce] task-pool worker spawn failed: {e}");
                    break;
                }
            }
        }
        // The workers loop forever with no shutdown path, so pin this
        // module in memory: a host that `dlclose`s it on last-instance
        // teardown must not leave a parked worker to wake into unmapped
        // code. Only once we actually have workers to protect.
        if !workers.is_empty() {
            pin_current_module();
        }
        Pool { shared, workers }
    })
}

/// Pin the module `truce-core` is linked into (the plugin cdylib in a
/// static build, or the hot-reload shell cdylib) so it is never
/// unmapped. The pool's workers loop forever - park + drain - with no
/// shutdown path and no `JoinHandle`s; if the host `dlclose`d the module
/// on last-instance teardown while a worker was parked, it would wake
/// with its program counter in unmapped code and take down the DAW
/// (Bitwig, Cubase, and JUCE hosts all unload). Pinning trades a small
/// permanent mapping for eliminating that crash class - the standard fix
/// for a persistent plugin helper thread. Called once, and only when the
/// pool actually spawned workers.
#[cfg(unix)]
fn pin_current_module() {
    // Field names mirror the platform's `Dl_info`; only `dli_fname` is
    // read, the rest are here for correct C layout (`dladdr` writes all).
    #[repr(C)]
    #[allow(clippy::struct_field_names)]
    struct DlInfo {
        dli_fname: *const c_char,
        dli_fbase: *mut c_void,
        dli_sname: *const c_char,
        dli_saddr: *mut c_void,
    }
    unsafe extern "C" {
        fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int;
        fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    }
    // RTLD_NOLOAD | RTLD_NODELETE: re-reference the already-loaded object
    // and mark it non-deletable. The flag values differ between glibc and
    // Darwin.
    #[cfg(target_os = "linux")]
    const FLAGS: c_int = 0x0004 | 0x1000;
    #[cfg(not(target_os = "linux"))]
    const FLAGS: c_int = 0x0010 | 0x0080;

    // SAFETY: `dladdr` reads the address of a live function in this module
    // and fills `info` with loader-owned strings valid for the immediate
    // `dlopen`. NOLOAD only re-references the already-mapped object; the
    // returned handle is intentionally leaked so NODELETE persists.
    unsafe {
        let mut info: DlInfo = core::mem::zeroed();
        if dladdr(pin_current_module as *const c_void, &raw mut info) != 0
            && !info.dli_fname.is_null()
        {
            let _ = dlopen(info.dli_fname, FLAGS);
        }
    }
}

#[cfg(windows)]
fn pin_current_module() {
    unsafe extern "system" {
        fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut c_void) -> i32;
    }
    // GET_MODULE_HANDLE_EX_FLAG_PIN | ..._FROM_ADDRESS: interpret `name`
    // as an address inside this module and bump its load count so it
    // never unloads.
    const PIN_FROM_ADDRESS: u32 = 0x0000_0001 | 0x0000_0004;

    // SAFETY: `name` is the address of a live function in this module;
    // `module` receives the pinned handle, which we intentionally leak.
    unsafe {
        let mut module: *mut c_void = core::ptr::null_mut();
        let _ = GetModuleHandleExW(
            PIN_FROM_ADDRESS,
            pin_current_module as *const u16,
            &raw mut module,
        );
    }
}

#[cfg(not(any(unix, windows)))]
fn pin_current_module() {}

/// Eagerly start the shared pool on the calling thread. The shell calls
/// this at instantiation (the host/main thread) when a plugin wires a
/// task spawner, so the worker threads exist before the audio thread ever
/// schedules. Without it a plugin that first schedules from `process()`
/// (the "rebuild the filter when a knob moves" pattern, with no startup
/// work in `init` to warm the pool) would cold-start the threads inside
/// the audio callback. Idempotent: the pool is a process-global singleton
/// after the first call.
pub fn warm_pool() {
    let _ = pool();
}

fn worker_loop(shared: &Shared) -> ! {
    loop {
        while let Some(sink) = shared.injector.pop() {
            sink.drain();
        }
        // Nothing pending: park. A concurrent push + `unpark` either
        // beats the park (the token makes this return at once) or wakes
        // us; `PARK_TIMEOUT` is a belt-and-suspenders re-check.
        thread::park_timeout(PARK_TIMEOUT);
    }
}

/// Enqueue a ready sink and wake a worker. Wait-free: `injector.push`
/// is lock-free and `unpark` is a bounded, non-blocking wake (the same
/// primitive the manual worker pattern uses). Returns `false` if the
/// injector is full so the caller can clear `scheduled` and let a later
/// `arm` retry, rather than leaving the sink flagged-but-unqueued.
fn schedule(sink: Arc<dyn Drain>) -> bool {
    let pool = pool();
    // No workers (every spawn failed at pool init): drop the task rather
    // than queue work nothing will ever drain, matching the "queue full ->
    // drop" policy. The caller clears `scheduled` so a later `arm` retries.
    if pool.workers.is_empty() {
        return false;
    }
    if pool.shared.injector.push(sink).is_err() {
        return false;
    }
    let i = pool.shared.next.fetch_add(1, Ordering::Relaxed) % pool.workers.len();
    pool.workers[i].unpark();
    true
}

/// A cheap-to-clone handle for scheduling background tasks onto the
/// shared pool. Held by the shell and handed to the plugin through
/// [`InitContext`], `ProcessContext`, and the editor's `PluginContext`.
///
/// A spawner built with [`Self::new`] may run its handler concurrently
/// with itself for one instance (see the module's Concurrency section);
/// [`Self::new_serialized`] runs it one at a time.
pub struct TaskSpawner<T: Send + 'static> {
    sink: Arc<Sink<T>>,
}

impl<T: Send + 'static> Clone for TaskSpawner<T> {
    fn clone(&self) -> Self {
        Self {
            sink: Arc::clone(&self.sink),
        }
    }
}

impl<T: Send + 'static> TaskSpawner<T> {
    /// Register an instance's handler with the shared pool. `run` is the
    /// monomorphized `move |task| task.run(&params)`, built once
    /// by the shell. The pool itself is not started until the first task
    /// is actually scheduled, so constructing a spawner for a plugin that
    /// never schedules costs only the (small) inbound queue.
    ///
    /// The handler may run concurrently with itself for one instance; use
    /// [`Self::new_serialized`] for a handler that isn't reentrancy-safe.
    pub fn new(run: impl Fn(T) + Send + Sync + 'static) -> Self {
        Self::with_mode(run, false)
    }

    /// Like [`Self::new`], but the pool runs the handler for a given
    /// instance one at a time ("one-slot" mode). The shell selects this
    /// when the plugin's `BackgroundTask::SERIALIZED` is `true`.
    pub fn new_serialized(run: impl Fn(T) + Send + Sync + 'static) -> Self {
        Self::with_mode(run, true)
    }

    fn with_mode(run: impl Fn(T) + Send + Sync + 'static, serialized: bool) -> Self {
        Self {
            sink: Arc::new(Sink {
                queue: ArrayQueue::new(TASK_QUEUE_PREALLOC),
                coalesced: ArrayQueue::new(1),
                scheduled: AtomicBool::new(false),
                serialized,
                draining: AtomicBool::new(false),
                run: Box::new(run),
            }),
        }
    }

    /// Enqueue a task, running it on the pool as soon as a worker is
    /// free. Wait-free. Returns `Err(task)` if the inbound queue is full
    /// (the audio thread decides what to do - drop, or coalesce via
    /// [`Self::spawn_coalescing`] - rather than block).
    ///
    /// # Errors
    ///
    /// Returns the task back when the preallocated inbound queue is full.
    pub fn try_spawn(&self, task: T) -> Result<(), T> {
        self.sink.queue.push(task)?;
        self.arm();
        Ok(())
    }

    /// Post a task into the single coalescing slot, replacing any
    /// still-unrun target. Wait-free, never rejects. Only the newest
    /// survives and the worker runs it at most once per drain, so a knob
    /// sweep that outruns the handler collapses to one execution, not one
    /// build per intermediate target. The displaced target drops on the
    /// caller (the audio thread on the hot path), so a coalescing task
    /// type should be cheap to drop - a small `Copy` request, not an
    /// owned buffer.
    pub fn spawn_coalescing(&self, task: T) {
        let _ = self.sink.coalesced.force_push(task);
        self.arm();
    }

    /// Inject this sink into the pool if it isn't already queued.
    fn arm(&self) {
        // Pairs with the SeqCst store + fence in `Sink::drain`: the caller
        // pushed the task just before this, and that push must be ordered
        // before the flag swap below, or the StoreLoad race described in
        // `drain` strands the task. The fence + SeqCst swap give the total
        // order that Release/AcqRel can't. Still wait-free (one barrier,
        // no lock/alloc/syscall), and `arm` runs at most once per block.
        fence(Ordering::SeqCst);
        if !self.sink.scheduled.swap(true, Ordering::SeqCst) {
            let sink: Arc<dyn Drain> = Arc::clone(&self.sink) as Arc<dyn Drain>;
            if !schedule(sink) {
                // Injector full: we flagged the sink but couldn't queue it.
                // Clear the flag so the next `arm` re-attempts injection
                // instead of skipping on a stale `true`.
                self.sink.scheduled.store(false, Ordering::SeqCst);
            }
        }
    }
}

/// One type-erased lane. Each element of [`AnyTaskSpawner`] holds one
/// `TaskSpawner<T>` for a distinct task type.
type ErasedLane = Arc<dyn std::any::Any + Send + Sync>;

/// A bundle of type-erased [`TaskSpawner`]s - one lane per declared task
/// type - so the concrete `ProcessContext` / `InitContext` (whose
/// signatures are fixed by the leaf trait and can't name the plugin's task
/// types) can carry every lane and hand back the right typed spawner on
/// demand via [`Self::downcast`]. Cheap to clone (one `Arc`).
#[derive(Clone)]
pub struct AnyTaskSpawner(Arc<[ErasedLane]>);

impl AnyTaskSpawner {
    /// Erase a single typed spawner into a one-lane bundle.
    #[must_use]
    pub fn new<T: Send + 'static>(spawner: &TaskSpawner<T>) -> Self {
        Self(Arc::from(vec![Arc::new(spawner.clone()) as ErasedLane]))
    }

    /// Bundle several already-erased lanes (one per task type). The
    /// `plugin!` macro builds the lanes with [`TaskSpawnerBundle`].
    #[must_use]
    pub fn from_lanes(lanes: Vec<ErasedLane>) -> Self {
        Self(Arc::from(lanes))
    }

    /// Recover the typed spawner for task type `T`, or `None` if no lane of
    /// that type was declared. Lanes have distinct types, so at most one
    /// matches.
    #[must_use]
    pub fn downcast<T: Send + 'static>(&self) -> Option<TaskSpawner<T>> {
        self.0
            .iter()
            .find_map(|lane| lane.downcast_ref::<TaskSpawner<T>>().cloned())
    }
}

/// Builder the `plugin!` macro uses to collect one lane per declared task
/// type into an [`AnyTaskSpawner`]. Kept separate so the macro never has to
/// name the erased-lane type.
#[derive(Default)]
pub struct TaskSpawnerBundle(Vec<ErasedLane>);

impl TaskSpawnerBundle {
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Add one task type's spawner to the bundle.
    pub fn push<T: Send + 'static>(&mut self, spawner: TaskSpawner<T>) {
        self.0.push(Arc::new(spawner) as ErasedLane);
    }

    /// Finish: `Some` bundle, or `None` when no lanes were added (a plugin
    /// that declared no tasks), matching the `Option<AnyTaskSpawner>` the
    /// shell threads through.
    #[must_use]
    pub fn into_any(self) -> Option<AnyTaskSpawner> {
        if self.0.is_empty() {
            None
        } else {
            Some(AnyTaskSpawner::from_lanes(self.0))
        }
    }
}

/// Context handed to `init` so a plugin can schedule startup background
/// work before the first block. Concrete (not generic over the task
/// type) because `init`'s signature lives on the leaf trait; recover the
/// typed spawner with [`Self::tasks`]. Params arrive as the separate
/// `init` argument.
pub struct InitContext {
    tasks: Option<AnyTaskSpawner>,
    /// Handle for the off-thread snapshot lane (large state save), when
    /// the shell wired one. `None` in `--shell` hot-reload builds, where
    /// (like `tasks`) it isn't threaded across the dylib boundary yet -
    /// so the off-thread snapshot path is a static-build feature.
    snapshot: Option<SnapshotPublisher>,
}

impl InitContext {
    #[must_use]
    pub fn new(tasks: Option<AnyTaskSpawner>) -> Self {
        Self {
            tasks,
            snapshot: None,
        }
    }

    /// Attach the off-thread snapshot publisher (see [`SnapshotPublisher`]).
    #[must_use]
    pub fn with_snapshot(mut self, snapshot: SnapshotPublisher) -> Self {
        self.snapshot = Some(snapshot);
        self
    }

    /// The task spawner for task type `T`, or `None` if the plugin declared
    /// no `tasks:` lane of that type on `plugin!`.
    #[must_use]
    pub fn tasks<T: Send + 'static>(&self) -> Option<TaskSpawner<T>> {
        self.tasks.as_ref().and_then(AnyTaskSpawner::downcast::<T>)
    }

    /// Handle to publish large custom state off the audio thread. Stash it
    /// in your DSP state and call `publish` from a background-task handler
    /// after your state changes. `None` in `--shell` builds; use
    /// `snapshot_into` for small state, which works everywhere.
    #[must_use]
    pub fn snapshot_publisher(&self) -> Option<SnapshotPublisher> {
        self.snapshot.clone()
    }
}

#[cfg(test)]
mod tests {
    // `TASK_QUEUE_PREALLOC` is 256, so casting it to `u32` for the loop
    // bounds is always exact.
    #![allow(clippy::cast_possible_truncation)]

    use super::*;
    use crate::snapshot::{SnapshotPublisher, SnapshotSlot};
    use std::sync::atomic::AtomicU32;
    use std::sync::{Condvar, Mutex};
    use std::time::Instant;

    #[test]
    fn init_context_exposes_snapshot_publisher() {
        let slot = SnapshotSlot::new();
        let cx = InitContext::new(None).with_snapshot(SnapshotPublisher::new(&slot));
        // The plugin captures this in `init` and publishes large state
        // through it off the audio thread.
        cx.snapshot_publisher()
            .expect("publisher present")
            .publish(vec![1, 2, 3]);
        assert_eq!(slot.read(), Some(vec![1, 2, 3]));
        // No snapshot wired (the `--shell` / no-slot case) yields None.
        assert!(InitContext::new(None).snapshot_publisher().is_none());
    }

    fn wait_until(deadline: Duration, mut done: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        done()
    }

    /// Blocking completion latch: the pool handler bumps it, the test
    /// blocks until a count is reached. Unlike the wall-clock `wait_until`,
    /// it has no deadline, so it stays deterministic under Miri, whose
    /// interpreter can't run background tasks within a real-time budget.
    /// A `Mutex`/`Condvar` (not an `mpsc::Sender`, which is `!Sync`) keeps
    /// the handler `Fn + Send + Sync`.
    #[derive(Default)]
    struct Latch {
        ran: Mutex<u32>,
        woke: Condvar,
    }

    impl Latch {
        fn bump(&self) {
            *self.ran.lock().unwrap() += 1;
            self.woke.notify_all();
        }

        fn wait_for(&self, target: u32) {
            let mut ran = self.ran.lock().unwrap();
            while *ran < target {
                ran = self.woke.wait(ran).unwrap();
            }
        }
    }

    #[test]
    fn warm_pool_starts_workers_and_is_idempotent() {
        // Warming off the audio thread is what keeps the first
        // audio-thread schedule from cold-starting the workers inline.
        warm_pool();
        warm_pool();
        assert!(
            !pool().workers.is_empty(),
            "warming spawns at least one worker"
        );
    }

    #[test]
    fn pin_current_module_is_safe_and_idempotent() {
        // Smoke: pinning must never panic or crash. In the test binary
        // the loader query resolves to the harness executable (never
        // unloaded anyway); we only assert the FFI is benign and can run
        // more than once.
        super::pin_current_module();
        super::pin_current_module();
    }

    #[test]
    fn runs_scheduled_tasks_off_thread() {
        let latch = Arc::new(Latch::default());
        let sum = Arc::new(AtomicU32::new(0));
        let (l, s) = (Arc::clone(&latch), Arc::clone(&sum));
        let spawner = TaskSpawner::<u32>::new(move |n| {
            s.fetch_add(n, Ordering::Relaxed);
            l.bump();
        });

        for n in 1..=10 {
            spawner.try_spawn(n).expect("queue has room");
        }

        // Block until all ten ran. The latch mutex orders every handler's
        // `sum` write before the read below, so the Relaxed sum is exact.
        latch.wait_for(10);
        assert_eq!(sum.load(Ordering::Relaxed), 55, "all ten tasks ran");
    }

    #[test]
    fn full_queue_returns_the_task() {
        // Handler blocks on a gate so the queue can actually fill.
        let gate = Arc::new(AtomicBool::new(false));
        let g = Arc::clone(&gate);
        let spawner = TaskSpawner::<u32>::new(move |_| {
            while !g.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
        });

        // First task is picked up and blocks a worker; fill the rest.
        let mut rejected = 0u32;
        for n in 0..(TASK_QUEUE_PREALLOC as u32 + 64) {
            if spawner.try_spawn(n).is_err() {
                rejected += 1;
            }
        }
        assert!(rejected > 0, "a full inbound queue rejects further tasks");
        gate.store(true, Ordering::Release);
    }

    #[test]
    fn panicking_task_does_not_kill_the_worker() {
        let latch = Arc::new(Latch::default());
        let l = Arc::clone(&latch);
        let spawner = TaskSpawner::<bool>::new(move |should_panic| {
            assert!(!should_panic, "intentional panic, caught by the pool");
            l.bump();
        });
        spawner.try_spawn(true).expect("queue has room"); // panics in the handler
        spawner.try_spawn(false).expect("queue has room"); // must still run
        // If the panic had killed the worker, the survivor never runs and
        // this blocks forever - surfaced as a hung test, not a false pass.
        latch.wait_for(1);
    }

    #[test]
    fn coalescing_never_rejects() {
        let last = Arc::new(AtomicU32::new(0));
        let l = Arc::clone(&last);
        let spawner = TaskSpawner::<u32>::new(move |n| {
            l.store(n, Ordering::Relaxed);
        });
        for n in 0..(TASK_QUEUE_PREALLOC as u32 * 4) {
            spawner.spawn_coalescing(n); // never panics, never blocks
        }
        let target = TASK_QUEUE_PREALLOC as u32 * 4 - 1;
        assert!(
            wait_until(Duration::from_secs(2), || last.load(Ordering::Relaxed)
                == target),
            "the newest task always runs"
        );
    }

    #[test]
    fn serialized_runs_one_at_a_time_and_drops_nothing() {
        // One-slot mode: the handler must never run concurrently with
        // itself for this instance, and every FIFO task must still run.
        const N: u32 = 64;
        let in_flight = Arc::new(AtomicU32::new(0));
        let peak = Arc::new(AtomicU32::new(0));
        let latch = Arc::new(Latch::default());
        let (inf, pk, l) = (
            Arc::clone(&in_flight),
            Arc::clone(&peak),
            Arc::clone(&latch),
        );
        let spawner = TaskSpawner::<u32>::new_serialized(move |_| {
            let now = inf.fetch_add(1, Ordering::AcqRel) + 1;
            pk.fetch_max(now, Ordering::AcqRel);
            // Widen the window so a second worker would overlap if the guard
            // let it - a bare increment could hide a real race.
            thread::sleep(Duration::from_millis(1));
            inf.fetch_sub(1, Ordering::AcqRel);
            l.bump();
        });

        // Push across a burst so re-arms land mid-drain: each one re-injects
        // the sink and tempts an idle worker to pick it up concurrently.
        for n in 0..N {
            while spawner.try_spawn(n).is_err() {
                thread::sleep(Duration::from_millis(1));
            }
        }

        // Blocks until all N ran; a stranded task would hang here (a hung
        // test, not a false pass).
        latch.wait_for(N);
        assert_eq!(
            peak.load(Ordering::Acquire),
            1,
            "serialized: at most one handler in flight at a time"
        );
    }

    #[test]
    fn coalescing_collapses_to_the_newest() {
        let runs = Arc::new(AtomicU32::new(0));
        let last = Arc::new(AtomicU32::new(0));
        let (r, l) = (Arc::clone(&runs), Arc::clone(&last));
        let spawner = TaskSpawner::<u32>::new(move |n| {
            r.fetch_add(1, Ordering::Relaxed);
            l.store(n, Ordering::Relaxed);
        });

        // Fill the coalescing slot repeatedly without arming the pool, so
        // the burst collapses in the slot rather than racing a worker.
        // Then drain once and confirm the whole burst ran a single time,
        // as the newest target.
        for n in 1..=1000 {
            let _ = spawner.sink.coalesced.force_push(n);
        }
        spawner.sink.drain();

        assert_eq!(runs.load(Ordering::Relaxed), 1, "the burst ran once");
        assert_eq!(last.load(Ordering::Relaxed), 1000, "and it was the newest");
    }
}

// Model-checked proof that the schedule/drain handshake can't strand a
// task under any thread interleaving. Run with:
//   cargo test -p truce-core --features loom loom
//
// loom can't see into crossbeam's `ArrayQueue`, so this models the
// protocol directly: a one-slot queue (`item`) plus the `scheduled` flag,
// driven through the exact SeqCst store / fence / swap sequence that
// `Sink::drain` and `TaskSpawner::arm` use. Weakening either side to
// Release/AcqRel (dropping the SeqCst or the fences) makes loom find the
// stranding interleaving; the version below passes.
#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicBool, Ordering, fence};
    use loom::thread;

    #[test]
    fn schedule_drain_never_strands_a_task() {
        loom::model(|| {
            // Start with a drain in flight: the sink was scheduled
            // (`flag == true`) and a worker is about to drain an empty
            // queue, concurrent with a producer pushing one more task.
            let flag = Arc::new(AtomicBool::new(true));
            let item = Arc::new(AtomicBool::new(false));

            let (f, i) = (flag.clone(), item.clone());
            let worker = thread::spawn(move || {
                // `Sink::drain`: clear the flag, then check the queue. The
                // presence check must be a plain load (a `pop` reading
                // empty) - an RMW would always read the latest value in
                // modification order and so hide the StoreLoad staleness
                // this test exists to catch.
                f.store(false, Ordering::SeqCst);
                fence(Ordering::SeqCst);
                if i.load(Ordering::Acquire) {
                    i.store(false, Ordering::Release); // popped it
                }
            });

            // `try_spawn` + `arm`: push the task, then flag the sink.
            item.store(true, Ordering::Release);
            fence(Ordering::SeqCst);
            let was_scheduled = flag.swap(true, Ordering::SeqCst);
            // `was_scheduled == false` => the producer injects a fresh
            // drain (it set `flag = true`). `true` => it relies on the
            // in-flight drain to pick the task up.
            let _ = was_scheduled;

            worker.join().unwrap();

            // Safe end states: the queue is empty (some drain popped it),
            // or a drain is still scheduled (`flag == true`) to pick it up.
            // A pending task with `flag == false` is the stranding bug.
            let pending = item.load(Ordering::SeqCst);
            let scheduled = flag.load(Ordering::SeqCst);
            assert!(
                !pending || scheduled,
                "task stranded: pending with scheduled == false"
            );
        });
    }

    // The serialized ("one-slot") path adds a `draining` guard for mutual
    // exclusion. The no-stranding signal stays the `scheduled` handshake: a
    // worker that loses the guard is inert, and the winner re-checks the
    // *flag* (not the queue) after each drain, looping until it reads
    // `false`. This models that body for two workers plus a producer and
    // asserts the same invariant. Re-checking `item` instead of the flag
    // (an unsynchronized queue read) makes loom find the interleaving where
    // the winner misses the producer's task and it strands.
    #[test]
    fn serialized_drain_never_strands_a_task() {
        loom::model(|| {
            // A sink already scheduled with one queued task; two workers pop
            // it (the producer's re-inject can hand it to a second worker),
            // and the producer pushes one more task concurrently.
            let scheduled = Arc::new(AtomicBool::new(true));
            let item = Arc::new(AtomicBool::new(true));
            let draining = Arc::new(AtomicBool::new(false));

            // One execution of the serialized `Sink::drain` path. The
            // re-check loop is bounded to two passes - enough for the one
            // extra task a single producer can push.
            let worker =
                |scheduled: Arc<AtomicBool>, item: Arc<AtomicBool>, draining: Arc<AtomicBool>| {
                    if draining.swap(true, Ordering::Acquire) {
                        return; // loser: inert
                    }
                    for _ in 0..2 {
                        // drain_queues: clear the flag (SeqCst), fence, pop.
                        scheduled.store(false, Ordering::SeqCst);
                        fence(Ordering::SeqCst);
                        let _ = item.swap(false, Ordering::AcqRel);
                        draining.store(false, Ordering::Release);
                        fence(Ordering::SeqCst);
                        // Re-check the flag, not the queue.
                        if !scheduled.load(Ordering::SeqCst) {
                            return;
                        }
                        if draining.swap(true, Ordering::Acquire) {
                            return;
                        }
                    }
                };

            let (s1, i1, d1) = (scheduled.clone(), item.clone(), draining.clone());
            let w1 = thread::spawn(move || worker(s1, i1, d1));
            let (s2, i2, d2) = (scheduled.clone(), item.clone(), draining.clone());
            let w2 = thread::spawn(move || worker(s2, i2, d2));

            // `try_spawn` + `arm`: push the task, then flag the sink.
            item.store(true, Ordering::Release);
            fence(Ordering::SeqCst);
            let _ = scheduled.swap(true, Ordering::SeqCst);

            w1.join().unwrap();
            w2.join().unwrap();

            // Same invariant: no task left pending unless the flag is still
            // set for a future drain to pick it up.
            let pending = item.load(Ordering::SeqCst);
            let is_scheduled = scheduled.load(Ordering::SeqCst);
            assert!(
                !pending || is_scheduled,
                "serialized task stranded: pending with scheduled == false"
            );
        });
    }
}
