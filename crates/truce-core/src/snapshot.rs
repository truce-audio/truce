//! Publish slots for a plugin's custom state, so the host can serialize
//! it without taking the plugin lock. Two lanes cover two size regimes:
//!
//! - **Inline lane** (`publish`): the shell calls `snapshot_into` on the
//!   audio thread after each *dirty* block and copies the bytes here
//!   through `try_lock`, so it never blocks. Right for KB-scale state (a
//!   label, a file path, a few flags). A generation token gates the copy
//!   so an unchanged block pays O(1) (see the shell's `publish_snapshot`).
//!
//! - **Off-thread lane** (`store_arc` via [`SnapshotPublisher`]): a
//!   background thread (e.g. a task-pool handler) serializes large state
//!   and swaps a whole buffer in; the audio thread is never involved. For
//!   MB-scale state (a sampler's audio, big wavetables) that must not be
//!   copied on the audio thread at all. `read` prefers this lane.
//!
//! Both reader entry points ([`SnapshotSlot::read`]) take a blocking
//! `lock`, contending only with each other and with `store_arc`; the
//! audio-thread producer (`publish`) uses `try_lock` and never blocks, so
//! there is no priority inversion on the audio side.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Capacity the inline publish buffer is pre-warmed to by
/// [`SnapshotSlot::new`], off the audio thread, so the first per-block
/// publish doesn't allocate. Small state fits without ever growing;
/// larger inline state reallocates once on the first publish, then stays
/// warm. A plugin can raise the pre-warm via `snapshot_prealloc_hint`
/// ([`SnapshotSlot::with_capacity`]). Genuinely large state should use
/// the off-thread lane ([`SnapshotPublisher`]) instead, which never
/// touches this buffer.
pub const SNAPSHOT_PREALLOC: usize = 256;

/// Shared handle to the published custom-state bytes. Held by the shell
/// (producer, audio thread) and the format wrapper (consumer, host /
/// GUI thread), both cloned from one `Arc`.
pub struct SnapshotSlot {
    /// Inline lane: the audio thread copies serialized bytes here each
    /// dirty block via `try_lock` (see [`Self::publish`]).
    bytes: Mutex<Vec<u8>>,
    /// Off-thread lane: a background thread swaps a whole pre-serialized
    /// buffer in (see [`Self::store_arc`]). `read` prefers this over
    /// `bytes`. The audio thread never touches this lock.
    swapped: Mutex<Option<Arc<Vec<u8>>>>,
    /// Set once *either* lane publishes for the first time. Until then a
    /// reader falls back to the locked `save_state()` path.
    supported: AtomicBool,
}

impl SnapshotSlot {
    /// A fresh slot with its inline buffer pre-warmed to
    /// [`SNAPSHOT_PREALLOC`]. Nothing is published yet, so [`Self::read`]
    /// returns `None` until the first publish.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Self::with_capacity(SNAPSHOT_PREALLOC)
    }

    /// A fresh slot whose inline buffer is pre-warmed to `cap` bytes.
    /// Used with a plugin's `snapshot_prealloc_hint` so a first inline
    /// publish of up-to-`cap` bytes doesn't reallocate on the audio
    /// thread.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Arc<Self> {
        let slot = Self {
            bytes: Mutex::new(Vec::with_capacity(cap)),
            swapped: Mutex::new(None),
            supported: AtomicBool::new(false),
        };
        // Warm both locks off the audio thread: some platforms' std
        // `Mutex` (macOS boxes a `pthread_mutex_t`) lazily allocate the OS
        // mutex on first lock, which would otherwise land on the first
        // `publish` from the audio thread. Locking here forces that
        // one-time init at construction instead.
        drop(slot.bytes.lock().unwrap_or_else(PoisonError::into_inner));
        drop(slot.swapped.lock().unwrap_or_else(PoisonError::into_inner));
        Arc::new(slot)
    }

    /// Audio thread: publish the current inline snapshot. The buffer is
    /// **cleared before `write` runs** (its capacity is retained, so a
    /// steady state stays allocation-free), so `write` just fills it and
    /// returns whether a snapshot exists - matching the
    /// `PluginLogic::snapshot_into` contract, where a writer that only
    /// `extend`s must not accumulate across blocks. Never blocks - on
    /// lock contention with a reader the publish is skipped and the
    /// previous snapshot stands.
    ///
    /// Returns whether the write **landed** (the lock was acquired and
    /// `write` ran). A `false` return means a reader held the lock and the
    /// publish was skipped, so the caller should not advance its
    /// last-published generation - the block must be retried.
    pub fn publish(&self, write: impl FnOnce(&mut Vec<u8>) -> bool) -> bool {
        if let Ok(mut guard) = self.bytes.try_lock() {
            guard.clear();
            if write(&mut guard) {
                self.supported.store(true, Ordering::Release);
            }
            true
        } else {
            false
        }
    }

    /// Off-thread lane: swap a whole pre-serialized snapshot in. Called
    /// from a background thread (a task-pool handler, a loader worker) -
    /// **never the audio thread** - so the bytes are never copied under a
    /// real-time deadline. Takes read preference in [`Self::read`].
    pub fn store_arc(&self, bytes: Vec<u8>) {
        *self.swapped.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(bytes));
        self.supported.store(true, Ordering::Release);
    }

    /// Whether the plugin has ever published a snapshot on either lane.
    /// Cheap atomic read (no lock). Once true it stays true: a plugin's
    /// decision to publish snapshots is a static capability, latched on
    /// the first successful publish.
    #[must_use]
    pub fn is_supported(&self) -> bool {
        self.supported.load(Ordering::Acquire)
    }

    /// Host / GUI thread: the latest published snapshot, or `None` when
    /// the plugin doesn't publish snapshots (nothing ever written) so
    /// the caller can fall back to the locked `save_state()` path. The
    /// off-thread lane wins over the inline lane - a plugin publishing
    /// large state there never touches `bytes`.
    #[must_use]
    pub fn read(&self) -> Option<Vec<u8>> {
        if !self.supported.load(Ordering::Acquire) {
            return None;
        }
        if let Some(arc) = self
            .swapped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            return Some((**arc).clone());
        }
        Some(
            self.bytes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        )
    }

    /// Just the off-thread (publisher) lane's bytes, or `None` when a
    /// background thread hasn't published there. A non-realtime save path
    /// (LV2) uses this to pick up a publisher-lane snapshot the plugin's
    /// live `save_state()` can't produce, without also reading the inline
    /// lane - which it serves live from the plugin instead, so a
    /// version-gated inline slot never makes it save stale bytes.
    #[must_use]
    pub fn read_offthread(&self) -> Option<Vec<u8>> {
        self.swapped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|arc| (**arc).clone())
    }
}

/// Plugin-facing handle for the off-thread snapshot lane. Obtain it from
/// `InitContext::snapshot_publisher()`, stash it in your DSP state, and
/// call [`Self::publish`] from a background-task handler after your
/// custom state changes (a sample load, a wavetable swap). The audio
/// thread never copies the bytes; the host reads whatever was last
/// published. Cloneable and `Send + Sync` so a task handler can hold it.
///
/// Use this **instead of** overriding `snapshot_into` - the two lanes are
/// independent and the off-thread lane takes read preference. It is the
/// recommended path for MB-scale state; `snapshot_into` is for KB-scale.
///
/// **Re-publish after a host load.** The framework can't refresh this
/// lane for you (it never holds your serialized bytes), and the wrapper's
/// post-load republish only drives the inline lane. Publish again from
/// your `state_changed` hook - which runs right after `load_state` - or
/// the host's next save returns the bytes you published *before* the
/// load, silently reverting the loaded preset.
#[derive(Clone)]
pub struct SnapshotPublisher {
    slot: Arc<SnapshotSlot>,
}

impl SnapshotPublisher {
    /// Wrap a slot handle for the plugin to publish through.
    #[must_use]
    pub fn new(slot: &Arc<SnapshotSlot>) -> Self {
        Self {
            slot: Arc::clone(slot),
        }
    }

    /// Publish a freshly serialized snapshot. Call from a background
    /// thread (never the audio thread) after your custom state changes.
    pub fn publish(&self, bytes: Vec<u8>) {
        self.slot.store_arc(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::{SNAPSHOT_PREALLOC, SnapshotPublisher, SnapshotSlot};

    #[test]
    fn buffer_is_prewarmed_so_first_publish_does_not_allocate() {
        let slot = SnapshotSlot::new();
        // A first publish that fits inside the pre-warmed capacity must
        // not grow the buffer - that is the whole point of warming it
        // off the audio thread.
        let landed = slot.publish(|buf| {
            buf.extend_from_slice(&[0xAB; SNAPSHOT_PREALLOC]);
            true
        });
        assert!(landed);
        let published = slot.read().expect("published");
        assert_eq!(published.len(), SNAPSHOT_PREALLOC);
    }

    #[test]
    fn read_is_none_until_first_publish() {
        let slot = SnapshotSlot::new();
        assert!(slot.read().is_none());
        slot.publish(|buf| {
            buf.extend_from_slice(&[1, 2, 3]);
            true
        });
        assert_eq!(slot.read(), Some(vec![1, 2, 3]));
    }

    #[test]
    fn unsupported_publish_never_marks_supported() {
        let slot = SnapshotSlot::new();
        assert!(slot.publish(|_| false)); // landed, but wrote nothing
        assert!(slot.read().is_none());
    }

    #[test]
    fn is_supported_latches_on_first_true_publish() {
        let slot = SnapshotSlot::new();
        assert!(!slot.is_supported());
        slot.publish(|_| false);
        assert!(!slot.is_supported());
        slot.publish(|buf| {
            buf.push(1);
            true
        });
        assert!(slot.is_supported());
        // A later empty (but true) publish keeps it supported. `buf`
        // arrives cleared, so returning true without writing publishes an
        // empty blob.
        slot.publish(|_| true);
        assert!(slot.is_supported());
        assert_eq!(slot.read(), Some(vec![]));
    }

    #[test]
    fn writer_that_only_appends_does_not_accumulate_across_publishes() {
        // Regression: the framework clears the buffer before each writer
        // runs, so a plugin `snapshot_into` that only `extend`s (per the
        // `PluginLogic::snapshot_into` "cleared first" contract) must not
        // append the new snapshot onto the previous one.
        let slot = SnapshotSlot::new();
        slot.publish(|buf| {
            buf.extend_from_slice(&[9; 64]);
            true
        });
        assert_eq!(slot.read(), Some(vec![9; 64]));

        slot.publish(|buf| {
            buf.extend_from_slice(&[7, 7]);
            true
        });
        assert_eq!(
            slot.read(),
            Some(vec![7, 7]),
            "second publish must replace, not append onto, the first"
        );
    }

    #[test]
    fn publish_reports_landed_vs_skipped_on_contention() {
        let slot = SnapshotSlot::new();
        // Hold the inline lock to simulate a reader mid-clone: the publish
        // can't land and reports so, leaving the previous snapshot in place.
        slot.publish(|buf| {
            buf.push(1);
            true
        });
        let held = slot.bytes.lock().unwrap();
        let landed = slot.publish(|buf| {
            buf.clear();
            buf.push(2);
            true
        });
        assert!(!landed, "publish must report a skipped (unlanded) write");
        drop(held);
        assert_eq!(slot.read(), Some(vec![1]), "the prior snapshot stands");
    }

    #[test]
    fn off_thread_lane_takes_read_preference() {
        let slot = SnapshotSlot::new();
        // Inline lane publishes first...
        slot.publish(|buf| {
            buf.extend_from_slice(&[1, 1]);
            true
        });
        assert_eq!(slot.read(), Some(vec![1, 1]));
        // ...then the off-thread lane swaps in a larger buffer, which wins.
        SnapshotPublisher::new(&slot).publish(vec![2; 4096]);
        assert_eq!(slot.read(), Some(vec![2; 4096]));
    }

    #[test]
    fn read_offthread_returns_only_the_publisher_lane() {
        let slot = SnapshotSlot::new();
        // Inline publish alone: no off-thread lane, so read_offthread is None
        // (the LV2 save path then falls back to the live `save_state()`).
        slot.publish(|buf| {
            buf.extend_from_slice(&[1, 1]);
            true
        });
        assert!(slot.read_offthread().is_none());
        // After a publisher-lane store, read_offthread returns those bytes.
        SnapshotPublisher::new(&slot).publish(vec![9; 3]);
        assert_eq!(slot.read_offthread(), Some(vec![9; 3]));
    }

    #[test]
    fn store_arc_latches_supported_without_inline_publish() {
        let slot = SnapshotSlot::new();
        assert!(!slot.is_supported());
        slot.store_arc(vec![5, 5, 5]);
        assert!(slot.is_supported());
        assert_eq!(slot.read(), Some(vec![5, 5, 5]));
    }

    #[test]
    fn with_capacity_prewarms_the_requested_size() {
        let slot = SnapshotSlot::with_capacity(8192);
        let landed = slot.publish(|buf| {
            buf.extend_from_slice(&[0xCD; 8192]);
            true
        });
        assert!(landed);
        assert_eq!(slot.read().expect("published").len(), 8192);
    }
}
