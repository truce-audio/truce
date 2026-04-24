//! Minimal transport model for standalone mode.
//!
//! A single `TransportState` value is shared between the UI thread
//! (toggles play/stop via SPACE, adjusts BPM via config) and the
//! audio thread (advances `position_beats` each block and populates
//! the `TransportInfo` handed to `plugin.process`).
//!
//! Not a DAW transport — no timeline, no loop points, no automation
//! lanes. Just enough to let plugins that care about host tempo
//! (LFOs, tempo-synced effects, arpeggiators) see a live beat grid.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use truce_core::events::TransportInfo;

/// Shared transport state. Cheap to `Arc::clone` between UI and
/// audio threads.
#[derive(Clone)]
pub struct Transport {
    inner: Arc<Inner>,
}

struct Inner {
    /// Tempo in BPM × 1000 (fixed-point for atomic storage).
    tempo_milli: AtomicU64,
    /// Position in beats × 1e6 (micro-beats) for the same reason.
    position_micro_beats: AtomicU64,
    playing: AtomicBool,
    sample_rate: AtomicU64,
}

impl Transport {
    pub fn new(bpm: f64, sample_rate: f64) -> Self {
        Self {
            inner: Arc::new(Inner {
                tempo_milli: AtomicU64::new((bpm * 1000.0) as u64),
                position_micro_beats: AtomicU64::new(0),
                playing: AtomicBool::new(false),
                sample_rate: AtomicU64::new(sample_rate as u64),
            }),
        }
    }

    pub fn set_sample_rate(&self, sr: f64) {
        self.inner.sample_rate.store(sr as u64, Ordering::Relaxed);
    }

    pub fn set_tempo(&self, bpm: f64) {
        self.inner
            .tempo_milli
            .store((bpm * 1000.0) as u64, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.inner.playing.load(Ordering::Relaxed)
    }

    pub fn toggle_playing(&self) {
        // Load, flip, store. Contention only matters if the audio
        // thread is also toggling — it doesn't.
        let cur = self.inner.playing.load(Ordering::Relaxed);
        self.inner.playing.store(!cur, Ordering::Relaxed);
    }

    pub fn tempo(&self) -> f64 {
        self.inner.tempo_milli.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Called from the audio callback. Advances `position_beats` by
    /// `num_frames` at the current tempo (iff playing) and returns
    /// a snapshot `TransportInfo` for the plugin.
    pub fn tick_audio(&self, num_frames: usize) -> TransportInfo {
        let sr = self.inner.sample_rate.load(Ordering::Relaxed) as f64;
        let bpm = self.tempo();
        let playing = self.is_playing();

        let start = self.inner.position_micro_beats.load(Ordering::Relaxed) as f64 / 1_000_000.0;

        if playing && sr > 0.0 {
            let seconds = num_frames as f64 / sr;
            let delta_beats = seconds * (bpm / 60.0);
            let new_beats = start + delta_beats;
            self.inner
                .position_micro_beats
                .store((new_beats * 1_000_000.0) as u64, Ordering::Relaxed);
        }

        self.info(start, bpm, sr, playing)
    }

    /// Called from the UI thread (via `EditorContext::transport`).
    /// Non-mutating — just reads the current position.
    pub fn snapshot(&self) -> TransportInfo {
        let sr = self.inner.sample_rate.load(Ordering::Relaxed) as f64;
        let bpm = self.tempo();
        let playing = self.is_playing();
        let position = self.inner.position_micro_beats.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        self.info(position, bpm, sr, playing)
    }

    fn info(&self, position_beats: f64, bpm: f64, sr: f64, playing: bool) -> TransportInfo {
        TransportInfo {
            playing,
            recording: false,
            tempo: bpm,
            time_sig_num: 4,
            time_sig_den: 4,
            position_samples: if bpm > 0.0 {
                (position_beats * 60.0 / bpm * sr) as i64
            } else {
                0
            },
            position_seconds: if bpm > 0.0 {
                position_beats * 60.0 / bpm
            } else {
                0.0
            },
            position_beats,
            bar_start_beats: 0.0,
            loop_active: false,
            loop_start_beats: 0.0,
            loop_end_beats: 0.0,
        }
    }
}
