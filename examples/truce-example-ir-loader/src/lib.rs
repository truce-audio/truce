//! Off-audio-thread IR loading. Browse (or a restored session) hands
//! a path to a background [`LoadIrRequest`]; the result swaps into
//! `process()` via a generation-guarded lock-free queue pair. See
//! `truce-example-fundsp-reverb-worker` for the pattern.
//!
//! `process()` is pass-through only; convolution is a future example.

use std::mem;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crossbeam_queue::ArrayQueue;
use truce::prelude::*;
use truce_core::editor::PluginContext;
use truce_egui::theme::{HEADER_BG, HEADER_TEXT};
use truce_egui::{EditorUi, EguiEditor};
use truce_font::JETBRAINS_MONO;

use IrLoaderParamsParamId as P;

const WINDOW_W: u32 = 360;
const WINDOW_H: u32 = 160;

// --- Parameters ---

#[derive(Params)]
pub struct IrLoaderParams {
    /// While set, a changed `ir_file_path` is not picked up. Audio
    /// still passes through; there is no DSP to bypass.
    #[param(name = "Bypass", default = 0)]
    pub bypass: BoolParam,

    /// Loaded IR's sample count, so the editor and tests can see a
    /// swap without reaching into `DspState`.
    #[meter]
    pub meter_loaded_samples: MeterSlot,

    #[persist = "ir_file_path"]
    pub ir_file_path: RwLock<String>,

    /// `#[skip]`, not a parameter, so the receiverless
    /// `run(&params)` can reach it via `Arc`.
    #[skip]
    pub loader: Arc<LoaderShared>,
}

// --- Shared state ---

pub struct LoaderShared {
    /// Background task -> audio: latest decoded IR.
    ready: ArrayQueue<DecodedIr>,
    /// Audio -> background task: swapped-out buffers, dropped off
    /// the audio thread.
    discard: ArrayQueue<DecodedIr>,
    last_error: AtomicCell<Option<DecodeErrorCode>>,
    /// Claimed via [`Self::bump_generation`] by the editor thread
    /// and by `process()`'s live-target check.
    next_generation: AtomicCell<u64>,
    /// `PluginContext` has no sample-rate accessor, so the editor
    /// reads this (kept current by `reset()`/`process()`) to build
    /// a [`LoadIrRequest`].
    sample_rate: AtomicCell<f64>,
}

impl Default for LoaderShared {
    fn default() -> Self {
        Self {
            ready: ArrayQueue::new(1),
            discard: ArrayQueue::new(8),
            last_error: AtomicCell::new(None),
            next_generation: AtomicCell::new(1),
            sample_rate: AtomicCell::new(44_100.0),
        }
    }
}

impl LoaderShared {
    fn bump_generation(&self) -> u64 {
        self.next_generation.fetch_add(1)
    }
}

struct DecodedIr {
    channels: Vec<Arc<[f32]>>,
    sample_rate: f64,
    generation: u64,
}

/// `Copy` version of `DecodeError`, for the `AtomicCell` status cell.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DecodeErrorCode {
    Io,
    UnsupportedFormat,
    Corrupt,
}

impl DecodeErrorCode {
    fn label(self) -> &'static str {
        match self {
            DecodeErrorCode::Io => "File not found or unreadable",
            DecodeErrorCode::UnsupportedFormat => "Unsupported audio format",
            DecodeErrorCode::Corrupt => "Corrupt or malformed audio file",
        }
    }
}

impl From<&truce_sample_loader::DecodeError> for DecodeErrorCode {
    fn from(e: &truce_sample_loader::DecodeError) -> Self {
        match e {
            truce_sample_loader::DecodeError::Io(_) => DecodeErrorCode::Io,
            truce_sample_loader::DecodeError::UnsupportedFormat(_) => {
                DecodeErrorCode::UnsupportedFormat
            }
            truce_sample_loader::DecodeError::Corrupt(_) => DecodeErrorCode::Corrupt,
        }
    }
}

// --- Background task ---

pub struct LoadIrRequest {
    path: PathBuf,
    target_sample_rate: f64,
    generation: u64,
}

impl BackgroundTask for LoadIrRequest {
    type Params = IrLoaderParams;

    /// Decode and resample on the pool, off the audio thread.
    fn run(self, params: &IrLoaderParams) {
        let l = &params.loader;
        while l.discard.pop().is_some() {}

        match truce_sample_loader::decode_file(&self.path)
            .map(|d| truce_sample_loader::resample(d, self.target_sample_rate))
        {
            Ok(decoded) => {
                l.last_error.store(None);
                let _ = l.ready.force_push(DecodedIr {
                    channels: decoded.channels,
                    sample_rate: decoded.sample_rate,
                    generation: self.generation,
                });
            }
            Err(e) => l.last_error.store(Some(DecodeErrorCode::from(&e))),
        }
    }
}

// --- Plugin ---

/// Stateless; DSP state lives in [`IrLoaderDspState`].
pub struct IrLoader;

/// Live IR buffer and the request it was built from. Decode runs on
/// the framework's task pool, so there is no worker thread to own.
pub struct IrLoaderDspState {
    ir: Vec<Arc<[f32]>>,
    current_generation: u64,
    last_built_path: String,
    sample_rate: f64,
}

impl Default for IrLoaderDspState {
    fn default() -> Self {
        Self {
            // Empty = silence. `process()` must treat an empty `ir`
            // as nothing-loaded-yet, not index into it.
            ir: Vec::new(),
            current_generation: 0,
            last_built_path: String::new(),
            sample_rate: 0.0,
        }
    }
}

impl PluginLogic for IrLoader {
    type Params = IrLoaderParams;
    type DspState = IrLoaderDspState;

    /// Off the audio thread: rebuild synchronously if the path or
    /// rate changed, so the first block after a restore is not stale.
    fn reset(state: &mut IrLoaderDspState, params: &IrLoaderParams, config: &AudioConfig) {
        let sample_rate = config.sample_rate;
        params.loader.sample_rate.store(sample_rate);

        let path = params
            .ir_file_path
            .read()
            .map(|p| p.clone())
            .unwrap_or_default();

        if path.is_empty() {
            state.sample_rate = sample_rate;
            return;
        }

        let sr_changed = sample_rate.to_bits() != state.sample_rate.to_bits();
        let path_changed = path != state.last_built_path;
        if sr_changed || path_changed {
            let w = &params.loader;
            state.current_generation = w.bump_generation();
            match truce_sample_loader::decode_file(std::path::Path::new(&path))
                .map(|d| truce_sample_loader::resample(d, sample_rate))
            {
                Ok(decoded) => {
                    state.ir = decoded.channels;
                    w.last_error.store(None);
                }
                Err(e) => w.last_error.store(Some(DecodeErrorCode::from(&e))),
            }
            state.last_built_path = path;
            state.sample_rate = sample_rate;
            // Safe to drop directly: reset() already runs off-thread.
            while w.ready.pop().is_some() {}
        }
    }

    fn process(
        state: &mut IrLoaderDspState,
        params: &IrLoaderParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        let w = &params.loader;
        w.sample_rate.store(context.sample_rate);

        // A generation mismatch means a superseded request; reroute
        // to discard instead of swapping in.
        if let Some(ready) = w.ready.pop() {
            if ready.generation == state.current_generation {
                let old_ir = mem::replace(&mut state.ir, ready.channels);
                let outgoing_sample_rate = state.sample_rate;
                state.sample_rate = ready.sample_rate;
                let _ = w.discard.push(DecodedIr {
                    channels: old_ir,
                    sample_rate: outgoing_sample_rate,
                    generation: 0,
                });
            } else {
                let _ = w.discard.push(ready);
            }
        }

        // try_read never blocks; a busy lock just retries next block.
        // Only load trigger on hosts with no editor.
        if !params.bypass.value()
            && let Ok(path) = params.ir_file_path.try_read()
            && *path != state.last_built_path
            && !path.is_empty()
        {
            let new_path = path.clone();
            drop(path);
            // Optimistic: avoids re-requesting a slow pool every block.
            state.last_built_path.clone_from(&new_path);
            state.current_generation = w.bump_generation();
            if let Some(tasks) = context.tasks::<LoadIrRequest>() {
                tasks.spawn_coalescing(LoadIrRequest {
                    path: PathBuf::from(new_path),
                    target_sample_rate: context.sample_rate,
                    generation: state.current_generation,
                });
            }
        }

        // Pass-through only; no DSP runs here.
        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            out.copy_from_slice(inp);
        }

        let loaded_samples = state.ir.first().map_or(0, |ch| ch.len());
        #[allow(clippy::cast_precision_loss)]
        context.set_meter(P::MeterLoadedSamples, loaded_samples as f32);

        ProcessStatus::Normal
    }

    fn editor(params: Arc<IrLoaderParams>) -> Box<dyn Editor> {
        Box::new(
            EguiEditor::with_ui(params.clone(), (WINDOW_W, WINDOW_H), IrLoaderUi::default())
                .with_visuals(truce_egui::theme::dark())
                .with_font(JETBRAINS_MONO),
        )
    }
}

// --- Editor ---

#[derive(Default)]
struct IrLoaderUi {
    /// Linux edit buffer (no native dialog there).
    edit_buf: String,
    /// De-dupes `state_changed` against repeat restores.
    last_dispatched_path: String,
}

impl IrLoaderUi {
    /// Does not write `ir_file_path`; callers write it first.
    fn spawn_load(&mut self, ctx: &PluginContext<IrLoaderParams>, path: String) {
        if path.is_empty() {
            return;
        }
        self.last_dispatched_path.clone_from(&path);
        let loader = &ctx.params().loader;
        let generation = loader.bump_generation();
        let target_sample_rate = loader.sample_rate.load();
        if let Some(tasks) = ctx.tasks::<LoadIrRequest>() {
            tasks.spawn_coalescing(LoadIrRequest {
                path: PathBuf::from(path),
                target_sample_rate,
                generation,
            });
        }
    }
}

impl EditorUi<IrLoaderParams> for IrLoaderUi {
    fn opened(&mut self, ctx: &PluginContext<IrLoaderParams>) {
        let path = ctx
            .params()
            .ir_file_path
            .read()
            .map(|p| p.clone())
            .unwrap_or_default();
        self.edit_buf.clone_from(&path);
        self.last_dispatched_path.clone_from(&path);
    }

    /// Primary restore trigger; `process()`'s live-target check is
    /// the fallback for hosts with no editor.
    fn state_changed(&mut self, ctx: &PluginContext<IrLoaderParams>) {
        let path = ctx
            .params()
            .ir_file_path
            .read()
            .map(|p| p.clone())
            .unwrap_or_default();
        self.edit_buf.clone_from(&path);
        if path != self.last_dispatched_path {
            self.spawn_load(ctx, path);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &PluginContext<IrLoaderParams>) {
        egui::Panel::top("header")
            .exact_size(30.0)
            .frame(egui::Frame::NONE.fill(HEADER_BG))
            .show_inside(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new("IR LOADER")
                            .size(14.0)
                            .color(HEADER_TEXT)
                            .strong(),
                    );
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(12.0))
            .show_inside(ui, |ui| {
                let mut bypassed = ctx.params().bypass.value();
                if ui.checkbox(&mut bypassed, "Bypass").changed() {
                    ctx.automate(P::Bypass, if bypassed { 1.0 } else { 0.0 });
                }
                ui.add_space(4.0);

                #[cfg(any(target_os = "macos", target_os = "windows"))]
                {
                    // rfd's picker panics if called on this thread; spawn off-thread.
                    if ui.button("Browse...").clicked() {
                        let params = ctx.params().clone();
                        std::thread::spawn(move || {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("Audio", &["wav"])
                                .pick_file()
                            {
                                let path_str = path.display().to_string();
                                if let Ok(mut guard) = params.ir_file_path.write() {
                                    guard.clone_from(&path_str);
                                }
                            }
                        });
                    }
                }
                #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                {
                    ui.label("IR file path (no native dialog on this platform):");
                    ui.add_space(4.0);
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.edit_buf)
                            .hint_text("/path/to/impulse.wav")
                            .desired_width(f32::INFINITY),
                    );
                    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let path_str = self.edit_buf.clone();
                        if let Ok(mut guard) = ctx.params().ir_file_path.write() {
                            guard.clone_from(&path_str);
                        }
                        self.spawn_load(ctx, path_str);
                    }
                }

                ui.add_space(8.0);
                if let Some(err) = ctx.params().loader.last_error.load() {
                    ui.colored_label(egui::Color32::from_rgb(220, 90, 90), err.label());
                } else {
                    let loaded_samples = ctx.get_meter(P::MeterLoadedSamples);
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    if loaded_samples > 0.0 {
                        ui.label(format!("Loaded: {} samples", loaded_samples as u32));
                    } else {
                        ui.label("No IR loaded");
                    }
                }
            });
    }
}

truce::plugin! {
    logic: IrLoader,
    params: IrLoaderParams,
    tasks: [LoadIrRequest],
}

// No-op without `--features rt-paranoid`; gates the audio-alloc tests below.
truce::enable_rt_paranoid!();

#[cfg(test)]
// Fixture rates are exact literals, so exact comparison is fine.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn fixture_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test_ir.wav")
    }

    fn write_wav(path: &std::path::Path, sample_rate: u32, samples: &[f32]) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for &s in samples {
            writer.write_sample(s).unwrap();
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    #[test]
    fn editor_lifecycle() {
        truce_test::assert_editor_lifecycle::<Plugin>();
    }

    #[test]
    fn state_round_trips() {
        truce_test::assert_state_round_trip::<Plugin>();
    }

    #[test]
    fn bus_config_effect() {
        truce_test::assert_bus_config_effect::<Plugin>();
    }

    #[test]
    fn param_count_matches() {
        truce_test::assert_param_count_matches::<Plugin>();
    }

    #[test]
    fn corrupt_state_no_crash() {
        truce_test::assert_corrupt_state_no_crash::<Plugin>();
    }

    /// Steady state must never allocate; checked only with `--features rt-paranoid`.
    #[test]
    fn process_is_allocation_free_steady_state() {
        use std::time::Duration;
        use truce_test::{InputSource, assert_no_audio_alloc, driver};

        assert_no_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.5))
                .run()
        });
    }

    #[test]
    fn renders_nonzero_output() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .duration(Duration::from_millis(50))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_nonzero(&result);
    }

    // `BlockRunner` has no task pool: `context.tasks()` returns
    // `None`, so tests call `LoadIrRequest::run()` directly instead.

    /// Driving the fixture through the full plugin lands the same
    /// data it decodes to directly in `state.ir`.
    #[test]
    fn fixture_loads_end_to_end() {
        let decoded = truce_sample_loader::decode_file(&fixture_path()).unwrap();
        assert_eq!(decoded.channels.len(), 1);
        assert_eq!(decoded.sample_rate, 48_000.0);
        assert_eq!(decoded.channels[0].len(), 4_800);

        let params = IrLoaderParams::new();
        let mut runner = truce_test::BlockRunner::<IrLoader>::new(&params).sample_rate(48_000.0);
        let input = [0.0f32; 64];
        let inputs: [&[f32]; 2] = [&input, &input];
        let events = EventList::with_capacity(0);

        *params.ir_file_path.write().unwrap() = fixture_path().display().to_string();
        runner.run(&params, &inputs, &events);
        let generation = runner.state().current_generation;
        assert_ne!(
            generation, 0,
            "live-target check should have claimed a generation"
        );
        assert!(runner.state().ir.is_empty(), "nothing to swap in yet");

        LoadIrRequest {
            path: fixture_path(),
            target_sample_rate: 48_000.0,
            generation,
        }
        .run(&params);

        runner.run(&params, &inputs, &events);
        assert_eq!(runner.state().ir.len(), 1);
        assert_eq!(runner.state().ir[0].len(), 4_800);
        assert!(params.loader.last_error.load().is_none());
    }

    /// A nonexistent path must not panic, must surface via the
    /// status cell, and must not allocate on the audio thread.
    #[test]
    fn missing_file_surfaces_error_no_panic() {
        use truce_test::assert_no_audio_alloc;

        let params = IrLoaderParams::new();
        let mut runner = truce_test::BlockRunner::<IrLoader>::new(&params).sample_rate(48_000.0);
        let input = [0.0f32; 64];
        let inputs: [&[f32]; 2] = [&input, &input];
        let events = EventList::with_capacity(0);

        *params.ir_file_path.write().unwrap() = "/nonexistent/path/does-not-exist.wav".to_string();
        let out = assert_no_audio_alloc(|| runner.run(&params, &inputs, &events));
        assert!(out.audio[0].iter().all(|s| s.is_finite()));
        let generation = runner.state().current_generation;
        assert_ne!(generation, 0);

        LoadIrRequest {
            path: PathBuf::from("/nonexistent/path/does-not-exist.wav"),
            target_sample_rate: 48_000.0,
            generation,
        }
        .run(&params);

        assert_eq!(params.loader.last_error.load(), Some(DecodeErrorCode::Io));
        runner.run(&params, &inputs, &events);
        assert!(
            runner.state().ir.is_empty(),
            "failed decode must not swap anything in"
        );
    }

    /// Only the newer generation's decode lands, even if the stale
    /// one arrives after.
    #[test]
    fn generation_race_keeps_only_newest() {
        let dir = tempfile::tempdir().unwrap();
        let stale_path = dir.path().join("stale.wav");
        let fresh_path = dir.path().join("fresh.wav");
        write_wav(&stale_path, 48_000, &vec![0.0; 2_400]);
        write_wav(&fresh_path, 48_000, &vec![0.0; 4_800]);

        let params = IrLoaderParams::new();
        let mut runner = truce_test::BlockRunner::<IrLoader>::new(&params).sample_rate(48_000.0);
        let input = [0.0f32; 64];
        let inputs: [&[f32]; 2] = [&input, &input];
        let events = EventList::with_capacity(0);

        *params.ir_file_path.write().unwrap() = stale_path.display().to_string();
        runner.run(&params, &inputs, &events);
        let gen_stale = runner.state().current_generation;

        *params.ir_file_path.write().unwrap() = fresh_path.display().to_string();
        runner.run(&params, &inputs, &events);
        let gen_fresh = runner.state().current_generation;
        assert_ne!(gen_stale, gen_fresh);
        assert!(
            runner.state().ir.is_empty(),
            "nothing should have swapped in yet"
        );

        // Stale result arrives after being superseded; must be rejected.
        LoadIrRequest {
            path: stale_path,
            target_sample_rate: 48_000.0,
            generation: gen_stale,
        }
        .run(&params);
        runner.run(&params, &inputs, &events);
        assert!(
            runner.state().ir.is_empty(),
            "stale decode must not have landed in state.ir"
        );

        LoadIrRequest {
            path: fresh_path,
            target_sample_rate: 48_000.0,
            generation: gen_fresh,
        }
        .run(&params);
        runner.run(&params, &inputs, &events);
        assert_eq!(runner.state().ir.len(), 1);
        assert_eq!(
            runner.state().ir[0].len(),
            4_800,
            "only the fresh decode should have landed"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/ir_loader_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/ir_loader_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/ir_loader_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
