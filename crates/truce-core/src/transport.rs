//! Shared transport slot: audio-thread writer → editor-thread reader.
//!
//! Each format wrapper owns a [`TransportSlot`] and writes it at the
//! top of every process block. The editor closure on
//! [`crate::editor::PluginContext`] reads from the same slot, giving
//! UI code access to host tempo / play state / beat position without
//! a format-specific callback.
//!
//! The implementation is a single-writer seqlock: the audio thread's
//! write path takes no locks and always lands; UI readers retry on
//! collision (the critical section is a single `TransportInfo` copy,
//! a few hundred nanoseconds at worst). A mutex-based design would
//! either drop audio-thread writes on contention (`try_lock`) or
//! introduce audio-thread blocking, both of which would let the
//! visualizer drift from real transport state.

use std::cell::UnsafeCell;
use std::ptr::{read_volatile, write_volatile};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::events::TransportInfo;

/// Single-writer / multi-reader transport slot. Held by format
/// wrappers; exposed to editors via `PluginContext::transport`.
///
/// The audio thread calls [`TransportSlot::write`] each block; readers
/// (UI thread, worker threads) call [`TransportSlot::read`].
///
/// The seq counter is 0 before any write, then alternates odd ("write
/// in progress") / even ("write done") as `write` runs. `read` reads
/// the counter, copies the data, re-reads the counter, and retries if
/// either snapshot landed on a write-in-progress or the two reads
/// disagree.
pub struct TransportSlot {
    /// Sequence counter. 0 = uninitialized; even, non-zero = quiescent
    /// after Nth write; odd = writer mid-update.
    seq: AtomicU64,
    /// Last-written transport. Written only by `write` (single writer
    /// assumption - the audio-thread callback). Read under seqlock by
    /// any number of `read`-calling threads.
    data: UnsafeCell<TransportInfo>,
}

// SAFETY: writes are guarded by the seq counter so concurrent reads
// detect torn states and retry; readers only observe the data when
// seq is even and unchanged across the read.
unsafe impl Sync for TransportSlot {}
unsafe impl Send for TransportSlot {}

impl TransportSlot {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            seq: AtomicU64::new(0),
            data: UnsafeCell::new(TransportInfo::default()),
        })
    }

    /// Realtime-safe write. Called on the audio thread at the top of
    /// each process block. Wait-free - never blocks, never drops.
    ///
    /// Single-writer: this assumes only one thread (the host's audio
    /// callback) ever calls `write` on a given slot. Format wrappers
    /// uphold this by giving each plugin instance its own slot.
    pub fn write(&self, info: &TransportInfo) {
        // The previous seq is even (or 0). Bump to the next odd value
        // to mark "write in progress", do the write, then bump to the
        // next even value to publish.
        let s = self.seq.load(Ordering::Relaxed);
        // First store: just signals "write in progress" to readers.
        // The data write that follows is published by the *second*
        // store's Release - readers acquire on that one. This first
        // store's only job is to flip parity, so Relaxed is enough.
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        // SAFETY: single-writer invariant means no other thread writes
        // `data` concurrently. Readers detect mid-update via the odd
        // seq value, but Rust's memory model treats a non-atomic write
        // overlapping a non-atomic read as a data race regardless of
        // observable outcome (the read result would be discarded by
        // the seq re-check anyway). `write_volatile` keeps the
        // compiler from breaking the write into reorderable chunks
        // and is the standard "least-bad" mitigation Rust offers
        // until a true `Atomic<T: Copy>` lands. The seqlock crate
        // applies the same pattern with the same caveat.
        unsafe {
            write_volatile(self.data.get(), *info);
        }
        // Release pairs with `read`'s Acquire load - makes the data
        // write above visible to any reader that observes this
        // updated even value.
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Read the most recently-reported transport info, or `None` if
    /// no host block has reported one yet.
    ///
    /// Bounded retry: each iteration is an Acquire-ordered counter
    /// load and a `TransportInfo` copy. In the worst observable case
    /// (writer scheduled out mid-update) the reader spins until the
    /// writer resumes - typically nanoseconds; with thread preemption
    /// in pathological scheduling, microseconds. We cap at 8 attempts
    /// and bail out with `None` rather than potentially spin forever
    /// - the editor next frame will read again.
    pub fn read(&self) -> Option<TransportInfo> {
        for _ in 0..8 {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 == 0 {
                return None;
            }
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            // SAFETY: even seq means no writer is mid-update at the
            // load above. The post-copy seq re-read confirms no
            // writer started during the copy; if that fails we
            // discard and retry rather than returning torn state.
            // `read_volatile` is the same mitigation `write` uses on
            // the producer side - see that doc-comment for the data-
            // race rationale.
            let snapshot = unsafe { read_volatile(self.data.get()) };
            let s2 = self.seq.load(Ordering::Acquire);
            if s1 == s2 {
                return Some(snapshot);
            }
        }
        None
    }
}
