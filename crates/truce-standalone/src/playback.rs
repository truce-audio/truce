//! `.wav` file → plugin input bus, gated on the `playback` feature.
//!
//! Decodes a WAV at startup, adapts it to the device sample rate +
//! channel count once, then sums the result into the audio
//! callback's per-channel buffers each block. One-shot — the
//! cursor saturates at the end of the file and subsequent calls
//! contribute nothing.
//!
//! Mic input (when enabled) and file playback both sum into the
//! same input bus, matching the CLI matrix in `cli.rs` /
//! `--help`.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

/// Pre-decoded WAV at the device's sample rate and channel count.
/// `Send + Sync` (just owns a `Vec<f32>` and an atomic) so it can
/// be cloned-by-`Arc` into the audio worker.
pub struct PlaybackSource {
    /// Interleaved samples, `channels` per frame.
    samples: Vec<f32>,
    channels: usize,
    total_frames: usize,
    /// Number of frames consumed so far. Saturates at
    /// `total_frames`. Atomic so the audio callback can advance
    /// it without holding any lock.
    cursor: AtomicUsize,
}

impl PlaybackSource {
    /// Decode `path`, adapt to `target_sr` / `target_channels`.
    /// Errors out only on unreadable / unparseable files; channel
    /// and SR mismatches are handled with an `eprintln!` warning
    /// and a documented resolution.
    pub fn from_wav(path: &Path, target_sr: f64, target_channels: usize) -> Result<Self, String> {
        let mut reader = hound::WavReader::open(path)
            .map_err(|e| format!("could not open '{}': {e}", path.display()))?;
        let spec = reader.spec();

        // Decode all samples to f32 in [-1.0, 1.0].
        let raw: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
            (hound::SampleFormat::Int, 16) => reader
                .samples::<i16>()
                .map(|s| s.map(|v| v as f32 / (i16::MAX as f32 + 1.0)))
                .collect::<Result<_, _>>()
                .map_err(|e| format!("WAV decode error: {e}"))?,
            (hound::SampleFormat::Int, 24) | (hound::SampleFormat::Int, 32) => {
                let bits = spec.bits_per_sample;
                // Hound returns 24-bit samples sign-extended in i32.
                let scale = (1u64 << (bits - 1)) as f32;
                reader
                    .samples::<i32>()
                    .map(|s| s.map(|v| v as f32 / scale))
                    .collect::<Result<_, _>>()
                    .map_err(|e| format!("WAV decode error: {e}"))?
            }
            (hound::SampleFormat::Float, 32) => reader
                .samples::<f32>()
                .collect::<Result<_, _>>()
                .map_err(|e| format!("WAV decode error: {e}"))?,
            (fmt, bits) => {
                return Err(format!(
                    "unsupported WAV format: {fmt:?} {bits}-bit \
                     (truce standalone supports int 16/24/32 and float 32)"
                ));
            }
        };

        let src_channels = spec.channels as usize;
        let src_sr = spec.sample_rate as f64;
        if src_channels == 0 {
            return Err("WAV has zero channels".into());
        }
        let src_frames = raw.len() / src_channels;
        if src_frames == 0 {
            return Err("WAV is empty".into());
        }

        // Sample-rate adapt first (cheaper to rechannel a smaller
        // buffer when downsampling, and a no-op when SR matches).
        let resampled: Vec<f32> = if (src_sr - target_sr).abs() < f64::EPSILON {
            raw
        } else {
            linear_resample(&raw, src_channels, src_frames, src_sr, target_sr)
        };
        let resampled_frames = resampled.len() / src_channels;

        // Channel adapt. See the table in `formats/standalone.md`:
        //   1 → N : broadcast mono to every channel
        //   N → N : passthrough
        //   N → M, N > M : take first M channels, warn
        //   N → M, N < M : copy file to dst[0..N], zero-fill rest
        let samples: Vec<f32> = if src_channels == target_channels {
            resampled
        } else if src_channels == 1 {
            let mut out = Vec::with_capacity(resampled_frames * target_channels);
            for &s in &resampled {
                for _ in 0..target_channels {
                    out.push(s);
                }
            }
            out
        } else {
            if src_channels > target_channels {
                eprintln!(
                    "file is {src_channels}ch, device is \
                     {target_channels}ch — discarding channels [{target_channels}..{src_channels}]"
                );
            } else {
                eprintln!(
                    "file is {src_channels}ch, device is \
                     {target_channels}ch — zero-filling channels [{src_channels}..{target_channels}]"
                );
            }
            let mut out = vec![0.0_f32; resampled_frames * target_channels];
            let copy = src_channels.min(target_channels);
            for f in 0..resampled_frames {
                for ch in 0..copy {
                    out[f * target_channels + ch] = resampled[f * src_channels + ch];
                }
            }
            out
        };

        let total_frames = samples.len() / target_channels;
        Ok(Self {
            samples,
            channels: target_channels,
            total_frames,
            cursor: AtomicUsize::new(0),
        })
    }

    /// Sum `frames` frames of playback samples into `channel_bufs`
    /// (one `Vec<f32>` per device channel, all sized `>= frames`).
    /// Saturates at EOF — calls beyond `total_frames` are no-ops.
    pub fn mix_into(&self, channel_bufs: &mut [Vec<f32>], frames: usize) {
        let start = self.cursor.load(Ordering::Relaxed);
        if start >= self.total_frames {
            return;
        }
        let take = frames.min(self.total_frames - start);
        let chans = self.channels.min(channel_bufs.len());
        let stride = self.channels;
        for (ch, buf) in channel_bufs.iter_mut().take(chans).enumerate() {
            for (f, dst) in buf.iter_mut().take(take).enumerate() {
                *dst += self.samples[(start + f) * stride + ch];
            }
        }
        self.cursor.store(start + take, Ordering::Relaxed);
    }

    /// Inspect the cursor without advancing it. Used by the
    /// real-time runner's input-EOF watcher to drive clean exit
    /// when paired with `--output-file`.
    pub fn is_eof(&self) -> bool {
        self.cursor.load(Ordering::Relaxed) >= self.total_frames
    }

    /// Native sample rate / channel count of the file as decoded
    /// (matches `target_sr` / `target_channels` from `from_wav`).
    /// Used by the offline runner so it can derive the output
    /// WAV's spec from the resolved input spec.
    pub fn channels(&self) -> usize {
        self.channels
    }
}

// ---------------------------------------------------------------------------
// Capture sink — `--output-file` real-time path.
// ---------------------------------------------------------------------------

/// `--output-file` capture: owned by the runner. Spawns a writer
/// thread on `create`; runner calls `finalize` (consuming `self`)
/// during shutdown to set the shutdown flag and join the writer.
///
/// The audio callback doesn't hold `CaptureSink` directly — it
/// holds a [`CapturePusher`] (cheap Clone) which references the
/// same channel + flags.
///
/// Shutdown isn't driven by sender drop because cpal on macOS may
/// keep the closure (and therefore the SyncSender clone) alive
/// for some time after the cpal Stream is dropped — a
/// `CapturePusher` left holding a sender would block the writer
/// thread's `recv` indefinitely. Instead, an `Arc<AtomicBool>`
/// shutdown flag short-circuits both sides: the runner sets it
/// in `finalize`, the audio callback checks it before submitting,
/// and the writer drains whatever's already in flight, then exits.
pub struct CaptureSink {
    chunk_tx: mpsc::SyncSender<Vec<f32>>,
    blocked_at_least_once: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    writer: Option<thread::JoinHandle<()>>,
    /// Mirror of the spec for the diagnostic line on finalize.
    spec: hound::WavSpec,
    path: std::path::PathBuf,
}

/// Cheap-clone handle the audio callback uses to push blocks.
/// Each clone holds its own `mpsc::SyncSender` (also cheap to
/// clone — internally `Arc<…>`); the writer thread exits via the
/// shared shutdown flag, not channel close, so it doesn't matter
/// how many sender clones outlive the runner.
#[derive(Clone)]
pub struct CapturePusher {
    chunk_tx: mpsc::SyncSender<Vec<f32>>,
    blocked_at_least_once: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
}

/// Channel depth in blocks. Tuned for typical block sizes
/// (256–1024 frames @ 48 kHz = 5–20 ms each), so the disk has
/// ~0.5–2 s of headroom before the audio thread starts blocking.
const CAPTURE_CHANNEL_DEPTH: usize = 128;

impl CaptureSink {
    /// Open `path` for writing, spawn a writer thread that
    /// drains `chunk_rx` until the shutdown flag is set. Errors
    /// only on filesystem / hound problems (path unwritable,
    /// parent missing, etc).
    pub fn create(path: &Path, sample_rate: f64, channels: usize) -> Result<Self, String> {
        let spec = hound::WavSpec {
            channels: channels as u16,
            sample_rate: sample_rate as u32,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(path, spec)
            .map_err(|e| format!("could not create '{}': {e}", path.display()))?;

        let (chunk_tx, chunk_rx) = mpsc::sync_channel::<Vec<f32>>(CAPTURE_CHANNEL_DEPTH);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_w = Arc::clone(&shutdown);
        let writer_thread = thread::Builder::new()
            .name("truce-standalone-capture".into())
            .spawn(move || {
                // Drain loop. Use a short `recv_timeout` so the
                // shutdown flag is checked even when no chunks
                // are arriving (e.g. cpal stream already torn
                // down). On shutdown, take one more pass through
                // the channel with `try_recv` to flush any
                // chunks that landed after the flag flipped.
                loop {
                    match chunk_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                        Ok(chunk) => {
                            for sample in chunk {
                                if let Err(e) = writer.write_sample(sample) {
                                    eprintln!("capture write failed: {e}");
                                    return;
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if shutdown_w.load(Ordering::Relaxed) {
                                break;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                // Final drain: anything queued before the audio
                // thread saw the flag should still make it to
                // disk so the WAV's tail is byte-complete.
                while let Ok(chunk) = chunk_rx.try_recv() {
                    for sample in chunk {
                        if let Err(e) = writer.write_sample(sample) {
                            eprintln!("capture write failed: {e}");
                            return;
                        }
                    }
                }
                if let Err(e) = writer.finalize() {
                    eprintln!("capture finalize failed: {e}");
                }
            })
            .map_err(|e| format!("could not spawn capture writer: {e}"))?;

        Ok(Self {
            chunk_tx,
            blocked_at_least_once: Arc::new(AtomicBool::new(false)),
            shutdown,
            writer: Some(writer_thread),
            spec,
            path: path.to_path_buf(),
        })
    }

    /// Get a cheap-clone handle suitable for the audio
    /// callback. Multiple pushers can coexist; the writer thread
    /// shuts down via the shared flag, not channel close.
    pub fn pusher(&self) -> CapturePusher {
        CapturePusher {
            chunk_tx: self.chunk_tx.clone(),
            blocked_at_least_once: Arc::clone(&self.blocked_at_least_once),
            shutdown: Arc::clone(&self.shutdown),
        }
    }

    /// Signal the writer to stop and join it. Any audio-thread
    /// `CapturePusher` clones still alive (e.g. held inside a
    /// cpal closure that hasn't dropped yet on macOS) will see
    /// the same flag and skip further `submit` calls, so they
    /// don't fight the writer for the channel.
    pub fn finalize(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.writer.take() {
            let _ = handle.join();
        }
        eprintln!(
            "captured to {} ({} Hz, {} ch, f32)",
            self.path.display(),
            self.spec.sample_rate,
            self.spec.channels,
        );
    }
}

impl CapturePusher {
    /// Hand a block of interleaved samples to the writer thread.
    /// No-op once the shutdown flag is set (typically because
    /// the runner already finalized — cpal callbacks may keep
    /// firing for a few hundred ms after the Stream is dropped
    /// on macOS, and we don't want to enqueue garbage past
    /// finalize). Try non-blocking first; on full, warn once and
    /// fall through to a blocking `send` so the WAV stays
    /// byte-complete (audible glitch on speakers when this trips
    /// is the documented trade-off).
    pub fn submit(&self, interleaved: Vec<f32>) {
        if self.shutdown.load(Ordering::Relaxed) {
            return;
        }
        match self.chunk_tx.try_send(interleaved) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(samples)) => {
                if !self.blocked_at_least_once.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "capture: audio thread blocking on \
                         disk write — output may glitch (this warning fires once)"
                    );
                }
                let _ = self.chunk_tx.send(samples);
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                // Writer exited — capture is dead. Audio
                // continues; the file is whatever got flushed.
            }
        }
    }
}

impl Drop for CaptureSink {
    /// Cover the unhappy path (panic / early exit). Set the
    /// shutdown flag and best-effort join so the WAV header is
    /// still rewritten with the real sample count.
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.writer.take() {
            let _ = handle.join();
        }
    }
}

/// Linear-interp resample interleaved `src` from `src_sr` to
/// `target_sr`. Quality limitation called out in `--help`. No
/// anti-alias filter — fine for pre-rendered test signals at the
/// device's native SR (the dominant case is no resample at all),
/// audible aliasing on broadband content.
fn linear_resample(
    src: &[f32],
    channels: usize,
    src_frames: usize,
    src_sr: f64,
    target_sr: f64,
) -> Vec<f32> {
    let ratio = target_sr / src_sr;
    let target_frames = ((src_frames as f64) * ratio).round() as usize;
    let mut out = vec![0.0_f32; target_frames * channels];
    let inv_ratio = src_sr / target_sr;
    for f in 0..target_frames {
        let src_pos = f as f64 * inv_ratio;
        let lo = src_pos.floor() as usize;
        let hi = (lo + 1).min(src_frames - 1);
        let t = (src_pos - lo as f64) as f32;
        for ch in 0..channels {
            let a = src[lo * channels + ch];
            let b = src[hi * channels + ch];
            out[f * channels + ch] = a + (b - a) * t;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(path: &Path, sr: u32, channels: u16, samples: &[i16]) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: sr,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for &s in samples {
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    #[test]
    fn one_shot_saturates_at_eof() {
        let dir = tempdir_path();
        let path = dir.join("tone.wav");
        // 4 frames stereo, simple ramp.
        write_wav(
            &path,
            48_000,
            2,
            &[1000, -1000, 2000, -2000, 3000, -3000, 4000, -4000],
        );

        let src = PlaybackSource::from_wav(&path, 48_000.0, 2).unwrap();
        assert_eq!(src.total_frames, 4);

        let mut bufs = vec![vec![0.0_f32; 8]; 2];
        src.mix_into(&mut bufs, 8);
        // First 4 frames have content (additive into zero-init bufs)
        assert!(bufs[0][0] != 0.0);
        assert!(bufs[0][3] != 0.0);
        // Frames 4..8 stay zero — saturated.
        assert_eq!(bufs[0][4], 0.0);
        assert_eq!(bufs[0][7], 0.0);

        // Subsequent calls are no-ops.
        let mut bufs2 = vec![vec![0.0_f32; 4]; 2];
        src.mix_into(&mut bufs2, 4);
        for ch in &bufs2 {
            for &s in ch {
                assert_eq!(s, 0.0);
            }
        }
    }

    #[test]
    fn mono_broadcasts_to_stereo() {
        let dir = tempdir_path();
        let path = dir.join("mono.wav");
        write_wav(&path, 48_000, 1, &[16384, -16384]);
        let src = PlaybackSource::from_wav(&path, 48_000.0, 2).unwrap();
        let mut bufs = vec![vec![0.0_f32; 2]; 2];
        src.mix_into(&mut bufs, 2);
        // L and R should be equal (mono broadcast).
        assert_eq!(bufs[0][0], bufs[1][0]);
        assert_eq!(bufs[0][1], bufs[1][1]);
        assert!(bufs[0][0] > 0.4);
        assert!(bufs[0][1] < -0.4);
    }

    #[test]
    fn mix_is_additive() {
        let dir = tempdir_path();
        let path = dir.join("ones.wav");
        write_wav(&path, 48_000, 2, &[16384, 16384, 16384, 16384]);
        let src = PlaybackSource::from_wav(&path, 48_000.0, 2).unwrap();
        let mut bufs = vec![vec![0.5_f32; 2]; 2];
        src.mix_into(&mut bufs, 2);
        // 0.5 + ~0.5 = ~1.0 (mic-style pre-existing signal + file).
        assert!((bufs[0][0] - 1.0).abs() < 0.01);
    }

    #[test]
    fn capture_sink_writes_submitted_samples() {
        let dir = tempdir_path();
        let path = dir.join("capture.wav");
        let sink = CaptureSink::create(&path, 48_000.0, 2).unwrap();
        let pusher = sink.pusher();

        // Submit two stereo blocks of distinguishable values.
        pusher.submit(vec![0.1, 0.2, 0.3, 0.4]);
        pusher.submit(vec![0.5, 0.6, 0.7, 0.8]);
        sink.finalize();

        let mut r = hound::WavReader::open(&path).unwrap();
        assert_eq!(r.spec().channels, 2);
        assert_eq!(r.spec().sample_rate, 48_000);
        assert_eq!(r.spec().sample_format, hound::SampleFormat::Float);
        assert_eq!(r.duration(), 4); // 4 frames total (2 per block, 2 blocks)
        let samples: Vec<f32> = r.samples::<f32>().collect::<Result<_, _>>().unwrap();
        assert_eq!(samples.len(), 8);
        assert!((samples[0] - 0.1).abs() < 1e-6);
        assert!((samples[7] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn capture_sink_skips_submit_after_finalize() {
        // The shutdown flag short-circuits cpal callbacks that
        // keep firing after the runner has called `finalize`.
        let dir = tempdir_path();
        let path = dir.join("late.wav");
        let sink = CaptureSink::create(&path, 48_000.0, 2).unwrap();
        let pusher = sink.pusher();
        pusher.submit(vec![0.1, 0.1]);
        sink.finalize();
        // Late submit — the audio thread didn't see the flag in
        // time. Should be a no-op (no panic, no late write).
        pusher.submit(vec![0.9, 0.9]);

        let mut r = hound::WavReader::open(&path).unwrap();
        assert_eq!(r.duration(), 1);
        let samples: Vec<f32> = r.samples::<f32>().collect::<Result<_, _>>().unwrap();
        assert_eq!(samples.len(), 2);
        assert!((samples[0] - 0.1).abs() < 1e-6);
    }

    fn tempdir_path() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "truce-standalone-playback-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
