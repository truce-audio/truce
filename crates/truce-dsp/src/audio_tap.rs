//! Canonical DSP→editor ring buffer for passing audio-derived data
//! (oscilloscope, spectrum, meter history, waveform, visualizer) from
//! the audio thread to the UI / worker thread.
//!
//! The audio thread owns an [`AudioTapProducer`] and the editor /
//! worker thread owns an [`AudioTapConsumer`]. The producer never
//! blocks, never allocates, and never fails: if the consumer has
//! fallen more than one ring's worth behind, the oldest samples are
//! silently dropped.
//!
//! For typical visualization workloads (a 1–2 second ring drained at
//! 60 Hz against a 44.1 kHz / 48 kHz source) drops never happen in
//! practice. If they do, the worst-case artifact is a brief visual
//! glitch — the kind of thing human eyes don't notice.
//!
//! Per-sample integrity is preserved via `AtomicU32` storage of the
//! `f32` bit pattern, so the consumer never sees torn individual
//! samples even under an overwrite. The ring does not attempt to
//! preserve chunk alignment across a wrap, so a consumer that pulls
//! one DSP block at a time should size the ring to at least
//! `dsp_block_size * K` frames where K is the expected consumer-to-
//! producer drain ratio.
//!
//! Example:
//!
//! ```
//! use truce_dsp::{audio_tap, AudioTapProducer, AudioTapConsumer};
//!
//! let (mut tx, mut rx) = audio_tap(4096, 2); // 4096-frame stereo ring
//!
//! // audio thread:
//! let block = [0.0_f32, 0.0, 0.1, 0.1, 0.2, 0.2];
//! let _dropped = tx.push_block(&block, 2);
//!
//! // editor / worker thread:
//! let mut scratch = [0.0_f32; 256];
//! let frames = rx.read(&mut scratch, 128);
//! assert_eq!(frames, 3);
//! ```

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

struct AudioTapShared {
    /// Sample storage. Length is fixed at construction to `cap` samples
    /// (not frames). Each slot holds the bit pattern of one `f32`.
    data: Box<[AtomicU32]>,
    /// Total sample count capacity (= `capacity_frames × channels`).
    cap: usize,
    /// Interleaved channel count. Baked in at construction so the
    /// consumer can report frame counts without further metadata.
    channels: u16,
    /// Monotonic count of samples pushed by the producer.
    ///
    /// Stored with `Release` on write so consumers observing it via
    /// `Acquire` also see every sample written below this point.
    write: AtomicUsize,
    /// Monotonic count of samples pulled by the consumer. The producer
    /// reads this with `Relaxed` solely to compute drop diagnostics.
    read: AtomicUsize,
}

/// Producer half of the ring. Owned by the audio thread.
///
/// Construction, cloning, and destruction of this value may allocate;
/// `push_block` never does.
pub struct AudioTapProducer {
    shared: Arc<AudioTapShared>,
}

/// Consumer half of the ring. Owned by the editor / UI / worker thread.
pub struct AudioTapConsumer {
    shared: Arc<AudioTapShared>,
}

/// Construct a new `capacity_frames × channels` lock-free SPSC ring
/// and return its producer / consumer halves.
///
/// `capacity_frames` is the number of interleaved frames the ring can
/// hold; the underlying storage is `capacity_frames × channels` samples.
/// `channels` must be non-zero.
///
/// Panics if `channels == 0` or if `capacity_frames == 0`.
pub fn audio_tap(capacity_frames: usize, channels: u16) -> (AudioTapProducer, AudioTapConsumer) {
    assert!(channels > 0, "audio_tap: channels must be > 0");
    assert!(
        capacity_frames > 0,
        "audio_tap: capacity_frames must be > 0"
    );

    let cap = capacity_frames * channels as usize;
    let mut data = Vec::with_capacity(cap);
    for _ in 0..cap {
        data.push(AtomicU32::new(0));
    }
    let shared = Arc::new(AudioTapShared {
        data: data.into_boxed_slice(),
        cap,
        channels,
        write: AtomicUsize::new(0),
        read: AtomicUsize::new(0),
    });
    (
        AudioTapProducer {
            shared: Arc::clone(&shared),
        },
        AudioTapConsumer { shared },
    )
}

impl AudioTapProducer {
    /// Push a block of interleaved samples onto the ring.
    ///
    /// Never blocks and never allocates. Safe to call from the audio
    /// thread. Samples beyond the ring's capacity overwrite the oldest
    /// still-queued samples.
    ///
    /// `channels` is accepted for a defensive shape check against the
    /// ring's configured channel count; mismatched values are ignored
    /// here but indicate a programming error at the caller.
    ///
    /// Returns the number of samples that were overwritten (dropped
    /// from the consumer's perspective). Callers can feed this into
    /// a meter or log it; it is not needed for correctness.
    pub fn push_block(&mut self, samples: &[f32], channels: u16) -> usize {
        debug_assert_eq!(
            channels, self.shared.channels,
            "push_block channel count mismatch",
        );

        let cap = self.shared.cap;
        let w = self.shared.write.load(Ordering::Relaxed);
        let r = self.shared.read.load(Ordering::Relaxed);

        // Number of samples this push will force the consumer to drop.
        let occupied = w.saturating_sub(r);
        let after = occupied + samples.len();
        let dropped = after.saturating_sub(cap);

        for (i, &s) in samples.iter().enumerate() {
            let idx = (w + i) % cap;
            self.shared.data[idx].store(s.to_bits(), Ordering::Relaxed);
        }

        // Release so the consumer's Acquire load of `write` sees the
        // sample stores above.
        self.shared
            .write
            .store(w + samples.len(), Ordering::Release);

        dropped
    }

    /// Channel count baked into this ring.
    pub fn channels(&self) -> u16 {
        self.shared.channels
    }

    /// Capacity in frames.
    pub fn capacity_frames(&self) -> usize {
        self.shared.cap / self.shared.channels as usize
    }
}

impl AudioTapConsumer {
    /// Channel count baked into this ring.
    pub fn channels(&self) -> u16 {
        self.shared.channels
    }

    /// Capacity in frames.
    pub fn capacity_frames(&self) -> usize {
        self.shared.cap / self.shared.channels as usize
    }

    /// Approximate number of buffered frames available to read.
    ///
    /// The result is clamped to the ring's capacity so it remains
    /// meaningful after an overwrite. Returns 0 if the ring is empty.
    pub fn available(&self) -> usize {
        let w = self.shared.write.load(Ordering::Acquire);
        let r = self.shared.read.load(Ordering::Relaxed);
        let avail = w.saturating_sub(r).min(self.shared.cap);
        avail / self.shared.channels as usize
    }

    /// Drain up to `max_frames` frames into `dest` (interleaved).
    ///
    /// `dest.len()` must be at least `max_frames × channels` to hold
    /// the full result; if smaller, the read is truncated to the
    /// number of whole frames `dest` can hold.
    ///
    /// Returns the number of frames written to `dest`.
    pub fn read(&mut self, dest: &mut [f32], max_frames: usize) -> usize {
        let cap = self.shared.cap;
        let channels = self.shared.channels as usize;

        let w = self.shared.write.load(Ordering::Acquire);
        let mut r = self.shared.read.load(Ordering::Relaxed);

        // If the producer has lapped us by more than one ring, skip
        // ahead to the oldest still-valid sample. Samples between the
        // old `r` and the new one have been overwritten and are lost.
        let total_avail = w.saturating_sub(r);
        if total_avail > cap {
            r = w - cap;
        }

        let frames_avail = (w - r) / channels;
        let frames = frames_avail.min(max_frames).min(dest.len() / channels);
        let samples = frames * channels;

        for i in 0..samples {
            let idx = (r + i) % cap;
            let bits = self.shared.data[idx].load(Ordering::Relaxed);
            dest[i] = f32::from_bits(bits);
        }

        self.shared.read.store(r + samples, Ordering::Relaxed);
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_push_read_stereo() {
        let (mut tx, mut rx) = audio_tap(8, 2);
        assert_eq!(rx.channels(), 2);
        assert_eq!(tx.channels(), 2);
        assert_eq!(rx.capacity_frames(), 8);

        let block = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6]; // 3 stereo frames
        assert_eq!(tx.push_block(&block, 2), 0);
        assert_eq!(rx.available(), 3);

        let mut dest = [0.0_f32; 10];
        assert_eq!(rx.read(&mut dest, 10), 3);
        assert_eq!(&dest[..6], &block);
        assert_eq!(rx.available(), 0);
    }

    #[test]
    fn read_respects_max_frames() {
        let (mut tx, mut rx) = audio_tap(8, 1);
        let block: Vec<f32> = (0..5).map(|i| i as f32).collect();
        tx.push_block(&block, 1);

        let mut dest = [0.0_f32; 10];
        assert_eq!(rx.read(&mut dest, 2), 2);
        assert_eq!(&dest[..2], &[0.0, 1.0]);
        assert_eq!(rx.read(&mut dest, 10), 3);
        assert_eq!(&dest[..3], &[2.0, 3.0, 4.0]);
    }

    #[test]
    fn read_truncated_to_dest_len() {
        let (mut tx, mut rx) = audio_tap(8, 2);
        let block = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]; // 4 frames
        tx.push_block(&block, 2);

        // dest can only hold one stereo frame.
        let mut dest = [0.0_f32; 2];
        assert_eq!(rx.read(&mut dest, 10), 1);
        assert_eq!(dest, [1.0, 2.0]);
    }

    #[test]
    fn drop_on_full_reports_and_preserves_latest() {
        // capacity = 4 frames = 4 mono samples.
        let (mut tx, mut rx) = audio_tap(4, 1);

        // Fill exactly.
        assert_eq!(tx.push_block(&[1.0, 2.0, 3.0, 4.0], 1), 0);
        // Overflow by 3 — samples [1, 2, 3] are overwritten.
        assert_eq!(tx.push_block(&[5.0, 6.0, 7.0], 1), 3);

        let mut dest = [0.0_f32; 8];
        let frames = rx.read(&mut dest, 8);
        assert_eq!(frames, 4);
        // Latest 4 samples, in order: 4, 5, 6, 7.
        assert_eq!(&dest[..4], &[4.0, 5.0, 6.0, 7.0]);
    }

    #[test]
    fn wraps_across_many_pushes() {
        let (mut tx, mut rx) = audio_tap(4, 1);
        let mut drain = [0.0_f32; 4];
        for i in 0..100u32 {
            let s = i as f32;
            tx.push_block(&[s], 1);
            assert_eq!(rx.read(&mut drain, 4), 1);
            assert_eq!(drain[0], s);
        }
    }

    #[test]
    fn producer_consumer_across_threads() {
        use std::thread;

        let (mut tx, mut rx) = audio_tap(1024, 1);

        let handle = thread::spawn(move || {
            for i in 0..1000u32 {
                tx.push_block(&[i as f32], 1);
            }
        });

        let mut received = Vec::<f32>::new();
        let mut scratch = [0.0_f32; 128];
        while received.len() < 1000 {
            let n = rx.read(&mut scratch, 128);
            received.extend_from_slice(&scratch[..n]);
            if n == 0 {
                thread::yield_now();
            }
        }
        handle.join().unwrap();

        assert_eq!(received.len(), 1000);
        for (i, &v) in received.iter().enumerate() {
            assert_eq!(v, i as f32);
        }
    }

    #[test]
    fn available_clamps_to_capacity_after_overwrite() {
        let (mut tx, rx) = audio_tap(4, 1);
        // Push 8 samples into a 4-sample ring — 4 drops.
        tx.push_block(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0], 1);
        assert_eq!(rx.available(), 4);
    }
}
