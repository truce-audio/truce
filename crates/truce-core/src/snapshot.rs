//! Lock-free (for the audio thread) publish slot for a plugin's custom
//! state, so the host can serialize it without taking the plugin lock.
//!
//! A plugin opts in by overriding `PluginLogic::snapshot_into`. The
//! shell then calls it on the audio thread after each process block and
//! publishes the bytes here; the wrapper's `save_state` reads them off
//! this slot instead of locking the plugin and calling `save_state()`.
//!
//! The audio thread publishes through `try_lock` so it never blocks -
//! on contention (a reader mid-clone) it skips a block's publish and
//! leaves the previous snapshot in place, at most one block stale. The
//! host / editor readers take a blocking `lock`, contending only with
//! each other; because the producer never blocks, there is no priority
//! inversion on the audio side.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Shared handle to the published custom-state bytes. Held by the shell
/// (producer, audio thread) and the format wrapper (consumer, host /
/// GUI thread), both cloned from one `Arc`.
pub struct SnapshotSlot {
    bytes: Mutex<Vec<u8>>,
    /// Set once the plugin publishes a snapshot for the first time.
    /// Until then a reader falls back to the locked `save_state()` path.
    supported: AtomicBool,
}

impl SnapshotSlot {
    /// A fresh, empty slot. Nothing is published yet, so [`Self::read`]
    /// returns `None` until the first [`Self::publish`].
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            bytes: Mutex::new(Vec::new()),
            supported: AtomicBool::new(false),
        })
    }

    /// Audio thread: publish the current snapshot. `write` receives the
    /// reusable buffer (it should `clear()` then fill it) and returns
    /// whether a snapshot exists. Never blocks - on lock contention with
    /// a reader the publish is skipped and the previous snapshot stands.
    pub fn publish(&self, write: impl FnOnce(&mut Vec<u8>) -> bool) {
        if let Ok(mut guard) = self.bytes.try_lock()
            && write(&mut guard)
        {
            self.supported.store(true, Ordering::Release);
        }
    }

    /// Whether the plugin has ever published a snapshot. Cheap atomic
    /// read (no lock). Once true it stays true: a plugin's decision to
    /// publish snapshots is a static capability, latched on the first
    /// successful publish.
    #[must_use]
    pub fn is_supported(&self) -> bool {
        self.supported.load(Ordering::Acquire)
    }

    /// Host / GUI thread: the latest published snapshot, or `None` when
    /// the plugin doesn't publish snapshots (nothing ever written) so
    /// the caller can fall back to the locked `save_state()` path.
    #[must_use]
    pub fn read(&self) -> Option<Vec<u8>> {
        if !self.supported.load(Ordering::Acquire) {
            return None;
        }
        Some(
            self.bytes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::SnapshotSlot;

    #[test]
    fn read_is_none_until_first_publish() {
        let slot = SnapshotSlot::new();
        assert!(slot.read().is_none());
        slot.publish(|buf| {
            buf.clear();
            buf.extend_from_slice(&[1, 2, 3]);
            true
        });
        assert_eq!(slot.read(), Some(vec![1, 2, 3]));
    }

    #[test]
    fn unsupported_publish_never_marks_supported() {
        let slot = SnapshotSlot::new();
        slot.publish(|_| false);
        assert!(slot.read().is_none());
    }

    #[test]
    fn is_supported_latches_on_first_true_publish() {
        let slot = SnapshotSlot::new();
        assert!(!slot.is_supported());
        slot.publish(|_| false);
        assert!(!slot.is_supported());
        slot.publish(|buf| {
            buf.clear();
            buf.push(1);
            true
        });
        assert!(slot.is_supported());
        // A later empty (but true) publish keeps it supported.
        slot.publish(|buf| {
            buf.clear();
            true
        });
        assert!(slot.is_supported());
        assert_eq!(slot.read(), Some(vec![]));
    }

    #[test]
    fn publish_overwrites_and_reuses_capacity() {
        let slot = SnapshotSlot::new();
        slot.publish(|buf| {
            buf.clear();
            buf.extend_from_slice(&[9; 64]);
            true
        });
        slot.publish(|buf| {
            buf.clear();
            buf.extend_from_slice(&[7, 7]);
            true
        });
        assert_eq!(slot.read(), Some(vec![7, 7]));
    }
}
