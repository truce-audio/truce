//! Lock-free handoff of decoded events from the audio thread to the
//! editor.
//!
//! The audio thread pushes [`LogEntry`] values in `process()`; the
//! editor drains them each frame in `view()`. `LogEntry` is `Copy`
//! with an inline `SysEx` buffer so a push never allocates - the audio
//! thread must stay real-time safe even when the inspector is staring
//! at a flood of `SysEx`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_queue::ArrayQueue;
use truce_core::events::EventBody;

/// Bytes of a `SysEx` payload kept inline for display. Longer payloads
/// are truncated (the full length is still reported, so the UI can
/// show a "… +N more" marker). 24 keeps `LogEntry` small while showing
/// enough to recognize a header (manufacturer id + a few data bytes).
pub const SYSEX_INLINE: usize = 24;

/// Ring capacity. Generous so a busy MIDI stream doesn't drop between
/// 60 fps drains; overflow overwrites the oldest entry.
const RING_CAP: usize = 4096;

/// One captured event, ready for interpretation on the GUI thread.
#[derive(Clone, Copy, Debug)]
pub struct LogEntry {
    /// Monotonic capture sequence number (stable ordering + display).
    pub seq: u64,
    /// Sample offset within the block the event arrived in.
    pub sample_offset: u32,
    pub body: EventBody,
    /// First [`SYSEX_INLINE`] payload bytes (only meaningful for
    /// `EventBody::SysEx`).
    pub sysex: [u8; SYSEX_INLINE],
    /// Full `SysEx` payload length, which may exceed what's inlined.
    pub sysex_len: u32,
}

/// Shared audio-thread → editor event channel.
pub struct EventRing {
    queue: ArrayQueue<LogEntry>,
    seq: AtomicU64,
    /// Count of entries overwritten before the editor could drain them.
    dropped: AtomicU64,
}

impl EventRing {
    #[must_use]
    pub fn new() -> Self {
        Self {
            queue: ArrayQueue::new(RING_CAP),
            seq: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// Push a decoded event. Real-time safe: fixed-size copy, no
    /// allocation, lock-free. `sysex` is the resolved payload (empty
    /// for non-SysEx events); only the first [`SYSEX_INLINE`] bytes are
    /// retained.
    pub fn push(&self, sample_offset: u32, body: EventBody, sysex: &[u8]) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut buf = [0u8; SYSEX_INLINE];
        let n = sysex.len().min(SYSEX_INLINE);
        buf[..n].copy_from_slice(&sysex[..n]);
        let entry = LogEntry {
            seq,
            sample_offset,
            body,
            sysex: buf,
            // Payload lengths are bounded by the host's SysEx limits,
            // far below u32::MAX; saturate defensively anyway.
            sysex_len: u32::try_from(sysex.len()).unwrap_or(u32::MAX),
        };
        // `force_push` overwrites the oldest entry when full and returns
        // it - that's a dropped event from the editor's point of view.
        if self.queue.force_push(entry).is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Drain all pending entries into `out` (oldest first), capping
    /// `out` at `max` by discarding from the front. Called on the GUI
    /// thread.
    pub fn drain_into(&self, out: &mut VecDeque<LogEntry>, max: usize) {
        while let Some(entry) = self.queue.pop() {
            out.push_back(entry);
        }
        while out.len() > max {
            out.pop_front();
        }
    }

    /// Total events overwritten before being drained.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Drop every queued entry and reset the dropped counter (the
    /// editor's "Clear" action).
    pub fn clear(&self) {
        while self.queue.pop().is_some() {}
        self.dropped.store(0, Ordering::Relaxed);
    }
}

impl Default for EventRing {
    fn default() -> Self {
        Self::new()
    }
}
