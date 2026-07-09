//! Managed background-task pool.
//!
//! A process-global pool of worker threads runs plugin `run_task`
//! handlers off the audio thread. Each plugin instance owns a
//! preallocated, wait-free inbound queue via a [`TaskSpawner`]: the
//! audio thread (or the editor, or `init`) pushes tasks without
//! allocating or blocking, and a pool worker drains them. Feedback to
//! the audio thread stays the plugin's job through shared `#[skip]`
//! channels - the pool owns only the worker threads and the inbound
//! queue.
//!
//! The pool is shared across every instance in the process (one small
//! set of threads, not one thread per instance) and initializes lazily
//! the first time any instance actually schedules a task, so a plugin
//! that never declares a `BackgroundTask` spawns no threads.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};
use std::sync::{Arc, OnceLock};
use std::thread::{self, Thread};
use std::time::Duration;

use crossbeam_queue::ArrayQueue;

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
    /// `run(task)` is `move |task| L::run_task(task, &params)`, built
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

impl<T: Send + 'static> Drain for Sink<T> {
    fn drain(&self) {
        // Clear the flag before draining so a task pushed mid-drain
        // re-arms the sink instead of being stranded. This clear and the
        // queue reads below form a StoreLoad pair against the producer's
        // push + `arm` swap; Release/AcqRel don't order a store followed
        // by a load of a *different* location, so without the SeqCst store
        // + fence a worker could clear the flag, read the queue empty, and
        // a concurrent producer could push a task and read the flag still
        // `true` - stranding that task with nothing scheduled to drain it.
        // Only SeqCst forbids that reordering. The stranded task self-heals
        // on the sink's next `arm`, so continuous work (a knob sweep) is
        // fine, but the last one-shot before an idle period would hang.
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
            let handle = thread::Builder::new()
                .name("truce-task-pool".into())
                .spawn(move || worker_loop(&shared))
                .expect("spawn truce task-pool worker");
            workers.push(handle.thread().clone());
        }
        Pool { shared, workers }
    })
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
    if pool.shared.injector.push(sink).is_err() {
        return false;
    }
    if !pool.workers.is_empty() {
        let i = pool.shared.next.fetch_add(1, Ordering::Relaxed) % pool.workers.len();
        pool.workers[i].unpark();
    }
    true
}

/// A cheap-to-clone handle for scheduling background tasks onto the
/// shared pool. Held by the shell and handed to the plugin through
/// [`InitContext`], `ProcessContext`, and the editor's `PluginContext`.
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
    /// monomorphized `move |task| L::run_task(task, &params)`, built once
    /// by the shell. The pool itself is not started until the first task
    /// is actually scheduled, so constructing a spawner for a plugin that
    /// never schedules costs only the (small) inbound queue.
    pub fn new(run: impl Fn(T) + Send + Sync + 'static) -> Self {
        Self {
            sink: Arc::new(Sink {
                queue: ArrayQueue::new(TASK_QUEUE_PREALLOC),
                coalesced: ArrayQueue::new(1),
                scheduled: AtomicBool::new(false),
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

/// A type-erased [`TaskSpawner`], so the concrete `ProcessContext` /
/// `InitContext` (whose signatures are fixed by the leaf trait and can't
/// name the plugin's task type) can carry the handle and hand back a
/// typed spawner on demand via [`Self::downcast`].
#[derive(Clone)]
pub struct AnyTaskSpawner(Arc<dyn std::any::Any + Send + Sync>);

impl AnyTaskSpawner {
    /// Erase a typed spawner. Called by the shell when it wires tasks.
    #[must_use]
    pub fn new<T: Send + 'static>(spawner: &TaskSpawner<T>) -> Self {
        Self(Arc::new(spawner.clone()) as Arc<dyn std::any::Any + Send + Sync>)
    }

    /// Recover the typed spawner. `None` if this handle was erased from a
    /// different task type (a caller asking for the wrong `T`).
    #[must_use]
    pub fn downcast<T: Send + 'static>(&self) -> Option<TaskSpawner<T>> {
        self.0.downcast_ref::<TaskSpawner<T>>().cloned()
    }
}

/// Context handed to `init` so a plugin can schedule startup background
/// work before the first block. Concrete (not generic over the task
/// type) because `init`'s signature lives on the leaf trait; recover the
/// typed spawner with [`Self::tasks`]. Params arrive as the separate
/// `init` argument.
pub struct InitContext {
    tasks: Option<AnyTaskSpawner>,
}

impl InitContext {
    #[must_use]
    pub fn new(tasks: Option<AnyTaskSpawner>) -> Self {
        Self { tasks }
    }

    /// The task spawner for this instance's `BackgroundTasks::Task`, or
    /// `None` if the plugin wired no `tasks:` on `plugin!`.
    #[must_use]
    pub fn tasks<T: Send + 'static>(&self) -> Option<TaskSpawner<T>> {
        self.tasks.as_ref().and_then(AnyTaskSpawner::downcast::<T>)
    }
}

#[cfg(test)]
mod tests {
    // `TASK_QUEUE_PREALLOC` is 256, so casting it to `u32` for the loop
    // bounds is always exact.
    #![allow(clippy::cast_possible_truncation)]

    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

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

    #[test]
    fn runs_scheduled_tasks_off_thread() {
        let counter = Arc::new(AtomicU32::new(0));
        let sum = Arc::new(AtomicU32::new(0));
        let (c, s) = (Arc::clone(&counter), Arc::clone(&sum));
        let spawner = TaskSpawner::<u32>::new(move |n| {
            c.fetch_add(1, Ordering::Relaxed);
            s.fetch_add(n, Ordering::Relaxed);
        });

        for n in 1..=10 {
            spawner.try_spawn(n).expect("queue has room");
        }

        assert!(
            wait_until(Duration::from_secs(2), || counter.load(Ordering::Relaxed)
                == 10),
            "all ten tasks ran"
        );
        assert_eq!(sum.load(Ordering::Relaxed), 55);
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
        let ran = Arc::new(AtomicU32::new(0));
        let r = Arc::clone(&ran);
        let spawner = TaskSpawner::<bool>::new(move |should_panic| {
            assert!(!should_panic, "intentional panic, caught by the pool");
            r.fetch_add(1, Ordering::Relaxed);
        });
        spawner.try_spawn(true).expect("queue has room"); // panics in the handler
        spawner.try_spawn(false).expect("queue has room"); // must still run
        assert!(
            wait_until(Duration::from_secs(2), || ran.load(Ordering::Relaxed) == 1),
            "the worker survived the panic and ran the next task"
        );
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
}
