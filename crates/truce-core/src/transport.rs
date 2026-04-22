//! Shared transport slot: audio-thread writer → editor-thread reader.
//!
//! Each format wrapper owns a [`TransportSlot`] and writes it at the
//! top of every process block. The editor closure on [`EditorContext`]
//! reads from the same slot, giving UI code access to host tempo /
//! play state / beat position without a format-specific callback.
//!
//! The audio-thread side uses `try_lock` and never blocks: if the UI
//! thread happens to be reading at the instant of write, that block's
//! update is skipped. At block-rate (typically every few ms) the next
//! successful write is effectively-instantaneous from a visualizer's
//! point of view.

use std::sync::{Arc, Mutex};

use crate::events::TransportInfo;

/// Lock-free-ish slot carrying the most recently-reported host
/// [`TransportInfo`]. Held by format wrappers; exposed to editors via
/// `EditorContext::transport`.
///
/// The audio thread calls [`TransportSlot::write`] each block; readers
/// (UI thread, worker threads) call [`TransportSlot::read`].
///
/// `populated` is set true on the first successful write so consumers
/// can distinguish "host has not reported transport yet" from
/// "host reported a default / stopped transport".
pub struct TransportSlot {
    inner: Mutex<Option<TransportInfo>>,
}

impl TransportSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(None),
        })
    }

    /// Realtime-safe write. Called on the audio thread at the top of
    /// each process block. Skips the update silently if the UI thread
    /// currently holds the read lock — the next block will write again.
    pub fn write(&self, info: &TransportInfo) {
        if let Ok(mut g) = self.inner.try_lock() {
            *g = Some(info.clone());
        }
    }

    /// Read the most recently-reported transport info, or `None` if
    /// no host block has reported one yet.
    pub fn read(&self) -> Option<TransportInfo> {
        // `try_lock` here too so the editor never blocks the audio
        // thread's next write — if we collide we simply return None
        // (caller treats this the same as "no transport yet").
        self.inner.try_lock().ok().and_then(|g| g.clone())
    }
}

impl Default for TransportSlot {
    fn default() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }
}
