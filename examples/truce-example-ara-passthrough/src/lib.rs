//! Minimal ARA (Audio Random Access) plug-in with an egui editor that
//! displays whatever the ARA layer hands it.
//!
//! It works as an ordinary gain-passthrough insert when the host has no
//! ARA, and advertises ARA capability (the `ara:supported` CLAP feature +
//! the ARA factory / plug-in extension) when it does. Under an ARA host
//! it analyzes each audio source off-thread (sample count, channels,
//! peak), renders its assigned playback regions, and persists the
//! analysis. The editor mirrors that live: source analysis, the assigned
//! region's placement, and a render-block counter, so every ARA phase is
//! observable without a debugger.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use truce::prelude::*;
#[cfg(feature = "ara")]
use truce_ara::{AnalysisProgress, AraAudioSource, AraRenderContext, PlaybackRegion};
use truce_core::editor::PluginContext;
use truce_egui::EguiEditor;
use truce_egui::theme::{HEADER_BG, HEADER_TEXT, PRIMARY, TEXT, TEXT_DIM};
use truce_egui::widgets::param_knob;
use truce_font::JETBRAINS_MONO;

use AraParamsParamId as P;

const WINDOW_W: u32 = 340;
const WINDOW_H: u32 = 340;

#[derive(Params)]
pub struct AraParams {
    #[param(
        name = "Gain",
        range = "linear(-60, 6)",
        unit = "dB",
        smooth = "exp(5)"
    )]
    pub gain: FloatParam,

    /// Live ARA readout, shared through the params `Arc` between the
    /// render callback that writes it and the editor that displays it.
    /// Not a parameter, so it carries `#[skip]`.
    #[skip]
    pub ara: AraInfo,
}

/// Per-audio-source analysis: the source's shape, peak level, and a cached
/// copy of the source audio (per channel) so `render_regions` can play it
/// back. Produced off-thread in [`AraPassthrough::analyze`]; the `Arc` keeps
/// the audio-thread copy cheap. `store` persists only the metadata, not the
/// samples (they're re-read on the next analysis).
#[derive(Clone, Default)]
pub struct SourceAnalysis {
    pub sample_count: i64,
    pub channels: u32,
    pub sample_rate: f64,
    pub peak: f32,
    /// Cached source audio, one `Vec<f32>` per channel. Empty on a restored
    /// analysis until re-analyzed.
    pub samples: Arc<Vec<Vec<f32>>>,
}

/// What the ARA layer has told this plug-in, as lock-free atomics so the
/// render thread can write it without blocking the editor.
#[derive(Default)]
pub struct AraInfo {
    render_blocks: AtomicU64,
    region_count: AtomicUsize,
    /// First region's placement on the playback timeline, in seconds
    /// (`f64` bits).
    region_start: AtomicU64,
    region_duration: AtomicU64,
    /// First region's source analysis, when analysis has completed.
    has_analysis: AtomicBool,
    src_samples: AtomicI64,
    src_channels: AtomicU32,
    src_sample_rate: AtomicU64,
    src_peak: AtomicU32,
}

impl AraInfo {
    /// Record one render block's worth of ARA state. Atomics only - safe
    /// to call from the audio thread.
    #[cfg(feature = "ara")]
    fn record(&self, regions: &[PlaybackRegion<SourceAnalysis>]) {
        self.render_blocks.fetch_add(1, Ordering::Relaxed);
        self.region_count.store(regions.len(), Ordering::Relaxed);
        let Some(region) = regions.first() else {
            self.has_analysis.store(false, Ordering::Relaxed);
            return;
        };
        self.region_start
            .store(region.start_in_playback_time.to_bits(), Ordering::Relaxed);
        self.region_duration.store(
            region.duration_in_playback_time.to_bits(),
            Ordering::Relaxed,
        );
        match region.analysis() {
            Some(a) => {
                self.src_samples.store(a.sample_count, Ordering::Relaxed);
                self.src_channels.store(a.channels, Ordering::Relaxed);
                self.src_sample_rate
                    .store(a.sample_rate.to_bits(), Ordering::Relaxed);
                self.src_peak.store(a.peak.to_bits(), Ordering::Relaxed);
                self.has_analysis.store(true, Ordering::Relaxed);
            }
            None => self.has_analysis.store(false, Ordering::Relaxed),
        }
    }

    /// A plain snapshot for the editor to render.
    fn snapshot(&self) -> AraSnapshot {
        AraSnapshot {
            render_blocks: self.render_blocks.load(Ordering::Relaxed),
            region_count: self.region_count.load(Ordering::Relaxed),
            region_start: f64::from_bits(self.region_start.load(Ordering::Relaxed)),
            region_duration: f64::from_bits(self.region_duration.load(Ordering::Relaxed)),
            analysis: self
                .has_analysis
                .load(Ordering::Relaxed)
                .then(|| SourceAnalysis {
                    sample_count: self.src_samples.load(Ordering::Relaxed),
                    channels: self.src_channels.load(Ordering::Relaxed),
                    sample_rate: f64::from_bits(self.src_sample_rate.load(Ordering::Relaxed)),
                    peak: f32::from_bits(self.src_peak.load(Ordering::Relaxed)),
                    // Display-only snapshot; the audio samples aren't needed.
                    samples: Arc::default(),
                }),
        }
    }
}

/// Plain-value view of [`AraInfo`] for the editor.
struct AraSnapshot {
    render_blocks: u64,
    region_count: usize,
    region_start: f64,
    region_duration: f64,
    analysis: Option<SourceAnalysis>,
}

/// Stateless passthrough descriptor.
pub struct AraPassthrough;

impl PurePluginLogic for AraPassthrough {
    type Params = AraParams;

    fn process(
        params: &AraParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Block-smoothed gain via `read_into`: one atomic load/store
        // amortized over the chunk (vs. `read()` per sample), converted
        // dB -> linear per sample. Chunk through a fixed stack buffer so any
        // host block size is handled.
        const CHUNK: usize = 512;
        let mut gain_db = [0.0f32; CHUNK];
        let total = buffer.num_samples();
        let mut offset = 0;
        while offset < total {
            let n = (total - offset).min(CHUNK);
            params.gain.read_into(&mut gain_db[..n]);
            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                for i in 0..n {
                    out[offset + i] = inp[offset + i] * db_to_linear(gain_db[i]);
                }
            }
            offset += n;
        }
        ProcessStatus::Normal
    }

    fn editor(params: Arc<AraParams>) -> Box<dyn Editor> {
        EguiEditor::new(params, (WINDOW_W, WINDOW_H), ara_ui)
            .with_visuals(truce_egui::theme::dark())
            .with_font(JETBRAINS_MONO)
            .into_editor()
    }
}

/// ARA capability. `Analysis` is the per-source [`SourceAnalysis`]; the
/// plug-in stays stateless (`DspState = ()`), f32, with `AraParams`.
#[cfg(feature = "ara")]
impl truce_ara::AraPlugin for AraPassthrough {
    // Declare a playback transformation so hosts engage this as a real
    // playback renderer (and route rendering through `render_regions`)
    // instead of optimizing a do-nothing passthrough away. The example
    // doesn't actually time-stretch; it just needs to look like a renderer.
    const PLAYBACK_TRANSFORMATION_FLAGS: i32 =
        truce_ara::abi::kARAPlaybackTransformationTimestretch;

    type Analysis = SourceAnalysis;
    type Edits = ();
    type Sample = f32;
    type DspState = ();
    type Params = AraParams;

    fn analyze(source: &AraAudioSource, progress: &AnalysisProgress) -> SourceAnalysis {
        // Read the whole source in blocks, tracking the peak and caching the
        // samples so `render_regions` can play them back. Off the audio
        // thread, so the allocations are fine.
        const BLOCK: usize = 4096;
        let channels = source.channel_count();
        let total = source.sample_count();
        let mut scratch: Vec<Vec<f32>> = (0..channels as usize).map(|_| vec![0.0; BLOCK]).collect();
        let mut cached: Vec<Vec<f32>> = (0..channels as usize).map(|_| Vec::new()).collect();
        let mut peak = 0.0f32;
        let mut frame = 0i64;
        while frame < total {
            let want = frames_left(total, frame, BLOCK);
            let mut slices: Vec<&mut [f32]> = scratch.iter_mut().map(|b| &mut b[..want]).collect();
            if source.read(frame, &mut slices).is_ok() {
                for (ch, channel) in scratch.iter().enumerate() {
                    for &s in &channel[..want] {
                        peak = peak.max(s.abs());
                    }
                    cached[ch].extend_from_slice(&channel[..want]);
                }
            }
            frame += i64::try_from(want).unwrap_or(i64::MAX);
            progress.update(analyzed_fraction(frame, total));
        }
        SourceAnalysis {
            sample_count: total,
            channels,
            sample_rate: source.sample_rate(),
            peak,
            samples: Arc::new(cached),
        }
    }

    fn render_regions(
        _state: &mut (),
        params: &AraParams,
        regions: &[PlaybackRegion<SourceAnalysis>],
        buffer: &mut AudioBuffer,
        context: &AraRenderContext,
    ) -> ProcessStatus {
        // Per-sample smoothed gain (dB) via `read_into`: one smoother advance
        // for the block. Reading `gain.read()` once per block (not per sample)
        // would converge block-size times too slowly. Blocks wider than this
        // reuse the last computed gain for the tail (rare; keeps the buffer on
        // the stack).
        const MAX_BLOCK: usize = 4096;
        params.ara.record(regions);
        let out_sr = context.sample_rate;
        // Playback time of the block's first sample (playhead position).
        let block_start = context.transport.position_seconds;
        let num_samples = buffer.num_samples();
        let num_channels = buffer.num_output_channels();

        // Start from silence; each region writes the samples it covers.
        for ch in 0..num_channels {
            buffer.output(ch).fill(0.0);
        }

        // When the transport is stopped the playhead is frozen, so every
        // block would re-render the same source slice - an audible buzz.
        // Output silence instead until playback rolls.
        if !context.transport.playing {
            return ProcessStatus::Normal;
        }

        let mut gain_db = [0.0f32; MAX_BLOCK];
        let n = num_samples.min(MAX_BLOCK);
        params.gain.read_into(&mut gain_db[..n]);

        for region in regions {
            let Some(a) = region.analysis() else { continue };
            if a.samples.is_empty() {
                continue;
            }
            let playback_start = region.start_in_playback_time;
            let playback_len = region.duration_in_playback_time;
            let mod_start = region.start_in_modification_time;
            let mod_len = region.duration_in_modification_time;
            let region_end = playback_start + playback_len;
            // Source samples per second of modification time, from the
            // region + source geometry rather than the reported source rate:
            // hosts disagree on the ARA time base (Reaper references the
            // source's rate, Cubase the project's), so `src_count / Dm` plays
            // the full clip in either.
            let src_per_sec = src_samples_per_mod_sec(a.sample_count, mod_len, a.sample_rate);
            // Playback-to-modification stretch (1.0 for an unstretched region).
            let stretch = if playback_len > 0.0 {
                mod_len / playback_len
            } else {
                1.0
            };
            for ch in 0..num_channels {
                // Map an output channel onto a source channel (mono source
                // feeds every output; extra output channels reuse the last).
                let src = &a.samples[ch.min(a.samples.len() - 1)];
                let out = buffer.output(ch);
                for (i, slot) in out.iter_mut().enumerate() {
                    let play_time = block_start + sample_seconds(i, out_sr);
                    if play_time < playback_start || play_time >= region_end {
                        continue;
                    }
                    let mod_time = mod_start + (play_time - playback_start) * stretch;
                    if let Some(&sample) = src.get(source_index(mod_time, src_per_sec)) {
                        *slot = sample * db_to_linear(gain_db[i.min(n - 1)]);
                    }
                }
            }
        }
        ProcessStatus::Normal
    }

    fn store(analysis: &SourceAnalysis, out: &mut Vec<u8>) {
        out.extend_from_slice(&analysis.sample_count.to_le_bytes());
        out.extend_from_slice(&analysis.channels.to_le_bytes());
        out.extend_from_slice(&analysis.sample_rate.to_le_bytes());
        out.extend_from_slice(&analysis.peak.to_le_bytes());
    }

    fn restore(bytes: &[u8]) -> Option<SourceAnalysis> {
        let sample_count = i64::from_le_bytes(bytes.get(0..8)?.try_into().ok()?);
        let channels = u32::from_le_bytes(bytes.get(8..12)?.try_into().ok()?);
        let sample_rate = f64::from_le_bytes(bytes.get(12..20)?.try_into().ok()?);
        let peak = f32::from_le_bytes(bytes.get(20..24)?.try_into().ok()?);
        Some(SourceAnalysis {
            sample_count,
            channels,
            sample_rate,
            peak,
            // Samples aren't persisted; the source is re-read on the next
            // analysis (which the renderer kicks off on region assignment).
            samples: Arc::default(),
        })
    }
}

/// Source samples per second of modification time. Derived from the region
/// plus source geometry rather than the reported source sample rate,
/// because hosts disagree on the ARA time base (Reaper references the
/// source's sample rate, Cubase the project's). `sample_count / Dm`
/// (assuming the region maps the whole source, as a placed clip does) plays
/// the full clip at the right length in either. Falls back to the source
/// rate for a zero-length modification.
#[cfg(feature = "ara")]
#[allow(clippy::cast_precision_loss)]
fn src_samples_per_mod_sec(sample_count: i64, dur_mod: f64, src_rate: f64) -> f64 {
    if dur_mod > 0.0 {
        sample_count as f64 / dur_mod
    } else {
        src_rate
    }
}

/// Frames still unread, capped at one block. `0 <= frame <= total`, so the
/// difference is a non-negative count that fits a `usize`.
#[cfg(feature = "ara")]
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn frames_left(total: i64, frame: i64, block: usize) -> usize {
    ((total - frame).max(0) as usize).min(block)
}

/// Fraction analyzed, for the host's progress readout. Precision loss over
/// a multi-million-sample source is irrelevant for a 0..1 progress value.
#[cfg(feature = "ara")]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn analyzed_fraction(frame: i64, total: i64) -> f32 {
    if total <= 0 {
        return 1.0;
    }
    (frame as f64 / total as f64) as f32
}

/// Seconds elapsed for output-sample index `i` at `sample_rate`.
#[cfg(feature = "ara")]
#[allow(clippy::cast_precision_loss)]
fn sample_seconds(i: usize, sample_rate: f64) -> f64 {
    i as f64 / sample_rate
}

/// Cached-source sample index for a modification-time position. Negative
/// times (before the source start) clamp to 0; the caller's `get` bounds
/// the upper end.
#[cfg(feature = "ara")]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn source_index(mod_time: f64, sample_rate: f64) -> usize {
    (mod_time * sample_rate).round().max(0.0) as usize
}

fn ara_ui(ui: &mut egui::Ui, state: &PluginContext<AraParams>) {
    // The ARA readout is written from the audio/model threads; egui only
    // repaints on input, so schedule a steady repaint to keep the live
    // counters (render blocks, regions, analysis) fresh without a mouseover.
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(33));
    egui::Panel::top("header")
        .exact_size(30.0)
        .frame(egui::Frame::NONE.fill(HEADER_BG))
        .show_inside(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("ARA (egui)")
                        .size(14.0)
                        .color(HEADER_TEXT)
                        .strong(),
                );
            });
        });
    egui::CentralPanel::default()
        .frame(egui::Frame::central_panel(ui.style()).inner_margin(12.0))
        .show_inside(ui, |ui| {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::TOP), |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(16.0, 0.0);
                param_knob(ui, state, P::Gain, "Gain");
                ui.vertical(|ui| ara_readout(ui, &state.params().ara.snapshot()));
            });
        });
}

/// The ARA readout column: everything the ARA layer has reported so far.
fn ara_readout(ui: &mut egui::Ui, info: &AraSnapshot) {
    ui.label(
        egui::RichText::new("ARA DIAGNOSTICS")
            .size(11.0)
            .color(PRIMARY)
            .strong(),
    );
    ui.add_space(4.0);

    if info.render_blocks == 0 {
        ui.label(
            egui::RichText::new("waiting for an ARA host...")
                .size(12.0)
                .italics()
                .color(TEXT_DIM),
        );
        return;
    }

    egui::Grid::new("ara_grid")
        .num_columns(2)
        .spacing(egui::vec2(12.0, 3.0))
        .show(ui, |ui| {
            row(ui, "render blocks", &info.render_blocks.to_string());
            row(ui, "regions", &info.region_count.to_string());
            if info.region_count > 0 {
                row(ui, "region start", &format!("{:.3} s", info.region_start));
                row(
                    ui,
                    "region length",
                    &format!("{:.3} s", info.region_duration),
                );
            }
            ui.end_row();

            match &info.analysis {
                Some(a) => {
                    row(ui, "analysis", "ready");
                    row(ui, "source frames", &a.sample_count.to_string());
                    row(ui, "channels", &a.channels.to_string());
                    row(ui, "sample rate", &format!("{:.0} Hz", a.sample_rate));
                    row(ui, "peak", &format!("{:.1} dBFS", dbfs(a.peak)));
                }
                None => row(ui, "analysis", "pending"),
            }
        });
}

/// One label/value line in the readout grid.
fn row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(egui::RichText::new(label).size(12.0).color(TEXT_DIM));
    ui.label(egui::RichText::new(value).size(12.0).color(TEXT).strong());
    ui.end_row();
}

/// Linear peak to dBFS, floored so a silent source reads a finite value.
fn dbfs(peak: f32) -> f32 {
    20.0 * peak.max(1e-9).log10()
}

truce::plugin! {
    logic: AraPassthrough,
    params: AraParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_count_matches() {
        truce_test::assert_param_count_matches::<Plugin>();
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    #[test]
    fn driver_passthrough() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .channels(2)
            .duration(Duration::from_millis(50))
            .input(InputSource::Constant(0.5))
            .run();

        assertions::assert_no_nans(&result);
    }

    /// `SourceAnalysis` survives the same store/restore the ARA archive
    /// uses, so a re-opened session skips re-analysis.
    #[cfg(feature = "ara")]
    #[test]
    fn analysis_store_round_trips() {
        use truce_ara::AraPlugin;

        let original = SourceAnalysis {
            sample_count: 96_000,
            channels: 2,
            sample_rate: 48_000.0,
            peak: 0.5,
            samples: Arc::default(),
        };
        let mut blob = Vec::new();
        AraPassthrough::store(&original, &mut blob);
        let restored = AraPassthrough::restore(&blob).expect("restore");
        assert_eq!(restored.sample_count, original.sample_count);
        assert_eq!(restored.channels, original.channels);
        assert!((restored.sample_rate - original.sample_rate).abs() < f64::EPSILON);
        assert!((restored.peak - original.peak).abs() < f32::EPSILON);
    }

    /// The ARA render path: bind an (empty) playback renderer and call the
    /// wrapper's `ara_render` exactly as `clap_plugin_process` would, and
    /// verify it routes into `render_regions` (which overwrites the buffer
    /// with a uniform marker instead of the pre-filled input).
    #[cfg(feature = "ara")]
    #[test]
    fn ara_render_routes_to_render_regions() {
        use truce_core::plugin::PluginRuntime;

        let mut plugin = Plugin::create();
        plugin.init();

        // A playback-renderer binding with no assigned regions.
        // SAFETY: a null controller ref is explicitly supported (empty
        // shared-params slot, no publish).
        let binding = unsafe {
            truce_ara::create_playback_binding(
                std::ptr::null_mut(),
                truce_ara::abi::kARAPlaybackRendererRole,
                std::sync::Arc::new(()) as std::sync::Arc<dyn core::any::Any + Send + Sync>,
            )
        };
        // Take the renderer straight off the instance (bypassing
        // `binding_renderer`, which gates on assigned regions - none here).
        // SAFETY: `binding` is a live binding from create_playback_binding.
        let renderer = unsafe { &*truce_ara::binding_instance(binding) }.playback_renderer_ref;
        assert!(!renderer.is_null(), "playback role -> renderer present");

        let inp0 = [0.0f32; 64];
        let inp1 = [0.0f32; 64];
        let mut out0 = [1.0f32; 64];
        let mut out1 = [1.0f32; 64];
        let inputs: [&[f32]; 2] = [&inp0, &inp1];
        let mut outputs: [&mut [f32]; 2] = [&mut out0, &mut out1];
        let mut buffer = AudioBuffer::from_slices_checked(&inputs, &mut outputs, 64);

        let transport = TransportInfo::default();
        let mut events = EventList::with_capacity(0);
        let mut ctx = ProcessContext::new(&transport, 48_000.0, 64, &mut events);

        let status = plugin.ara_render(renderer.cast(), &mut buffer, &mut ctx);
        assert!(matches!(status, ProcessStatus::Normal));

        // `render_regions` wrote a uniform marker over the pre-filled 1.0.
        assert!(out0.iter().all(|&s| (s - out0[0]).abs() < f32::EPSILON));
        assert!((out0[0] - 1.0).abs() > f32::EPSILON, "render_regions ran");

        // SAFETY: binding not used after this.
        unsafe { truce_ara::destroy_playback_binding(binding) };
    }
}
