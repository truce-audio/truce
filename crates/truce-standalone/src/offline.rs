//! Offline render — `--no-playback --input-file in.wav --output-file out.wav`.
//!
//! Decodes the input WAV to channel-major buffers, hands the whole
//! thing to [`truce_driver::PluginDriver`] as an
//! [`InputSource::Buffer`] for a fixed duration, then writes the
//! captured output via [`truce_driver::DriverResult::write_wav`]. No threads, no
//! mpsc, no cpal. Disk slowness stretches render time but never
//! causes glitches.
//!
//! Gated on `feature = "playback"`; called from `lib.rs::run_with`
//! when the user's flag combination resolves to offline mode (see
//! `cli.rs::HELP_PLAYBACK`).
//!
//! The driver lives in `truce-driver` so the same engine powers
//! tests (`truce-test::driver!`) and plugin authors writing custom
//! `main.rs` bins. This module just adapts the CLI input/output
//! shape to the driver's builder.
//!
//! Instrument support is currently disabled — instruments need a
//! MIDI-file driver, not yet wired up.

use std::path::Path;
use std::time::{Duration, Instant};

use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_driver::{InputSource, PluginDriver};

use crate::cli::Options;

/// Block size when `--buffer` isn't supplied. 1024 frames at
/// 48 kHz is ~21 ms — plenty of room for plugin work, small
/// enough that a tail-truncation loss is bounded.
const DEFAULT_BLOCK_SIZE: usize = 1024;

/// Drive the plugin against the input file and write the result
/// to the output file. Caller has already validated that both
/// `opts.input_file` and `opts.output_file` are `Some`.
///
/// # Errors
///
/// Returns `Err(String)` if `--input-file` / `--output-file` are
/// missing, the plugin category is not `Effect`, or any
/// `hound::WavReader` / `WavWriter` step fails (open, decode,
/// channel-count negotiation, write, finalize).
pub fn render<P: PluginExport>(opts: &Options) -> Result<(), String>
where
    P::Params: 'static,
{
    let input_path = opts
        .input_file
        .as_deref()
        .ok_or("offline render requires --input-file")?;
    let output_path = opts
        .output_file
        .as_deref()
        .ok_or("offline render requires --output-file")?;

    if P::info().category != PluginCategory::Effect {
        return Err("offline render currently only supports effect plugins \
             (instruments need a --midi-file driver, not yet implemented)"
            .into());
    }

    let (file_sr, file_channels) = peek_wav_spec(input_path)?;
    let sample_rate = opts
        .sample_rate.map_or_else(|| f64::from(file_sr), f64::from);
    // Cap to 2 channels for v1 — most plugins are stereo;
    // surround / mono workflows can wait for an explicit flag.
    let channels = file_channels.clamp(1, 2);
    let block_size = opts
        .buffer_size
        .map_or(DEFAULT_BLOCK_SIZE, |b| b as usize);

    eprintln!(
        "Offline render: {} → {} ({} Hz, {} ch, block {} frames)",
        input_path.display(),
        output_path.display(),
        sample_rate,
        channels,
        block_size,
    );

    let input_buf = decode_wav_channel_major(input_path, sample_rate, channels)?;
    let total_frames = input_buf.first().map_or(0, std::vec::Vec::len);
    let duration = Duration::from_secs_f64(total_frames as f64 / sample_rate);

    let started = Instant::now();

    let mut driver = PluginDriver::<P>::new()
        .sample_rate(sample_rate)
        .channels(channels)
        .block_size(block_size)
        .duration(duration)
        .bpm(opts.bpm.unwrap_or(120.0))
        .input(InputSource::Buffer(input_buf));

    if let Some(path) = opts.state_path.as_deref() {
        driver = driver.state_file(path);
    }

    let result = driver.run();
    result
        .write_wav(output_path)
        .map_err(|e| format!("WAV write failed: {e}"))?;

    let elapsed = started.elapsed();
    let render_secs = total_frames as f64 / sample_rate;
    let speedup = render_secs / elapsed.as_secs_f64().max(1e-9);
    eprintln!(
        "Offline render: wrote {} frames in {:.2}s ({:.1}× real-time)",
        total_frames,
        elapsed.as_secs_f32(),
        speedup
    );

    Ok(())
}

/// Minimal WAV spec read — open, grab `(sample_rate, channels)`,
/// drop. Avoids decoding the entire file just to figure out
/// what target SR / channels to render at.
fn peek_wav_spec(path: &Path) -> Result<(u32, usize), String> {
    let reader = hound::WavReader::open(path)
        .map_err(|e| format!("could not open '{}': {e}", path.display()))?;
    let spec = reader.spec();
    Ok((spec.sample_rate, spec.channels as usize))
}

/// Decode `path` to channel-major `Vec<Vec<f32>>`, adapted to
/// `target_sr` / `target_channels`. Reuses [`crate::playback::PlaybackSource`]
/// for the format/SR/channel adapter logic — this drains the source
/// once into per-channel buffers sized to the file length.
fn decode_wav_channel_major(
    path: &Path,
    target_sr: f64,
    target_channels: usize,
) -> Result<Vec<Vec<f32>>, String> {
    use crate::playback::PlaybackSource;

    let source = PlaybackSource::from_wav(path, target_sr, target_channels)?;
    let total = source.total_frames();
    let mut out: Vec<Vec<f32>> = (0..target_channels).map(|_| vec![0.0_f32; total]).collect();
    source.mix_into(&mut out, total);
    Ok(out)
}
