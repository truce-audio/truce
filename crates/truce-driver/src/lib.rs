//! Headless driver for truce plugins.
//!
//! Instantiate a plugin, feed it scripted audio + events for a fixed
//! duration, capture the output. Used by:
//!
//! - **Tests** via [`truce-test`](../truce_test) — adds assertion
//!   helpers on top of the captured [`DriverResult`].
//! - **The standalone host's offline-render path** —
//!   `cargo truce run --no-playback` parses CLI flags into an
//!   [`InputSource::Buffer`] + [`Script`], runs [`PluginDriver`],
//!   writes the captured audio out as WAV.
//! - **Plugin authors writing custom `main.rs` bins** — batch CI
//!   renders, demo audio generation, preset rendering pipelines.
//!
//! No cpal, no midir, no live-audio plumbing. The driver does:
//!
//! 1. `P::create()` → `init()` → `reset()` → param `set_sample_rate`
//!    + `snap_smoothers`.
//! 2. Apply `state_file` bytes via `plugin.load_state(...)`.
//! 3. Run the `setup` closure (`&mut Plugin`).
//! 4. Loop blocks: pull script events into the block window, run
//!    `plugin.process(...)`, append the output.
//! 5. Capture meters / output events / per-block snapshots
//!    according to [`CaptureSpec`].
//!
//! See [`PluginDriver`] for the builder surface.
//!
//! ```ignore
//! use std::time::Duration;
//! use truce_driver::{InputSource, PluginDriver};
//!
//! let result = PluginDriver::<MyPlugin>::new()
//!     .sample_rate(48_000.0)
//!     .duration(Duration::from_secs(2))
//!     .input(InputSource::Constant(0.5))
//!     .set_param(MyParamId::Gain, 0.7)
//!     .script(|s| {
//!         s.note_on(60, 0.8);
//!         s.wait_ms(500);
//!         s.note_off(60);
//!     })
//!     .run();
//! ```

use std::path::PathBuf;
use std::time::Duration;

use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_core::process::ProcessContext;
use truce_params::Params;

// ---------------------------------------------------------------------------
// InputSource
// ---------------------------------------------------------------------------

/// What audio gets fed into the plugin's input bus each block.
///
/// `Silence` is the default. Effects with smoothers / lookahead /
/// modulators usually want one of the non-silent variants to reach
/// steady state during the run.
#[derive(Default)]
pub enum InputSource {
    /// Zero on every channel for the whole run.
    #[default]
    Silence,
    /// Constant DC: every sample is `value` on every channel.
    Constant(f32),
    /// Channel-major buffer (`bufs[ch][frame]`). Length must be
    /// `>= total_frames`; shorter buffers panic at run-time. The
    /// channel count must match the driver's `channels`.
    Buffer(Vec<Vec<f32>>),
    /// `(frame_idx, sample_rate) -> sample`. Same value goes into
    /// every channel. Useful for sweeps / noise / generators.
    Generator(Box<dyn FnMut(usize, f64) -> f32>),
}


// ---------------------------------------------------------------------------
// TransportSpec
// ---------------------------------------------------------------------------

/// Transport state visible to the plugin's `ProcessContext`.
#[derive(Clone)]
pub struct TransportSpec {
    pub bpm: f64,
    pub playing: bool,
    pub position_beats: f64,
    pub time_signature: (u8, u8),
}

impl Default for TransportSpec {
    fn default() -> Self {
        Self {
            bpm: 120.0,
            playing: false,
            position_beats: 0.0,
            time_signature: (4, 4),
        }
    }
}

// ---------------------------------------------------------------------------
// MeterCapture / CaptureSpec
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
#[derive(Default)]
pub enum MeterCapture {
    None,
    /// One snapshot at end-of-run.
    #[default]
    Final,
    /// One snapshot per process block (post-process).
    PerBlock,
}


#[derive(Clone, Copy, Default)]
pub struct CaptureSpec {
    /// Capture the rendered audio. Default true — turning it off
    /// means `DriverResult::output` is empty (use case: a meter-only
    /// run that doesn't care about audio).
    pub audio: bool,
    pub meters: MeterCapture,
    /// Capture events the plugin emits via `ProcessContext::output_events`.
    pub output_events: bool,
    /// Capture each block's `(param_id, plain_value)` map. Off by
    /// default; tests that need it opt in.
    pub block_snapshots: bool,
}

impl CaptureSpec {
    fn defaults() -> Self {
        Self {
            audio: true,
            meters: MeterCapture::Final,
            output_events: false,
            block_snapshots: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Script
// ---------------------------------------------------------------------------

/// Sample-accurate sequence of events fed to the plugin during a
/// run. Cursor advances via `wait_ms` / `wait_samples`; events
/// land at the current cursor position.
#[derive(Default, Clone)]
pub struct Script {
    /// `(sample_offset, body)` — sorted by offset on `run`.
    events: Vec<(usize, EventBody)>,
    cursor_samples: usize,
    sample_rate: f64,
}

impl Script {
    pub fn note_on(&mut self, note: u8, velocity: f32) {
        self.push(EventBody::NoteOn {
            channel: 0,
            note,
            velocity,
        });
    }

    pub fn note_off(&mut self, note: u8) {
        self.push(EventBody::NoteOff {
            channel: 0,
            note,
            velocity: 0.0,
        });
    }

    pub fn cc(&mut self, cc: u8, value: f32) {
        self.push(EventBody::ControlChange {
            channel: 0,
            cc,
            value,
        });
    }

    pub fn pitch_bend(&mut self, normalized: f32) {
        self.push(EventBody::PitchBend {
            channel: 0,
            value: normalized,
        });
    }

    pub fn channel_pressure(&mut self, value: f32) {
        self.push(EventBody::ChannelPressure {
            channel: 0,
            pressure: value,
        });
    }

    /// Set a parameter to a normalized [0.0, 1.0] value, sample-
    /// accurate at the cursor's offset. The plugin sees a
    /// `ParamChange` event in its event list — same delivery path
    /// CLAP / VST3 / AU automation lanes use.
    pub fn set_param(&mut self, id: impl Into<u32>, normalized: f32) {
        self.push(EventBody::ParamChange {
            id: id.into(),
            value: normalized as f64,
        });
    }

    /// Push an arbitrary `EventBody` at the current cursor — escape
    /// hatch for events `Script` doesn't have a typed helper for.
    pub fn raw(&mut self, body: EventBody) {
        self.push(body);
    }

    /// Advance the cursor by `ms` milliseconds at the run's sample
    /// rate. `wait_ms(0)` is a no-op (subsequent events land at the
    /// same offset). Resolves correctly only after `Script::sample_rate`
    /// is filled in by `PluginDriver::run` — call sites can rely on
    /// the driver wiring it before scanning the script.
    pub fn wait_ms(&mut self, ms: u64) {
        let sr = if self.sample_rate > 0.0 {
            self.sample_rate
        } else {
            44_100.0
        };
        self.cursor_samples += ((sr * ms as f64) / 1000.0) as usize;
    }

    /// Advance the cursor by `n` samples.
    pub fn wait_samples(&mut self, n: usize) {
        self.cursor_samples += n;
    }

    fn push(&mut self, body: EventBody) {
        self.events.push((self.cursor_samples, body));
    }
}

// ---------------------------------------------------------------------------
// DriverResult
// ---------------------------------------------------------------------------

/// Captured audio + metadata + plugin instance from a
/// [`PluginDriver`] run.
///
/// Holds the post-run plugin instance (`plugin: P`) so post-run
/// assertions can read params or custom state directly. As a side
/// effect, `DriverResult: !Send` whenever `P: !Send` — which is
/// true for plugins built via `truce::plugin!` (the generated
/// `Plugin` alias is `unsafe impl Send` only conditionally on its
/// inner `Params` type). Test code rarely cares; document if you
/// hit it.
pub struct DriverResult<P: PluginExport> {
    /// Channel-major output: `output[ch][frame]`. Empty when
    /// `CaptureSpec::audio == false`.
    pub output: Vec<Vec<f32>>,
    pub sample_rate: f64,
    pub block_size: usize,
    pub total_frames: usize,

    /// Final-or-per-block meter readings.
    pub meters: MeterReadings,

    /// Output events emitted by the plugin. Offsets are absolute
    /// (cumulative across blocks). Empty unless
    /// `CaptureSpec::output_events`.
    pub output_events: Vec<Event>,

    /// Per-block param snapshots (one Vec per block), each entry
    /// `(param_id, plain_value)`. Empty unless
    /// `CaptureSpec::block_snapshots`.
    pub block_snapshots: Vec<Vec<(u32, f64)>>,

    /// Post-run plugin instance. Read params or custom state from
    /// here when writing assertions over the final state.
    pub plugin: P,
}

#[derive(Default)]
pub enum MeterReadings {
    #[default]
    None,
    Final(Vec<(u32, f32)>),
    PerBlock(Vec<Vec<(u32, f32)>>),
}

#[cfg(feature = "wav")]
impl<P: PluginExport> DriverResult<P> {
    /// Write the captured audio as a 32-bit float WAV. Available
    /// when the `wav` feature is enabled. Convenience shim around
    /// `hound`; if you need a different sample format, drive `hound`
    /// yourself off `result.output` / `result.sample_rate`.
    pub fn write_wav(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        if self.output.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "no audio captured (CaptureSpec::audio was false)",
            ));
        }
        let spec = hound::WavSpec {
            channels: self.output.len() as u16,
            sample_rate: self.sample_rate as u32,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut wav = hound::WavWriter::create(path, spec).map_err(io_err)?;
        for frame in 0..self.total_frames {
            for ch in &self.output {
                wav.write_sample(ch[frame]).map_err(io_err)?;
            }
        }
        wav.finalize().map_err(io_err)?;
        Ok(())
    }
}

#[cfg(feature = "wav")]
fn io_err(e: hound::Error) -> std::io::Error {
    std::io::Error::other(e)
}

// ---------------------------------------------------------------------------
// PluginDriver builder
// ---------------------------------------------------------------------------

type SetupFn<P> = Box<dyn FnOnce(&mut P)>;

pub struct PluginDriver<P: PluginExport> {
    sample_rate: f64,
    channels: Option<usize>,
    block_size: usize,
    duration: Duration,

    transport: TransportSpec,
    input: InputSource,
    script: Script,

    /// `.pluginstate` bytes loaded after init/reset, before
    /// `set_param` shortcuts and `setup` run. See "State files"
    /// section of the design doc for lifecycle ordering.
    state_bytes: Option<Vec<u8>>,
    /// Manifest dir for `state_file` path resolution. Set by callers
    /// that pass a relative path; absolute paths bypass.
    manifest_dir: PathBuf,
    /// `.set_param(id, v)` shortcuts — applied after state load,
    /// before the `setup` closure.
    param_overrides: Vec<(u32, f32)>,
    /// `&mut P` closure run after state load + param overrides.
    setup: Option<SetupFn<P>>,

    capture: CaptureSpec,
    _marker: std::marker::PhantomData<P>,
}

impl<P: PluginExport> Default for PluginDriver<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: PluginExport> PluginDriver<P> {
    pub fn new() -> Self {
        Self {
            sample_rate: 44_100.0,
            channels: None,
            block_size: 512,
            duration: Duration::from_secs(1),
            transport: TransportSpec::default(),
            input: InputSource::Silence,
            script: Script::default(),
            state_bytes: None,
            manifest_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            param_overrides: Vec::new(),
            setup: None,
            capture: CaptureSpec::defaults(),
            _marker: std::marker::PhantomData,
        }
    }

    pub fn sample_rate(mut self, sr: f64) -> Self {
        self.sample_rate = sr;
        self
    }
    pub fn channels(mut self, n: usize) -> Self {
        self.channels = Some(n);
        self
    }
    pub fn block_size(mut self, n: usize) -> Self {
        self.block_size = n;
        self
    }
    pub fn duration(mut self, d: Duration) -> Self {
        self.duration = d;
        self
    }

    pub fn transport(mut self, t: TransportSpec) -> Self {
        self.transport = t;
        self
    }
    pub fn bpm(mut self, bpm: f64) -> Self {
        self.transport.bpm = bpm;
        self
    }
    pub fn playing(mut self, playing: bool) -> Self {
        self.transport.playing = playing;
        self
    }

    pub fn input(mut self, source: InputSource) -> Self {
        self.input = source;
        self
    }

    /// Build a script via a closure. Each `set_param` / `note_on`
    /// / etc. lands at the cursor's current sample offset; `wait_ms`
    /// advances the cursor.
    pub fn script(mut self, f: impl FnOnce(&mut Script)) -> Self {
        // The script's sample rate is wired in `run` (so `wait_ms`
        // resolves against the actual run SR). Set it provisionally
        // here so build-time `wait_ms` calls work even when the
        // user reads the cursor immediately.
        self.script.sample_rate = self.sample_rate;
        f(&mut self.script);
        self
    }

    /// Set a parameter to a normalized [0, 1] value before the run
    /// starts. Equivalent to a `setup(|p| p.params().set_normalized(id, v))`
    /// closure but written as one builder call. Multiple `.set_param`
    /// calls compose; they run in declaration order, before the
    /// `.setup` closure (if any).
    ///
    /// For automation *during* a run, use `.script(|s| s.set_param(...))`
    /// — that emits a sample-accurate `ParamChange` event the plugin
    /// processes inline.
    pub fn set_param(mut self, id: impl Into<u32>, normalized: f32) -> Self {
        self.param_overrides.push((id.into(), normalized));
        self
    }

    /// Anchor for `state_file` relative paths. Defaults to the
    /// process CWD; callers from `truce-test` override it with the
    /// test crate's `CARGO_MANIFEST_DIR` via the `screenshot!`-style
    /// macro pattern (see `truce-test`'s wrapping macro).
    pub fn manifest_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.manifest_dir = dir.into();
        self
    }

    /// Mutate the plugin between init/reset+state-load and the
    /// first process block. Use when the test needs more than
    /// param tweaks — load arbitrary fields, drive a warmup
    /// `process()` call to populate meters / lookahead, etc.
    ///
    /// Composes with `state_file` (state loads first) and
    /// `set_param` (shortcuts apply first); the closure runs last.
    pub fn setup<F: FnOnce(&mut P) + 'static>(mut self, f: F) -> Self {
        self.setup = Some(Box::new(f));
        self
    }

    /// Read a `.pluginstate` file (the standalone host's `Cmd+S`
    /// save format) and apply it via `plugin.load_state(&bytes)`
    /// after init/reset and before any `set_param` overrides /
    /// `setup` closure. Path is resolved relative to
    /// `manifest_dir`, or used as-is if absolute.
    pub fn state_file(mut self, path: impl Into<PathBuf>) -> Self {
        let raw = path.into();
        let resolved = if raw.is_absolute() {
            raw
        } else {
            self.manifest_dir.join(&raw)
        };
        let bytes = std::fs::read(&resolved)
            .unwrap_or_else(|e| panic!("state_file: failed to read {}: {e}", resolved.display()));
        self.state_bytes = Some(bytes);
        self
    }

    pub fn capture_audio(mut self, on: bool) -> Self {
        self.capture.audio = on;
        self
    }
    pub fn capture_meters(mut self, m: MeterCapture) -> Self {
        self.capture.meters = m;
        self
    }
    pub fn capture_output_events(mut self, on: bool) -> Self {
        self.capture.output_events = on;
        self
    }
    pub fn capture_block_snapshots(mut self, on: bool) -> Self {
        self.capture.block_snapshots = on;
        self
    }

    /// Drive the plugin and return the captured result.
    pub fn run(mut self) -> DriverResult<P> {
        // Build + activate.
        let mut plugin = P::create();
        plugin.init();
        plugin.reset(self.sample_rate, self.block_size);
        plugin.params().set_sample_rate(self.sample_rate);
        plugin.params().snap_smoothers();

        // 1. State load (if any).
        if let Some(bytes) = self.state_bytes.as_deref() {
            plugin.load_state(bytes);
        }

        // 2. Param overrides (the `.set_param(...)` shortcuts).
        for (id, value) in &self.param_overrides {
            plugin.params().set_normalized(*id, *value as f64);
        }
        plugin.params().snap_smoothers();

        // 3. Setup closure (most general).
        if let Some(f) = self.setup.take() {
            f(&mut plugin);
        }

        // Resolve channel count.
        let channels = self.channels.unwrap_or_else(|| {
            let layouts = P::bus_layouts();
            let layout = &layouts[0];
            let outs = layout.total_output_channels() as usize;
            if outs > 0 { outs } else { 2 }
        });

        let is_effect = P::info().category == PluginCategory::Effect;
        let total_frames = (self.duration.as_secs_f64() * self.sample_rate) as usize;

        // Capture buffers.
        let mut output: Vec<Vec<f32>> = if self.capture.audio {
            (0..channels)
                .map(|_| Vec::with_capacity(total_frames))
                .collect()
        } else {
            Vec::new()
        };
        let mut output_events_capture: Vec<Event> = Vec::new();
        let mut per_block_meters: Vec<Vec<(u32, f32)>> = Vec::new();
        let mut block_snapshots: Vec<Vec<(u32, f64)>> = Vec::new();

        // Pre-resolve input source into per-block chunks. For
        // Buffer / Generator we lazy-fill per block; for Constant
        // we just produce a single fill-value to broadcast.
        let constant_value: Option<f32> = match &self.input {
            InputSource::Constant(v) => Some(*v),
            InputSource::Silence => Some(0.0),
            _ => None,
        };

        // Wire script SR + sort once.
        self.script.sample_rate = self.sample_rate;
        self.script.events.sort_by_key(|(off, _)| *off);
        let script_events = self.script.events;

        // Transport tracker.
        let mut transport_pos_beats = self.transport.position_beats;
        let beats_per_second = self.transport.bpm / 60.0;

        let meter_ids: Vec<u32> = plugin.params().meter_ids().into_iter().collect();

        let mut cursor = 0usize;
        while cursor < total_frames {
            let block_len = self.block_size.min(total_frames - cursor);

            // Pull events that fall inside [cursor, cursor+block_len).
            let mut event_list = EventList::new();
            for (off, body) in &script_events {
                if *off >= cursor && *off < cursor + block_len {
                    event_list.push(Event {
                        sample_offset: (*off - cursor) as u32,
                        body: body.clone(),
                    });
                }
            }

            // Allocate per-block channel buffers.
            let mut out_bufs: Vec<Vec<f32>> =
                (0..channels).map(|_| vec![0.0f32; block_len]).collect();

            // Build input slices.
            let in_bufs_owned: Vec<Vec<f32>> = if !is_effect {
                Vec::new()
            } else {
                match (&mut self.input, constant_value) {
                    (_, Some(v)) => (0..channels).map(|_| vec![v; block_len]).collect(),
                    (InputSource::Buffer(bufs), _) => {
                        assert_eq!(
                            bufs.len(),
                            channels,
                            "InputSource::Buffer channel count {} doesn't match driver channels {channels}",
                            bufs.len(),
                        );
                        bufs.iter()
                            .map(|ch| {
                                let end = (cursor + block_len).min(ch.len());
                                let start = cursor.min(ch.len());
                                let mut block = ch[start..end].to_vec();
                                block.resize(block_len, 0.0);
                                block
                            })
                            .collect()
                    }
                    (InputSource::Generator(g), _) => (0..channels)
                        .map(|_| {
                            (0..block_len)
                                .map(|i| g(cursor + i, self.sample_rate))
                                .collect()
                        })
                        .collect(),
                    _ => unreachable!(),
                }
            };
            let in_slices: Vec<&[f32]> = in_bufs_owned.iter().map(|b| b.as_slice()).collect();
            let mut out_slices: Vec<&mut [f32]> =
                out_bufs.iter_mut().map(|b| b.as_mut_slice()).collect();
            let mut audio =
                unsafe { AudioBuffer::from_slices(&in_slices, &mut out_slices, block_len) };

            // Transport snapshot for this block.
            let transport_info = TransportInfo {
                playing: self.transport.playing,
                tempo: self.transport.bpm,
                time_sig_num: self.transport.time_signature.0,
                time_sig_den: self.transport.time_signature.1,
                position_seconds: cursor as f64 / self.sample_rate,
                position_beats: transport_pos_beats,
                bar_start_beats: 0.0,
                ..Default::default()
            };
            let mut output_events_block = EventList::new();
            let mut ctx = ProcessContext::new(
                &transport_info,
                self.sample_rate,
                block_len,
                &mut output_events_block,
            );

            plugin.process(&mut audio, &event_list, &mut ctx);

            // Capture audio.
            if self.capture.audio {
                for (ch, buf) in out_bufs.into_iter().enumerate() {
                    output[ch].extend_from_slice(&buf);
                }
            }

            // Capture output events with absolute offsets.
            if self.capture.output_events {
                for ev in output_events_block.iter() {
                    let mut e = ev.clone();
                    e.sample_offset += cursor as u32;
                    output_events_capture.push(e);
                }
            }

            // Capture per-block meters / param snapshots.
            if matches!(self.capture.meters, MeterCapture::PerBlock) {
                per_block_meters.push(
                    meter_ids
                        .iter()
                        .map(|id| (*id, plugin.get_meter(*id)))
                        .collect(),
                );
            }
            if self.capture.block_snapshots {
                let infos = plugin.params().param_infos();
                block_snapshots.push(
                    infos
                        .iter()
                        .map(|pi| (pi.id, plugin.params().get_plain(pi.id).unwrap_or(0.0)))
                        .collect(),
                );
            }

            // Advance transport.
            if self.transport.playing {
                let block_seconds = block_len as f64 / self.sample_rate;
                transport_pos_beats += block_seconds * beats_per_second;
            }

            cursor += block_len;
        }

        let meters = match self.capture.meters {
            MeterCapture::None => MeterReadings::None,
            MeterCapture::Final => MeterReadings::Final(
                meter_ids
                    .iter()
                    .map(|id| (*id, plugin.get_meter(*id)))
                    .collect(),
            ),
            MeterCapture::PerBlock => MeterReadings::PerBlock(per_block_meters),
        };

        DriverResult {
            output,
            sample_rate: self.sample_rate,
            block_size: self.block_size,
            total_frames,
            meters,
            output_events: output_events_capture,
            block_snapshots,
            plugin,
        }
    }
}
