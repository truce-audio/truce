//! Offline render - `--no-playback --input-file in.wav --output-file out.wav`.
//!
//! Decodes the input WAV to channel-major buffers, hands the whole
//! thing to [`truce_driver::PluginDriver`] as an
//! [`InputSource::Buffer`] for a fixed duration, then writes the
//! captured output via [`truce_driver::DriverResult::write_wav`]. No threads, no
//! mpsc, no cpal. Disk slowness stretches render time but never
//! causes glitches. An optional `--sidechain-file` decodes into the
//! plugin's sidechain bus via the driver's `sidechain` source.
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
//! Instrument support is disabled because instruments need a MIDI
//! input source (e.g. a MIDI file driver), which this offline path
//! doesn't expose.

use std::path::Path;
use std::time::{Duration, Instant};

use truce_core::bus::BusLayout;
use truce_core::cast::frame_count_f64;
use truce_core::config::ProcessMode;
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_driver::{InputSource, PluginDriver};

use crate::cli::Options;

/// Block size when `--buffer` isn't supplied. 1024 frames at
/// 48 kHz is ~21 ms - plenty of room for plugin work, small
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
        return Err("offline render only supports effect plugins; \
             instruments need a MIDI input source which this path does not expose"
            .into());
    }

    let (file_sr, file_channels) = peek_wav_spec(input_path)?;
    let sample_rate = opts
        .sample_rate
        .map_or_else(|| f64::from(file_sr), f64::from);
    // Render in a declared plugin layout and adapt the input file to that
    // layout's MAIN width - never let the file's own width become the
    // main-bus width. The sidechain bus is appended right after the main
    // channels, so a file narrower or wider than the plugin's main bus
    // would otherwise shift the sidechain to the wrong flat offset (a mono
    // file into a stereo-main plugin would route sidechain L as the main
    // right channel). Prefer a layout whose main width already matches the
    // file so the common case never remixes; fall back to the first
    // declared layout otherwise. `channels` also drives the sidechain
    // offset and the output width.
    let layouts = P::bus_layouts();
    let (channels, sidechain_width) = resolve_render_layout(&layouts, file_channels)
        .ok_or("effect plugin declares no bus layout; cannot offline-render")?;
    if channels == 0 {
        return Err("plugin's main input bus has no channels; cannot offline-render".into());
    }
    let block_size = opts.buffer_size.map_or(DEFAULT_BLOCK_SIZE, |b| b as usize);

    eprintln!(
        "Offline render: {} → {} ({} Hz, {} ch main, block {} frames)",
        input_path.display(),
        output_path.display(),
        sample_rate,
        channels,
        block_size,
    );

    let input_buf = decode_wav_channel_major(input_path, sample_rate, channels)?;
    let total_frames = input_buf.first().map_or(0, std::vec::Vec::len);
    let duration = Duration::from_secs_f64(frame_count_f64(total_frames) / sample_rate);

    let sidechain_buf = match opts.sidechain_file.as_deref() {
        Some(path) if sidechain_width > 0 => {
            eprintln!(
                "Offline sidechain: {} → sidechain bus ({} ch)",
                path.display(),
                sidechain_width,
            );
            Some(decode_wav_channel_major(
                path,
                sample_rate,
                sidechain_width,
            )?)
        }
        Some(path) => {
            eprintln!(
                "--sidechain-file {} ignored: plugin has no sidechain input bus",
                path.display(),
            );
            None
        }
        None => None,
    };

    let started = Instant::now();

    let mut driver = PluginDriver::<P>::new()
        .sample_rate(sample_rate)
        .channels(channels)
        // Pin the sidechain width to the layout we selected, so the
        // driver's flat input matches even when that isn't layout 0.
        .sidechain_channels(sidechain_width)
        .block_size(block_size)
        .duration(duration)
        .process_mode(ProcessMode::Offline)
        .bpm(opts.bpm.unwrap_or(120.0))
        .input(InputSource::Buffer(input_buf));

    if let Some(sc) = sidechain_buf {
        driver = driver.sidechain(InputSource::Buffer(sc));
    }

    if let Some(path) = opts.state_path.as_deref() {
        driver = driver.state_file(path);
    }
    // `--preset` applies through the driver's setup hook (full
    // envelope, same as windowed / headless), independent of the
    // `state_file` path above.
    if let Some(sel) = opts.preset.clone() {
        let presets_dir = opts.presets_dir.clone();
        driver = driver.setup(move |plugin, _ctx| {
            let store = crate::presets::store::<P>(presets_dir.as_deref());
            crate::presets::apply_selected(&store, plugin, &sel);
        });
    }

    let result = driver.run();
    result
        .write_wav(output_path)
        .map_err(|e| format!("WAV write failed: {e}"))?;

    let elapsed = started.elapsed();
    let render_secs = frame_count_f64(total_frames) / sample_rate;
    let speedup = render_secs / elapsed.as_secs_f64().max(1e-9);
    eprintln!(
        "Offline render: wrote {} frames in {:.2}s ({:.1}× real-time)",
        total_frames,
        elapsed.as_secs_f32(),
        speedup
    );

    Ok(())
}

/// Channels of a layout's main (first) input bus.
fn main_bus_width(layout: &BusLayout) -> usize {
    layout
        .inputs
        .first()
        .map_or(0, |b| b.channels.channel_count() as usize)
}

/// Summed channels of a layout's non-main (sidechain / aux) input buses.
fn sidechain_bus_width(layout: &BusLayout) -> usize {
    layout
        .inputs
        .iter()
        .skip(1)
        .map(|b| b.channels.channel_count() as usize)
        .sum()
}

/// Pick the layout to render in and return `(main_width, sidechain_width)`.
/// Prefers a declared layout whose main width already equals the file's,
/// so the common case never up/down-mixes; falls back to the first
/// declared layout otherwise. The file is then adapted to `main_width`, so
/// the sidechain (appended after the main channels) always lands at the
/// right flat offset regardless of the file's own width. `None` only when
/// the plugin declares no layout at all.
fn resolve_render_layout(layouts: &[BusLayout], file_channels: usize) -> Option<(usize, usize)> {
    let layout = layouts
        .iter()
        .find(|l| main_bus_width(l) == file_channels)
        .or_else(|| layouts.first())?;
    Some((main_bus_width(layout), sidechain_bus_width(layout)))
}

/// Minimal WAV spec read - open, grab `(sample_rate, channels)`,
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
/// for the format/SR/channel adapter logic - this drains the source
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

#[cfg(test)]
mod tests {
    use super::resolve_render_layout;
    use truce_core::bus::{BusLayout, ChannelConfig};

    /// A mono source into a stereo-main + stereo-sidechain plugin must
    /// render at the plugin's declared main width (2), not the file's
    /// width (1) - otherwise the sidechain, appended after the main
    /// channels, would start at flat index 1 and alias the main right
    /// channel. The main file is up-mixed to stereo; the sidechain lands
    /// at index 2.
    #[test]
    fn mono_source_stereo_main_keeps_declared_width() {
        let layouts =
            [BusLayout::stereo().with_sidechain_input("Sidechain", ChannelConfig::Stereo)];
        assert_eq!(resolve_render_layout(&layouts, 1), Some((2, 2)));
    }

    /// A stereo source into a mono-main + stereo-sidechain plugin renders
    /// at the declared main width (1): the file is down-mixed to mono and
    /// the sidechain starts at flat index 1, not 2.
    #[test]
    fn stereo_source_mono_main_keeps_declared_width() {
        let layouts = [BusLayout::new()
            .with_input("Main", ChannelConfig::Mono)
            .with_output("Main", ChannelConfig::Mono)
            .with_sidechain_input("Sidechain", ChannelConfig::Stereo)];
        assert_eq!(resolve_render_layout(&layouts, 2), Some((1, 2)));
    }

    /// When a declared layout's main width already matches the file, it's
    /// preferred over the first layout so the common case never remixes.
    #[test]
    fn prefers_layout_matching_file_width() {
        let layouts = [
            BusLayout::stereo().with_sidechain_input("Sidechain", ChannelConfig::Stereo),
            BusLayout::new()
                .with_input("Main", ChannelConfig::Mono)
                .with_output("Main", ChannelConfig::Mono)
                .with_sidechain_input("Sidechain", ChannelConfig::Mono),
        ];
        // Mono file matches the second layout: main 1, sidechain 1.
        assert_eq!(resolve_render_layout(&layouts, 1), Some((1, 1)));
        // Stereo file matches the first layout: main 2, sidechain 2.
        assert_eq!(resolve_render_layout(&layouts, 2), Some((2, 2)));
    }

    /// No compatible layout: fall back to the first declared layout and
    /// adapt the file to its main width.
    #[test]
    fn falls_back_to_first_layout() {
        let layouts =
            [BusLayout::stereo().with_sidechain_input("Sidechain", ChannelConfig::Stereo)];
        // 6-channel file, no 6ch layout: use the first (stereo) layout.
        assert_eq!(resolve_render_layout(&layouts, 6), Some((2, 2)));
    }

    /// No declared layout at all yields `None` (render is rejected).
    #[test]
    fn no_layout_is_none() {
        assert_eq!(resolve_render_layout(&[], 2), None);
    }
}
