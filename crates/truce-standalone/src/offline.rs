//! Offline render — `--no-playback --input-file in.wav --output-file out.wav`.
//!
//! Bypasses cpal entirely. A tight loop reads from
//! `PlaybackSource`, runs `plugin.process`, and writes to a
//! `hound::WavWriter` as fast as the CPU allows. Mirrors
//! `in_process::run` but with WAV I/O instead of in-memory
//! buffers and scripted MIDI.
//!
//! Gated on `feature = "playback"`; called from
//! `lib.rs::run_with` when the user's flag combination
//! resolves to offline mode (see `cli.rs::HELP_PLAYBACK`).
//!
//! No threads, no atomics, no mpsc. Just a synchronous loop —
//! disk slowness stretches render time but never causes
//! glitches.

use std::path::Path;
use std::time::Instant;

use truce_core::buffer::AudioBuffer;
use truce_core::events::EventList;
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_core::process::ProcessContext;
use truce_params::Params;

use crate::cli::Options;
use crate::playback::PlaybackSource;
use crate::transport::Transport;

/// Block size when `--buffer` isn't supplied. 1024 frames at
/// 48 kHz is ~21 ms — plenty of room for plugin work, small
/// enough that a tail-truncation loss is bounded.
const DEFAULT_BLOCK_SIZE: usize = 1024;

/// Drive the plugin against the input file and write the result
/// to the output file. Caller has already validated that both
/// `opts.input_file` and `opts.output_file` are `Some`.
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

    let is_effect = P::info().category == PluginCategory::Effect;
    if !is_effect {
        return Err(
            "offline render currently only supports effect plugins \
             (instruments need a --midi-file driver, not yet implemented)"
                .into(),
        );
    }

    // Resolve sample rate: CLI override wins; else inherit the
    // input file's native SR. Channel count: read the file's
    // spec separately so we can pick a sensible target without
    // double-decoding.
    let (file_sr, file_channels) = peek_wav_spec(input_path)?;
    let sample_rate = opts
        .sample_rate
        .map(|s| s as f64)
        .unwrap_or_else(|| file_sr as f64);
    // Cap to 2 channels for v1 — most plugins are stereo;
    // surround / mono workflows can wait for an explicit flag.
    let channels = file_channels.clamp(1, 2);
    let block_size = opts
        .buffer_size
        .map(|b| b as usize)
        .unwrap_or(DEFAULT_BLOCK_SIZE);

    eprintln!(
        "Offline render: {} → {} ({} Hz, {} ch, block {} frames)",
        input_path.display(),
        output_path.display(),
        sample_rate,
        channels,
        block_size,
    );

    let input = PlaybackSource::from_wav(input_path, sample_rate, channels)?;

    let writer_spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate: sample_rate as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(output_path, writer_spec)
        .map_err(|e| format!("could not create '{}': {e}", output_path.display()))?;

    let mut plugin = P::create();
    plugin.init();
    plugin.reset(sample_rate, block_size);
    plugin.params().set_sample_rate(sample_rate);
    plugin.params().snap_smoothers();

    let transport = Transport::new(opts.bpm.unwrap_or(120.0), sample_rate);

    let mut total_frames = 0_usize;
    let started = Instant::now();

    while !input.is_eof() {
        // Per-block scratch buffers. v1 reallocs each block —
        // negligible against plugin process cost; can hoist to
        // a single pre-allocated buffer in v2 if a profile
        // shows it matters.
        let mut channel_bufs: Vec<Vec<f32>> =
            (0..channels).map(|_| vec![0.0_f32; block_size]).collect();

        // Sum the input file's contribution into the per-channel
        // buffers (offline mode has no mic to add).
        input.mix_into(&mut channel_bufs, block_size);

        // Build the AudioBuffer / context for this block. Same
        // shape as `audio.rs::audio_callback` and `in_process.rs`.
        let input_bufs: Vec<Vec<f32>> = channel_bufs.clone();
        let input_slices: Vec<&[f32]> = input_bufs.iter().map(|b| b.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            channel_bufs.iter_mut().map(|b| b.as_mut_slice()).collect();
        let mut audio = unsafe {
            AudioBuffer::from_slices(&input_slices, &mut output_slices, block_size)
        };

        let transport_info = transport.tick_audio(block_size);
        let event_list = EventList::new();
        let mut output_events = EventList::new();
        let mut ctx =
            ProcessContext::new(&transport_info, sample_rate, block_size, &mut output_events);

        plugin.process(&mut audio, &event_list, &mut ctx);

        // Write the block to disk, one sample at a time
        // (interleaved). Hound's `write_sample` is a thin wrap
        // over the underlying writer — fine for v1, can switch
        // to `write_sample_buffer` later if it's a hot path.
        for f in 0..block_size {
            for buf in channel_bufs.iter().take(channels) {
                writer
                    .write_sample(buf[f])
                    .map_err(|e| format!("WAV write failed: {e}"))?;
            }
        }
        total_frames += block_size;
    }

    writer
        .finalize()
        .map_err(|e| format!("WAV finalize failed: {e}"))?;

    let elapsed = started.elapsed();
    let render_secs = total_frames as f64 / sample_rate;
    let speedup = render_secs / elapsed.as_secs_f64().max(1e-9);
    eprintln!(
        "Offline render: wrote {} frames in {:.2}s ({:.1}× real-time)",
        total_frames, elapsed.as_secs_f32(), speedup
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
