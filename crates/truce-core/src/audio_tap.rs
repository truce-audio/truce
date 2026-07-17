//! Audio tap: stream interleaved frames off the audio thread to a
//! background consumer.
//!
//! The audio thread pushes frames wait-free from `process`; a background
//! consumer drains them. This is the *stream + analyze* half of the
//! worker pattern (a spectrum analyzer, a loudness meter, an
//! oscilloscope), where [`crate::tasks`]'s discrete task pool is the
//! *construction offload* half.
//!
//! Two ways to drain:
//! - a `BackgroundTask` handler (in `truce_plugin`) on the shared pool,
//!   woken by a coalescing task each block - bounded threads, best when
//!   analysis is bursty or coalescable.
//! - a dedicated [`StreamWorker`] via [`AudioTap::spawn_worker`] - one
//!   owned thread that parks on the tap and drains sequentially. Consumer
//!   state lives thread-local (no lock), and it never stalls on unrelated
//!   pool work, at the cost of one thread per worker.
//!
//! The tap is a plain `#[skip]` field the plugin owns (usually
//! `Arc<AudioTap<S>>`), so both `process` (via `&params`) and the
//! handler (via `&params`) reach it - the same shared-`Arc` mechanism as
//! any audio -> worker channel. It composes with the pool rather than
//! introducing new threading:
//!
//! ```ignore
//! #[derive(Params)]
//! struct AnalyzerParams {
//!     #[skip]
//!     tap: Arc<AudioTap<f32>>,        // Default: a stereo tap at the default capacity
//!     // ... published spectrum atoms (also #[skip]) ...
//! }
//!
//! // process (audio thread):
//! params.tap.push_frames(&interleaved);
//! if let Some(t) = ctx.tasks::<Analyze>() { t.spawn_coalescing(Analyze); }
//!
//! // run (pool thread):
//! params.tap.drain_with(|chunk| { /* run the FFT, publish */ });
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle, Thread};
use std::time::Duration;

use crossbeam_queue::ArrayQueue;

/// Safety-net wake interval for a dedicated [`StreamWorker`]. Normal
/// wakeups come from `push_frames` unparking the thread; this only bounds
/// how long a missed unpark or a pending shutdown can go unnoticed.
const WORKER_PARK_TIMEOUT: Duration = Duration::from_millis(100);

/// Frame capacity [`AudioTap::default`] builds: 32768 stereo frames, a
/// generous consumer-scheduling margin (~170 ms at 192 kHz, ~740 ms at
/// 44.1 kHz). Call [`AudioTap::new`] for a different size or channel count.
const DEFAULT_TAP_FRAMES: usize = 32 * 1024;
/// Channel count [`AudioTap::default`] builds. Stereo is the common case;
/// call [`AudioTap::new`] for mono or higher channel counts.
const DEFAULT_TAP_CHANNELS: usize = 2;

/// A lock-free audio tap. The audio thread is the sole producer
/// ([`Self::push_frames`], wait-free); a background consumer drains it
/// ([`Self::drain_with`]). Whole-frame drop-on-full keeps a drop from
/// desyncing channels.
pub struct AudioTap<S> {
    ring: ArrayQueue<S>,
    channels: usize,
    /// Reused pop buffer whose lock also serializes drains: with a shared
    /// pool a second worker could pick up a coalesced drain while the
    /// first is still running, and a per-sample ring must have one
    /// consumer at a time. `try_lock` makes the second worker bow out
    /// (the first is already draining the same data). Locked only on the
    /// pool thread, never on the audio thread. Also retains any trailing
    /// partial frame between drains (see [`Self::drain_with`]): the
    /// producer pushes a frame's channels one at a time, so a concurrent
    /// drain can catch it mid-frame, and the leftover reassembles next call.
    scratch: Mutex<Vec<S>>,
    /// The dedicated [`StreamWorker`] thread, if one drains this tap.
    /// `push_frames` unparks it (a lock-free atomic load + unpark) so the
    /// consumer wakes without polling. Unset when the tap is drained by
    /// the shared pool instead.
    waker: OnceLock<Thread>,
}

/// Builds a stereo tap at a default capacity (32768 frames, ~170 ms at
/// 192 kHz), so a plugin can hold an `Arc<AudioTap<S>>` as a `#[skip]`
/// field and still `#[derive(Default)]` its params. Use [`AudioTap::new`]
/// when the size or channel count needs to differ from the default.
impl<S: Copy + Send + 'static> Default for AudioTap<S> {
    fn default() -> Self {
        Self::new(DEFAULT_TAP_FRAMES, DEFAULT_TAP_CHANNELS)
    }
}

impl<S: Copy + Send + 'static> AudioTap<S> {
    /// Build a tap holding up to `frame_capacity` interleaved frames of
    /// `channels` each. Size `frame_capacity` for the worst realistic
    /// consumer-scheduling gap; drop-on-full is the safety net beyond it
    /// (e.g. the analyzer uses ~32k frames, ~170 ms at 192 kHz).
    ///
    /// # Panics
    ///
    /// Panics if `channels` is zero.
    #[must_use]
    pub fn new(frame_capacity: usize, channels: usize) -> Self {
        assert!(channels > 0, "AudioTap needs at least one channel");
        Self {
            ring: ArrayQueue::new(frame_capacity * channels),
            channels,
            scratch: Mutex::new(Vec::new()),
            waker: OnceLock::new(),
        }
    }

    /// Channel count each frame carries.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Push interleaved frames from the audio thread. Wait-free. Drops
    /// whole frames (never a partial one, so a drop can't desync
    /// channels) once the ring is full.
    pub fn push_frames(&self, interleaved: &[S]) {
        for frame in interleaved.chunks_exact(self.channels) {
            // The producer is the sole pusher and the consumer only frees
            // space, so free room seen here is a lower bound - if a whole
            // frame fits now it still fits when we push it.
            if self.ring.capacity() - self.ring.len() < self.channels {
                break;
            }
            for &sample in frame {
                let _ = self.ring.push(sample);
            }
        }
        // Wake a dedicated worker, if one is attached. One unpark per
        // block; the park token means a push landing between the worker's
        // drain and its next park is never lost.
        if let Some(worker) = self.waker.get() {
            worker.unpark();
        }
    }

    /// Discard everything buffered. Called off the audio thread (e.g.
    /// from `reset` on a sample-rate change, so frames captured at the
    /// old rate aren't analyzed against the new one). Serialized with
    /// [`Self::drain_with`]; if a drain is mid-flight this is a no-op and
    /// the drain finishes the stale frames - `reset` should follow with
    /// the consumer's own state reset.
    pub fn clear(&self) {
        let Ok(mut guard) = self.scratch.try_lock() else {
            return;
        };
        while self.ring.pop().is_some() {}
        // Drop any partial frame carried from an interrupted drain, so it
        // can't prepend stale samples onto post-clear frames.
        guard.clear();
    }

    /// Drain the buffered whole frames and hand them to `f` as one
    /// interleaved slice - always a whole number of frames. Runs on the
    /// consumer (a pool worker); safe to call from a `BackgroundTask::run`.
    /// If another worker is already draining this tap it returns without
    /// double-draining - that worker sees the same data. `f` is not called
    /// when no whole frame is buffered.
    ///
    /// The producer pushes a frame's channels one at a time, so a drain
    /// running concurrently can catch it mid-frame. Any trailing partial
    /// frame is held in `scratch` and reassembled on the next drain rather
    /// than handed to `f` split - a consumer deinterleaving with
    /// `chunks_exact(channels)` never sees a channel-swapped chunk.
    pub fn drain_with(&self, mut f: impl FnMut(&[S])) {
        let Ok(mut scratch) = self.scratch.try_lock() else {
            return;
        };
        // Do NOT clear: `scratch` may hold a partial frame carried from a
        // previous drain. Append after it so the frame reassembles.
        while let Some(sample) = self.ring.pop() {
            scratch.push(sample);
        }
        // Hand off only whole frames; keep any trailing partial for the
        // next call. `drain` shifts the (sub-channel-count) remainder to
        // the front and preserves the buffer's capacity for reuse.
        let whole = scratch.len() - scratch.len() % self.channels;
        if whole > 0 {
            f(&scratch[..whole]);
            scratch.drain(..whole);
        }
    }

    /// Spawn a dedicated thread that drains this tap sequentially. The
    /// thread parks until [`Self::push_frames`] unparks it, then hands
    /// every buffered frame to `on_drain` as one interleaved slice, in
    /// order. Consumer state lives inside the closure - a single owner,
    /// so no lock. The returned [`StreamWorker`] joins the thread on drop.
    ///
    /// This is the dedicated-thread alternative to draining on the shared
    /// task pool: pick it when the consumer runs continuously and would
    /// otherwise contend with unrelated pool work. It spends one thread
    /// per worker, so unlike the pool it does not bound thread growth.
    ///
    /// Attach **at most one** worker per tap: the per-sample ring has a
    /// single consumer, and only one thread can be registered for the
    /// wake-on-push. Drain a tap with either a `StreamWorker` or the shared
    /// pool, not both.
    ///
    /// # Panics
    ///
    /// Panics if a worker is already attached to this tap, or if the OS
    /// refuses to spawn the worker thread.
    #[must_use]
    pub fn spawn_worker(
        self: Arc<Self>,
        name: &str,
        mut on_drain: impl FnMut(&[S]) + Send + 'static,
    ) -> StreamWorker {
        // Fail loud on a second attach rather than silently registering no
        // waker (which would fall back to the park timeout and race the
        // first worker on the drain lock). Checked before spawning so a
        // rejected call leaks no thread.
        assert!(
            self.waker.get().is_none(),
            "AudioTap already has a StreamWorker; attach at most one worker per tap",
        );
        let tap = Arc::clone(&self);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_thread = Arc::clone(&shutdown);
        let handle = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                while !shutdown_thread.load(Ordering::Acquire) {
                    tap.drain_with(&mut on_drain);
                    thread::park_timeout(WORKER_PARK_TIMEOUT);
                }
            })
            .expect("spawn truce stream worker");
        // Publish the thread so `push_frames` can unpark it. Set from the
        // spawner (not the worker itself) so it is in place before the
        // first push; a push before it lands just relies on the timeout.
        let _ = self.waker.set(handle.thread().clone());
        StreamWorker {
            shutdown,
            thread: handle.thread().clone(),
            handle: Some(handle),
        }
    }
}

/// Handle to a dedicated [`AudioTap`] consumer thread spawned by
/// [`AudioTap::spawn_worker`]. Dropping it asks the thread to stop and
/// joins it, so the worker's lifetime is tied to whatever owns the handle
/// (typically a `#[skip]` field alongside the tap).
pub struct StreamWorker {
    shutdown: Arc<AtomicBool>,
    thread: Thread,
    handle: Option<JoinHandle<()>>,
}

impl Drop for StreamWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        // Wake the thread out of its park so it sees the flag now rather
        // than after the timeout.
        self.thread.unpark();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_frames() {
        let tap = AudioTap::<f32>::new(16, 2);
        tap.push_frames(&[1.0, 2.0, 3.0, 4.0]); // two stereo frames
        let mut got = Vec::new();
        tap.drain_with(|chunk| got.extend_from_slice(chunk));
        assert_eq!(got, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn drop_on_full_stays_frame_aligned() {
        // Capacity 2 frames (4 samples). Push 4 frames; the last 2 drop.
        let tap = AudioTap::<i32>::new(2, 2);
        tap.push_frames(&[1, 1, 2, 2, 3, 3, 4, 4]);
        let mut got = Vec::new();
        tap.drain_with(|chunk| got.extend_from_slice(chunk));
        // Whatever survived is a whole number of frames (even length),
        // and never a split frame.
        assert_eq!(got.len() % 2, 0);
        assert_eq!(got, vec![1, 1, 2, 2]);
    }

    #[test]
    fn drain_of_empty_tap_does_not_call_f() {
        let tap = AudioTap::<f32>::new(4, 1);
        let mut called = false;
        tap.drain_with(|_| called = true);
        assert!(!called, "no callback when nothing is buffered");
    }

    #[test]
    fn stream_worker_drains_in_order() {
        use std::sync::mpsc;

        let tap = Arc::new(AudioTap::<i32>::new(64, 2));
        let (tx, rx) = mpsc::channel();
        let worker = tap
            .clone()
            .spawn_worker("test-stream-worker", move |chunk| {
                for &sample in chunk {
                    let _ = tx.send(sample);
                }
            });
        tap.push_frames(&[1, 2, 3, 4]);

        // The worker drains FIFO, so the samples arrive in push order.
        let mut got = Vec::new();
        for _ in 0..4 {
            got.push(
                rx.recv_timeout(Duration::from_secs(5))
                    .expect("worker drained the pushed frames"),
            );
        }
        assert_eq!(got, vec![1, 2, 3, 4]);
        drop(worker);
    }

    #[test]
    fn default_is_a_usable_stereo_tap() {
        let tap = AudioTap::<f32>::default();
        assert_eq!(tap.channels(), DEFAULT_TAP_CHANNELS);
        tap.push_frames(&[1.0, 2.0]);
        let mut got = Vec::new();
        tap.drain_with(|chunk| got.extend_from_slice(chunk));
        assert_eq!(got, vec![1.0, 2.0]);
    }

    /// The producer pushes a frame's channels one at a time, so a drain
    /// running concurrently routinely lands between them. Every chunk
    /// handed to `f` must still be a whole number of frames - otherwise a
    /// consumer deinterleaving with `chunks_exact(2)` gets a channel-swapped
    /// chunk. Sizing the ring for the whole run rules out drops, so the
    /// reassembled stream must also be the exact FIFO sequence.
    ///
    /// Several independent rounds: each producer/consumer start is its own
    /// race, so a regressed drain (handing off partial frames) is caught
    /// with high probability, while the correct one always passes.
    ///
    /// Skipped under Miri: 400k pushes across two threads per round are
    /// hour-scale in the interpreter, and the mid-frame race it soaks for
    /// depends on real thread timing Miri's scheduler doesn't reproduce.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    #[cfg_attr(
        miri,
        ignore = "concurrency soak - too slow under Miri, no timing repro"
    )]
    #[test]
    fn concurrent_drain_hands_out_whole_frames() {
        use std::time::Instant;

        let frames = 100_000usize;
        for _round in 0..4 {
            let tap = Arc::new(AudioTap::<i32>::new(frames, 2));

            let producer = {
                let tap = Arc::clone(&tap);
                thread::spawn(move || {
                    // Frame i is [2i, 2i+1]: L even, R odd, values consecutive
                    // across the stream, so any split or swap is visible.
                    for i in 0..frames as i32 {
                        tap.push_frames(&[2 * i, 2 * i + 1]);
                    }
                })
            };

            let mut got: Vec<i32> = Vec::with_capacity(frames * 2);
            let start = Instant::now();
            while got.len() < frames * 2 {
                tap.drain_with(|chunk| {
                    assert_eq!(
                        chunk.len() % 2,
                        0,
                        "drain handed a partial frame (len {})",
                        chunk.len()
                    );
                    got.extend_from_slice(chunk);
                });
                assert!(start.elapsed() < Duration::from_secs(30), "drain stalled");
            }
            producer.join().unwrap();

            // No drops (ring sized for the run) means strict FIFO: sample j is j.
            assert_eq!(got.len(), frames * 2);
            for (j, &v) in got.iter().enumerate() {
                assert_eq!(v, j as i32, "sample {j} out of place - frame misaligned");
            }
        }
    }

    #[test]
    #[should_panic(expected = "at most one worker per tap")]
    fn second_worker_panics() {
        let tap = Arc::new(AudioTap::<i32>::new(16, 2));
        let _first = tap.clone().spawn_worker("first", |_| {});
        // A second worker on the same tap is misuse and must fail loudly.
        let _second = tap.clone().spawn_worker("second", |_| {});
    }
}
