//! Shared cpal audio setup + callback. One implementation used by
//! both `windowed` and `headless` runners.
//!
//! Both the input and output cpal streams are owned by dedicated
//! worker threads (cpal `Stream` is `!Send` on macOS, so it can't
//! cross threads). UI / menu / CLI callers manipulate them through
//! `Send + Sync` controllers (`InputController`, `OutputController`)
//! that talk to the workers via `mpsc` channels:
//!
//! - **Toggle / enable** for input (drop the cpal input stream when
//!   off — saves CPU and skips the OS mic permission prompt) and
//!   output (mute — keep the stream open so processing keeps ticking,
//!   just zero-fill the speaker buffer).
//! - **Switch device** for either side. Worker drops the old stream
//!   and opens a new one against the requested device name; on
//!   failure the previous device's name remains in place and the
//!   audio callback keeps running unchanged.

use std::mem::transmute;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use truce_core::buffer::AudioBuffer;
use truce_core::cast::{sample_count_usize, sample_rate_u32};
use truce_core::events::{EVENT_LIST_PREALLOC, Event, EventBody, EventList};
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_core::process::ProcessContext;
use truce_params::Params;

use crate::cli::Options;
use crate::transport::Transport;
use crate::vlog;

type BoxErr = Box<dyn std::error::Error>;

/// A queued MIDI event the UI thread hands off to the audio callback.
pub struct MidiEvent {
    pub body: EventBody,
}

/// Shared audio-thread resources handed back from `start_audio`.
///
/// The output stream is owned by a worker thread, not by this
/// struct, so `AudioHandles` is fully `Send`. The worker exits
/// when the controller channel closes (all `OutputController`
/// clones dropped).
pub struct AudioHandles<P: PluginExport> {
    /// Event queue the caller pushes MIDI into; drained by the audio
    /// callback each block.
    pub pending: Arc<Mutex<Vec<MidiEvent>>>,
    /// Plugin instance shared between caller and audio callback.
    pub plugin: Arc<Mutex<P>>,
    /// Audio config (sample rate, channels) resolved from the device.
    pub sample_rate: f64,
    pub channels: usize,
    pub is_effect: bool,
    /// `Send + Sync` handle for toggling mic input and switching
    /// the input device. Backed by a worker thread that owns the
    /// (`!Send`) cpal input stream.
    pub input: InputController,
    /// `Send + Sync` handle for switching the output device.
    /// Backed by a worker thread that owns the (`!Send`) cpal
    /// output stream.
    pub output: OutputController,
    /// Shared transport state; UI thread toggles play/stop, audio
    /// thread advances position each block.
    pub transport: Transport,
    /// Live-mode `--input-file` source (gated on the `playback`
    /// feature). Exposed so the runner can poll
    /// `playback.is_eof()` to drive clean shutdown when paired
    /// with `--output-file`.
    #[cfg(feature = "playback")]
    pub playback: Option<Arc<crate::playback::PlaybackSource>>,
    /// Live-mode `--output-file` capture sink. The runner calls
    /// `take_capture().finalize()` on its way out so the WAV
    /// header gets the correct sample count.
    #[cfg(feature = "playback")]
    pub capture: Option<crate::playback::CaptureSink>,
}

// ---------------------------------------------------------------------------
// InputController
// ---------------------------------------------------------------------------

/// `Send + Sync` handle for managing mic input from the UI thread.
///
/// Cloneable; multiple holders can request toggles or device
/// switches. The actual `cpal::Stream` (`!Send` on macOS) lives on
/// a dedicated worker thread spawned by `start_audio`.
#[derive(Clone)]
pub struct InputController {
    /// Audio callback reads this every block to decide whether to
    /// drain the input ring or zero-fill. Worker thread updates it
    /// when a toggle completes (or fails).
    pub enabled: Arc<AtomicBool>,
    /// True if an input device is configured (default or
    /// CLI-named). When false, toggling on is a no-op.
    pub has_device: bool,
    /// Sender for input commands. Worker blocks on the matching
    /// receiver. Closing the channel exits the worker.
    cmd_tx: mpsc::Sender<InputCmd>,
    /// Worker mirrors the resolved device name here after each
    /// open so the menu can render a checkmark on the active
    /// device. `None` = default device or no device available.
    current_name: Arc<Mutex<Option<String>>>,
}

enum InputCmd {
    SetEnabled(bool),
    SetDevice(Option<String>),
}

impl InputController {
    /// Toggle the input. Returns immediately; the worker thread
    /// processes the request asynchronously.
    pub fn set_enabled(&self, on: bool) {
        let _ = self.cmd_tx.send(InputCmd::SetEnabled(on));
    }

    /// Read the current state. Source of truth for the audio
    /// callback's zero-fill decision.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Switch the input device by name. Pass `None` to fall back
    /// to the system default. If the input is currently enabled,
    /// the worker re-opens the stream against the new device; if
    /// disabled, the change takes effect on the next enable.
    pub fn set_device(&self, name: Option<String>) {
        let _ = self.cmd_tx.send(InputCmd::SetDevice(name));
    }

    /// Currently-resolved input device name, or `None` if no
    /// device has been opened (or the worker resolved to the
    /// system default with no nameable device).
    #[must_use]
    pub fn current_name(&self) -> Option<String> {
        self.current_name.lock().ok().and_then(|g| g.clone())
    }
}

// ---------------------------------------------------------------------------
// OutputController
// ---------------------------------------------------------------------------

/// `Send + Sync` handle for managing the output device from the
/// UI thread. Cloneable; clones share the worker.
#[derive(Clone)]
pub struct OutputController {
    /// Audio callback reads this every block to decide whether to
    /// zero-fill the output buffer (mute). Worker thread (and direct
    /// callers via `set_enabled`) update it.
    pub enabled: Arc<AtomicBool>,
    cmd_tx: mpsc::Sender<OutputCmd>,
    current_name: Arc<Mutex<Option<String>>>,
}

enum OutputCmd {
    SetDevice(Option<String>),
}

impl OutputController {
    /// Mute / unmute the output. The cpal stream stays open either
    /// way — disabling just makes the audio callback zero-fill its
    /// buffer, so the plugin keeps processing (transport ticks,
    /// MIDI is consumed) while the speakers are silent.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// Read the current mute state. Source of truth for the audio
    /// callback's zero-fill decision.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Switch the output device by name. Pass `None` to fall back
    /// to the system default. Failure to open is logged but
    /// non-fatal — the previous stream remains running.
    pub fn set_device(&self, name: Option<String>) {
        let _ = self.cmd_tx.send(OutputCmd::SetDevice(name));
    }

    /// Currently-resolved output device name, or `None` if not
    /// resolvable.
    #[must_use]
    pub fn current_name(&self) -> Option<String> {
        self.current_name.lock().ok().and_then(|g| g.clone())
    }
}

// ---------------------------------------------------------------------------
// Device enumeration helpers (used by `--list-devices` + menus).
// ---------------------------------------------------------------------------

/// Print available audio devices and return. Used by `--list-devices`.
pub fn list_devices() {
    println!("Audio devices");
    println!("Output:");
    let (default_out, outs) = enumerate_devices(true);
    for name in outs {
        let marker = if default_out.as_deref() == Some(name.as_str()) {
            " (default)"
        } else {
            ""
        };
        println!("  {name}{marker}");
    }
    println!("Input:");
    let (default_in, ins) = enumerate_devices(false);
    for name in ins {
        let marker = if default_in.as_deref() == Some(name.as_str()) {
            " (default)"
        } else {
            ""
        };
        println!("  {name}{marker}");
    }
}

/// Snapshot of available output devices (`(default_name, all_names)`).
#[must_use]
pub fn list_output_devices() -> (Option<String>, Vec<String>) {
    enumerate_devices(true)
}

/// Snapshot of available input devices (`(default_name, all_names)`).
#[must_use]
pub fn list_input_devices() -> (Option<String>, Vec<String>) {
    enumerate_devices(false)
}

fn enumerate_devices(output: bool) -> (Option<String>, Vec<String>) {
    let host = cpal::default_host();
    let default_name = if output {
        host.default_output_device().and_then(|d| d.name().ok())
    } else {
        host.default_input_device().and_then(|d| d.name().ok())
    };
    let names = if output {
        host.output_devices()
            .map(|it| it.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    } else {
        host.input_devices()
            .map(|it| it.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    };
    (default_name, names)
}

// ---------------------------------------------------------------------------
// start_audio
// ---------------------------------------------------------------------------

/// Resolve devices, instantiate the plugin, spawn input + output
/// worker threads, return the handles the caller needs to push MIDI
/// and reach the workers.
///
/// # Errors
///
/// Returns an error if the requested input/output device can't be
/// found (or no default exists), the device's default stream config
/// can't be queried, or any of the cpal stream-build calls fail.
pub fn start_audio<P: PluginExport>(opts: &Options) -> Result<AudioHandles<P>, BoxErr> {
    let audio_host = cpal::default_host();

    // Resolve initial output device synchronously so we can pull
    // its default config (sample rate, channels) before spawning
    // the worker. The worker re-resolves by name on each switch.
    let initial_output = match &opts.output_device {
        Some(name) => find_device(&audio_host, name, true).ok_or_else(|| {
            format!(
                "no output device matching '{name}'. \
                 Run with --list-devices to see available outputs."
            )
        })?,
        None => audio_host.default_output_device().ok_or(
            "no default audio output device. \
             Plug in or enable an output, then retry.",
        )?,
    };

    let default_config = initial_output
        .default_output_config()
        .map_err(|e| format!("could not query default config for the audio output: {e}"))?;

    let config: cpal::StreamConfig = resolve_config(&initial_output, &default_config, opts);
    let sample_format = default_config.sample_format();
    let sample_rate = f64::from(config.sample_rate.0);
    let channels = config.channels as usize;
    let is_effect = P::info().category == PluginCategory::Effect;

    let pending: Arc<Mutex<Vec<MidiEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let plugin = Arc::new(Mutex::new({
        let mut p = P::create();
        p.init();
        p.reset(sample_rate, config.buffer_size_max_frames());
        p.params().set_sample_rate(sample_rate);
        // Apply `--state <path>` BEFORE snapping smoothers so the
        // first audio block sees the restored values, not defaults
        // ramping toward them.
        if let Some(path) = opts.state_path.as_deref() {
            crate::state::load_into(&mut p, path);
        }
        p.params().snap_smoothers();
        p
    }));

    let input_setup = setup_input_pipeline(&audio_host, opts, is_effect, channels, sample_rate);
    let input_ring = input_setup.ring;
    let input_enabled = input_setup.enabled;
    let input_controller = input_setup.controller;

    let transport = Transport::new(opts.bpm.unwrap_or(120.0), sample_rate);

    // Initial output device name (may differ from the resolver's
    // requested name if it matched by substring).
    let initial_output_name = initial_output.name().ok();
    let output_current_name = Arc::new(Mutex::new(initial_output_name.clone()));
    let (output_cmd_tx, output_cmd_rx) = mpsc::channel::<OutputCmd>();
    let (open_result_tx, open_result_rx) = mpsc::channel::<Result<(), String>>();

    // Output defaults to enabled — the user launched standalone to
    // hear the plugin. `--output-enabled off` (or the config file)
    // can flip the launch state.
    let output_enabled = Arc::new(AtomicBool::new(opts.output_enabled.unwrap_or(true)));

    let output_controller = OutputController {
        enabled: Arc::clone(&output_enabled),
        cmd_tx: output_cmd_tx,
        current_name: Arc::clone(&output_current_name),
    };

    // Decode `--input-file` (if set) once at startup against the
    // resolved device sample-rate / channel-count. Hard error on
    // unreadable / unparseable file — we fail noisily here rather
    // than letting the audio worker silently emit zeros.
    #[cfg(feature = "playback")]
    let playback = match &opts.input_file {
        Some(path) if is_effect => {
            let src = crate::playback::PlaybackSource::from_wav(path, sample_rate, channels)?;
            vlog!(
                "Playback: {} → input bus (one-shot, sums with mic when enabled)",
                path.display()
            );
            Some(Arc::new(src))
        }
        Some(_) => {
            // Instrument plugins have no input bus to feed; warn
            // and ignore rather than failing.
            eprintln!("--input-file ignored: plugin is not an effect");
            None
        }
        None => None,
    };

    // `--output-file` capture sink. Created here so any
    // filesystem error (missing parent dir, unwritable target,
    // …) propagates back to the runner before audio starts.
    #[cfg(feature = "playback")]
    let capture = match &opts.output_file {
        Some(path) => {
            let sink = crate::playback::CaptureSink::create(path, sample_rate, channels)?;
            vlog!(
                "Capture: {} ({} Hz, {} ch, f32) — pre-mute output",
                path.display(),
                sample_rate,
                channels,
            );
            Some(sink)
        }
        None => None,
    };

    let res = OutputResources {
        plugin: Arc::clone(&plugin),
        pending: Arc::clone(&pending),
        input_ring: Arc::clone(&input_ring),
        input_enabled: Arc::clone(&input_enabled),
        output_enabled: Arc::clone(&output_enabled),
        transport: transport.clone(),
        current_name: Arc::clone(&output_current_name),
        #[cfg(feature = "playback")]
        playback: playback.clone(),
        #[cfg(feature = "playback")]
        capture: capture.as_ref().map(super::playback::CaptureSink::pusher),
    };

    let initial_output_name_for_worker = initial_output_name.clone();
    std::thread::Builder::new()
        .name("truce-standalone-output".into())
        .spawn(move || {
            output_worker::<P>(
                output_cmd_rx,
                open_result_tx,
                initial_output_name_for_worker,
                config,
                sample_format,
                sample_rate,
                channels,
                is_effect,
                res,
            );
        })
        .map_err(|e| format!("could not spawn output worker: {e}"))?;

    // Wait for the worker to confirm initial open, so any error
    // propagates back to start_audio's caller (matches the
    // pre-refactor synchronous behavior).
    match open_result_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e.into()),
        Err(e) => return Err(format!("output worker exited before reporting: {e}").into()),
    }

    if !output_enabled.load(Ordering::Relaxed) {
        vlog!(
            "Output: muted at launch — toggle from the Plugin menu or \
             pass --output-enabled on"
        );
    }

    Ok(AudioHandles {
        pending,
        plugin,
        sample_rate,
        channels,
        is_effect,
        input: input_controller,
        output: output_controller,
        transport,
        #[cfg(feature = "playback")]
        playback,
        #[cfg(feature = "playback")]
        capture,
    })
}

/// State produced by [`setup_input_pipeline`] that `start_audio`
/// needs to wire into the output worker (`ring` / `enabled` go into
/// `OutputResources`) and the public handles (`controller`).
struct InputSetup {
    controller: InputController,
    ring: Arc<Mutex<Vec<f32>>>,
    enabled: Arc<AtomicBool>,
}

/// Resolve the initial input device, allocate the input ring + control
/// channels, and (for effects) spawn the input worker thread. Also
/// flips `set_enabled(true)` when the user passed
/// `--input-enabled on`, so the launch state matches the CLI ask.
fn setup_input_pipeline(
    audio_host: &cpal::Host,
    opts: &Options,
    is_effect: bool,
    channels: usize,
    sample_rate: f64,
) -> InputSetup {
    let input_ring: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));

    // Resolve the initial input device name so the menu can show
    // the currently-active device on first open. Worker re-resolves
    // on each open against this name (or whatever the user picks).
    let initial_input_name: Option<String> = if is_effect {
        let device = match &opts.input_device {
            Some(name) => find_device(audio_host, name, false),
            None => audio_host.default_input_device(),
        };
        let name = device.and_then(|d| d.name().ok());
        if name.is_none() {
            eprintln!("Note: no input device found — input-enable will be a no-op.");
        }
        name
    } else {
        None
    };

    let input_enabled = Arc::new(AtomicBool::new(false));
    let has_input_device = initial_input_name.is_some();
    let (input_cmd_tx, input_cmd_rx) = mpsc::channel::<InputCmd>();
    let input_current_name = Arc::new(Mutex::new(initial_input_name.clone()));

    let controller = InputController {
        enabled: Arc::clone(&input_enabled),
        has_device: has_input_device,
        cmd_tx: input_cmd_tx,
        current_name: Arc::clone(&input_current_name),
    };

    if is_effect {
        let device_name = initial_input_name.clone();
        let ring = Arc::clone(&input_ring);
        let enabled_flag = Arc::clone(&input_enabled);
        let current = Arc::clone(&input_current_name);
        std::thread::Builder::new()
            .name("truce-standalone-input".into())
            .spawn(move || {
                input_worker(
                    input_cmd_rx,
                    device_name,
                    channels,
                    sample_rate,
                    ring,
                    enabled_flag,
                    current,
                );
            })
            .ok();
    }

    let want_input_enabled = is_effect && opts.input_enabled.unwrap_or(false);
    if want_input_enabled {
        controller.set_enabled(true);
    }

    if is_effect {
        vlog!(
            "Input:  {} ({})",
            initial_input_name.as_deref().unwrap_or("(none)"),
            if want_input_enabled {
                "enabled"
            } else {
                {
                    #[cfg(target_os = "macos")]
                    {
                        "disabled — press Cmd+I in the window or pass --input-enabled on"
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        "disabled — press Ctrl+I in the window or pass --input-enabled on"
                    }
                }
            }
        );
    }

    InputSetup {
        controller,
        ring: input_ring,
        enabled: input_enabled,
    }
}

// ---------------------------------------------------------------------------
// Output worker
// ---------------------------------------------------------------------------

/// Resources the output callback needs. Held by the worker; new
/// streams clone the inner Arcs so `process()` keeps seeing the
/// same plugin / pending / transport state across device switches.
struct OutputResources<P: PluginExport> {
    plugin: Arc<Mutex<P>>,
    pending: Arc<Mutex<Vec<MidiEvent>>>,
    input_ring: Arc<Mutex<Vec<f32>>>,
    input_enabled: Arc<AtomicBool>,
    /// Drives the audio callback's mute / unmute decision (UI thread
    /// flips it via `OutputController::set_enabled`).
    output_enabled: Arc<AtomicBool>,
    transport: Transport,
    current_name: Arc<Mutex<Option<String>>>,
    /// Optional `.wav` playback source (gated on the `playback`
    /// feature). When present, summed into the input bus alongside
    /// the mic ring — see the matrix in `cli.rs::HELP`.
    #[cfg(feature = "playback")]
    playback: Option<Arc<crate::playback::PlaybackSource>>,
    /// Optional `--output-file` capture pusher. Cloned into each
    /// cpal callback closure, so device switches don't tear down
    /// the capture. The owning `CaptureSink` lives on
    /// `AudioHandles`; finalize is the runner's responsibility.
    #[cfg(feature = "playback")]
    capture: Option<crate::playback::CapturePusher>,
}

// Spawned-thread body — owns its state across the worker's lifetime.
// Switching to refs would force the caller to outlive the thread.
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
fn output_worker<P: PluginExport>(
    cmd_rx: mpsc::Receiver<OutputCmd>,
    open_result: mpsc::Sender<Result<(), String>>,
    initial_device_name: Option<String>,
    config: cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    sample_rate: f64,
    channels: usize,
    is_effect: bool,
    res: OutputResources<P>,
) {
    let mut stream: Option<cpal::Stream> = None;

    let initial = open_output_stream::<P>(
        initial_device_name.as_deref(),
        &config,
        sample_format,
        sample_rate,
        channels,
        is_effect,
        &res,
        &mut stream,
    );
    let _ = open_result.send(initial);

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            OutputCmd::SetDevice(name) => {
                // Drop the old stream BEFORE building the new one
                // — some backends won't open a second exclusive
                // stream against the same device.
                stream = None;
                if let Err(e) = open_output_stream::<P>(
                    name.as_deref(),
                    &config,
                    sample_format,
                    sample_rate,
                    channels,
                    is_effect,
                    &res,
                    &mut stream,
                ) {
                    eprintln!("output device switch failed: {e}");
                }
            }
        }
    }
    drop(stream);
}

#[allow(clippy::too_many_arguments)]
fn open_output_stream<P: PluginExport>(
    name: Option<&str>,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    sample_rate: f64,
    channels: usize,
    is_effect: bool,
    res: &OutputResources<P>,
    stream_slot: &mut Option<cpal::Stream>,
) -> Result<(), String> {
    // Resolve fresh each open — hot-plug may have changed the
    // device list since the last switch.
    let host = cpal::default_host();
    let device = match name {
        Some(n) => {
            find_device(&host, n, true).ok_or_else(|| format!("no output device matching '{n}'"))?
        }
        None => host
            .default_output_device()
            .ok_or_else(|| "no default audio output device".to_string())?,
    };
    let resolved_name = device.name().ok();

    let plugin_a = Arc::clone(&res.plugin);
    let pending_a = Arc::clone(&res.pending);
    let ring_a = Arc::clone(&res.input_ring);
    let enabled_a = Arc::clone(&res.input_enabled);
    let out_enabled_a = Arc::clone(&res.output_enabled);
    let transport_a = res.transport.clone();
    #[cfg(feature = "playback")]
    let playback_a = res.playback.clone();
    #[cfg(feature = "playback")]
    let capture_a = res.capture.clone();

    // Per-stream audio-callback scratch. Owned by the move-closure so
    // it lives across callbacks but never crosses threads — cpal calls
    // the closure on a single dedicated audio thread per stream.
    // Amortizes the `vec![0.0; num_frames]` per-channel allocation and
    // the `channel_bufs.clone()` for the effect input mirror, plus the
    // two `EventList::default()`s per block (input drain + plugin output)
    // — both `clear()`ed and reused, capacity-preserving.
    let mut channel_bufs: Vec<Vec<f32>> = Vec::with_capacity(channels);
    let mut input_bufs: Vec<Vec<f32>> = Vec::with_capacity(channels);
    let mut event_list = EventList::with_capacity(EVENT_LIST_PREALLOC);
    let mut output_events = EventList::with_capacity(EVENT_LIST_PREALLOC);
    // Slice scratch reused across audio_callback invocations: each
    // block clears and re-extends rather than allocating a fresh Vec.
    // Lifetimes are laundered to `'static` via transmute inside the
    // callback; the scratches are read-then-cleared within each
    // callback invocation, so the false `'static` never escapes.
    let mut input_slices_scratch: Vec<&'static [f32]> = Vec::with_capacity(channels);
    let mut output_slices_scratch: Vec<&'static mut [f32]> = Vec::with_capacity(channels);

    let stream = match sample_format {
        cpal::SampleFormat::F32 => device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    audio_callback::<P>(
                        data,
                        channels,
                        sample_rate,
                        is_effect,
                        &plugin_a,
                        &pending_a,
                        &ring_a,
                        &enabled_a,
                        &out_enabled_a,
                        &transport_a,
                        &mut channel_bufs,
                        &mut input_bufs,
                        &mut event_list,
                        &mut output_events,
                        &mut input_slices_scratch,
                        &mut output_slices_scratch,
                        #[cfg(feature = "playback")]
                        playback_a.as_ref(),
                        #[cfg(feature = "playback")]
                        capture_a.as_ref(),
                    );
                },
                |err| eprintln!("Audio error: {err}"),
                None,
            )
            .map_err(|e| format!("could not build output stream: {e}"))?,
        format => {
            return Err(format!(
                "audio output format {format:?} is not supported \
                 (truce standalone currently only handles f32)"
            ));
        }
    };

    stream
        .play()
        .map_err(|e| format!("could not start output stream: {e}"))?;

    *stream_slot = Some(stream);
    if let Ok(mut g) = res.current_name.lock() {
        g.clone_from(&resolved_name);
    }

    vlog!(
        "Output: {} @ {} Hz, {} ch",
        resolved_name.as_deref().unwrap_or("(unnamed)"),
        sample_rate,
        channels,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Input worker
// ---------------------------------------------------------------------------

// Spawned-thread body — owns its state across the worker's lifetime.
// Switching to refs would force the caller to outlive the thread.
#[allow(clippy::needless_pass_by_value)]
fn input_worker(
    cmd_rx: mpsc::Receiver<InputCmd>,
    initial_device_name: Option<String>,
    channels: usize,
    sample_rate: f64,
    ring: Arc<Mutex<Vec<f32>>>,
    enabled_flag: Arc<AtomicBool>,
    current_name: Arc<Mutex<Option<String>>>,
) {
    let mut stream: Option<cpal::Stream> = None;
    let mut device_name = initial_device_name;
    let mut want_enabled = false;

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            InputCmd::SetEnabled(on) => {
                want_enabled = on;
                apply_input_state(
                    &mut stream,
                    want_enabled,
                    device_name.as_deref(),
                    channels,
                    sample_rate,
                    &ring,
                    &enabled_flag,
                    &current_name,
                );
            }
            InputCmd::SetDevice(name) => {
                device_name = name;
                if want_enabled {
                    // Drop old before opening new — some backends
                    // won't open a second exclusive stream against
                    // the same device.
                    stream = None;
                    enabled_flag.store(false, Ordering::Relaxed);
                    apply_input_state(
                        &mut stream,
                        true,
                        device_name.as_deref(),
                        channels,
                        sample_rate,
                        &ring,
                        &enabled_flag,
                        &current_name,
                    );
                } else if let Ok(mut g) = current_name.lock() {
                    // Reflect the chosen device immediately even
                    // though we haven't opened a stream — the menu
                    // checkmark should match the user's pick.
                    g.clone_from(&device_name);
                }
            }
        }
    }
    drop(stream);
}

#[allow(clippy::too_many_arguments)]
fn apply_input_state(
    stream: &mut Option<cpal::Stream>,
    want: bool,
    device_name: Option<&str>,
    channels: usize,
    sample_rate: f64,
    ring: &Arc<Mutex<Vec<f32>>>,
    enabled_flag: &Arc<AtomicBool>,
    current_name: &Arc<Mutex<Option<String>>>,
) {
    let currently = stream.is_some();
    if want == currently {
        return;
    }
    if want {
        let host = cpal::default_host();
        let device = match device_name {
            Some(name) => find_device(&host, name, false),
            None => host.default_input_device(),
        };
        if let Some(dev) = device {
            let resolved = dev.name().ok();
            match build_and_play_input_stream(&dev, channels, sample_rate, Arc::clone(ring)) {
                Ok(s) => {
                    *stream = Some(s);
                    enabled_flag.store(true, Ordering::Relaxed);
                    if let Ok(mut g) = current_name.lock() {
                        *g = resolved;
                    }
                }
                Err(e) => {
                    eprintln!("mic enable failed: {e}");
                    enabled_flag.store(false, Ordering::Relaxed);
                }
            }
        } else {
            eprintln!("mic enable failed: no input device available");
            enabled_flag.store(false, Ordering::Relaxed);
        }
    } else {
        *stream = None;
        enabled_flag.store(false, Ordering::Relaxed);
        if let Ok(mut r) = ring.lock() {
            r.clear();
        }
    }
}

/// Build an input stream against the given device that drains
/// captured samples into `ring`. Called from the worker thread.
fn build_and_play_input_stream(
    device: &cpal::Device,
    channels: usize,
    sample_rate: f64,
    ring: Arc<Mutex<Vec<f32>>>,
) -> Result<cpal::Stream, BoxErr> {
    // Channel count < u16::MAX (typical: 1-8); sample rate goes
    // through `cast::sample_rate_u32` which debug-asserts the
    // (positive, ≤ u32::MAX) preconditions.
    #[allow(clippy::cast_possible_truncation)]
    let input_config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: cpal::SampleRate(sample_rate_u32(sample_rate)),
        buffer_size: cpal::BufferSize::Default,
    };
    let stream = device
        .build_input_stream(
            &input_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if let Ok(mut buf) = ring.lock() {
                    let max_size = sample_count_usize(sample_rate) * channels / 10;
                    buf.extend_from_slice(data);
                    if buf.len() > max_size {
                        let drain = buf.len() - max_size;
                        buf.drain(..drain);
                    }
                }
            },
            |err| eprintln!("Input error: {err}"),
            None,
        )
        .map_err(|e| -> BoxErr { format!("could not build input stream: {e}").into() })?;
    stream
        .play()
        .map_err(|e| -> BoxErr { format!("could not start input stream: {e}").into() })?;
    Ok(stream)
}

fn find_device(host: &cpal::Host, name: &str, output: bool) -> Option<cpal::Device> {
    let devices = if output {
        host.output_devices().ok()?
    } else {
        host.input_devices().ok()?
    };
    devices.into_iter().find(|d| {
        d.name()
            .is_ok_and(|n| n.to_lowercase().contains(&name.to_lowercase()))
    })
}

/// Build the cpal `StreamConfig` honoring opts where possible. Falls
/// back to the device's default config for any unspecified or
/// unsupported choice.
fn resolve_config(
    device: &cpal::Device,
    default: &cpal::SupportedStreamConfig,
    opts: &Options,
) -> cpal::StreamConfig {
    let channels = default.channels();
    let mut sample_rate = default.sample_rate();
    let mut buffer_size = cpal::BufferSize::Default;

    if let Some(sr) = opts.sample_rate {
        // Verify the requested rate is in the supported set; fall
        // back silently if not.
        if let Ok(mut ranges) = device.supported_output_configs() {
            let desired = cpal::SampleRate(sr);
            let supported =
                ranges.any(|r| r.min_sample_rate() <= desired && r.max_sample_rate() >= desired);
            if supported {
                sample_rate = desired;
            } else {
                eprintln!(
                    "sample rate {sr} Hz not supported; \
                     using device default {}",
                    default.sample_rate().0
                );
            }
        }
    }
    if let Some(bs) = opts.buffer_size {
        buffer_size = cpal::BufferSize::Fixed(bs);
    }

    cpal::StreamConfig {
        channels,
        sample_rate,
        buffer_size,
    }
}

trait BufferSizeMax {
    fn buffer_size_max_frames(&self) -> usize;
}
impl BufferSizeMax for cpal::StreamConfig {
    fn buffer_size_max_frames(&self) -> usize {
        match self.buffer_size {
            cpal::BufferSize::Fixed(n) => n as usize,
            cpal::BufferSize::Default => 2048, // reasonable upper bound for `reset`
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn audio_callback<P: PluginExport>(
    data: &mut [f32],
    channels: usize,
    sample_rate: f64,
    is_effect: bool,
    plugin: &Arc<Mutex<P>>,
    pending: &Arc<Mutex<Vec<MidiEvent>>>,
    input_ring: &Arc<Mutex<Vec<f32>>>,
    input_enabled: &Arc<AtomicBool>,
    output_enabled: &Arc<AtomicBool>,
    transport: &Transport,
    channel_bufs: &mut Vec<Vec<f32>>,
    input_bufs: &mut Vec<Vec<f32>>,
    event_list: &mut EventList,
    output_events: &mut EventList,
    input_slices: &mut Vec<&'static [f32]>,
    output_slices: &mut Vec<&'static mut [f32]>,
    #[cfg(feature = "playback")] playback: Option<&Arc<crate::playback::PlaybackSource>>,
    #[cfg(feature = "playback")] capture: Option<&crate::playback::CapturePusher>,
) {
    let num_frames = data.len() / channels;

    event_list.clear();
    output_events.clear();
    if let Ok(mut events) = pending.try_lock() {
        for ev in events.drain(..) {
            event_list.push(Event {
                sample_offset: 0,
                body: ev.body,
            });
        }
    }

    let Ok(mut plugin) = plugin.try_lock() else {
        data.fill(0.0);
        return;
    };

    // Re-shape the persistent channel scratch to (channels, num_frames)
    // and zero it. `Vec::resize` and `Vec::resize(.., 0.0)` only
    // allocate when growing past capacity, so a stable cpal stream
    // (fixed channels + buffer size) does not allocate after warm-up.
    channel_bufs.resize_with(channels, Vec::new);
    for buf in channel_bufs.iter_mut() {
        buf.clear();
        buf.resize(num_frames, 0.0);
    }

    // Effect-only input plumbing: mic + file are independent input
    // sources that *sum* into the plugin's bus, and the plugin reads
    // from `input_bufs` while writing to `channel_bufs`. Three
    // `is_effect`-gated steps live together here so the invariant
    // ("only effects have inputs") stays in one place — instruments
    // skip the whole block and just clear `input_bufs`.
    if is_effect {
        // (1) Mic ring → channel_bufs (per-block sum).
        if input_enabled.load(Ordering::Relaxed)
            && let Ok(mut ring) = input_ring.try_lock()
        {
            let needed = num_frames * channels;
            let available = ring.len().min(needed);
            for i in 0..available / channels {
                for ch in 0..channels {
                    if i < num_frames {
                        channel_bufs[ch][i] += ring[i * channels + ch];
                    }
                }
            }
            if available > 0 {
                ring.drain(..available);
            }
        }

        // (2) Playback file → channel_bufs (per-block sum, summed on
        // top of mic above).
        #[cfg(feature = "playback")]
        if let Some(src) = playback {
            src.mix_into(channel_bufs, num_frames);
        }

        // (3) Mirror channel_bufs into input_bufs so the plugin can
        // hold an immutable input slice and a mutable output slice
        // pointing at independent storage. Same resize-without-realloc
        // trick as above.
        input_bufs.resize_with(channels, Vec::new);
        for (dst, src) in input_bufs.iter_mut().zip(channel_bufs.iter()) {
            dst.clear();
            dst.extend_from_slice(src);
        }
    } else {
        input_bufs.clear();
    }
    // Reuse the caller's slice scratches: each block clears and
    // re-extends, so allocation only happens when the scratch grows
    // past its initial capacity (channels-many slots reserved at
    // setup). The caller stores the scratch as `Vec<&'static [f32]>`
    // because cpal's stream closure must be `'static`; the slices
    // here borrow from `input_bufs` / `channel_bufs` on the same
    // stack frame, so the laundered lifetime never escapes this
    // call.
    input_slices.clear();
    output_slices.clear();
    for buf in input_bufs.iter() {
        let slice: &[f32] = buf.as_slice();
        // SAFETY: lifetime laundered to 'static for storage; the
        // slice is read only inside this `audio_callback` and the
        // scratch is `clear()`ed at the top of the next block before
        // anyone could observe a dangling element.
        input_slices.push(unsafe { transmute::<&[f32], &'static [f32]>(slice) });
    }
    for buf in channel_bufs.iter_mut() {
        let slice: &mut [f32] = buf.as_mut_slice();
        // SAFETY: same reasoning as the input slice push above.
        output_slices.push(unsafe { transmute::<&mut [f32], &'static mut [f32]>(slice) });
    }

    // SAFETY: The slices stored in `input_slices` / `output_slices`
    // were just transmuted *up* to `'static` from this stack frame's
    // `input_bufs` / `channel_bufs`. To call `from_slices` we need
    // matching outer-borrow lifetimes; reborrow through a raw pointer
    // to escape the `&'2 mut Vec<&'static mut [f32]>` invariance, then
    // transmute the resulting `AudioBuffer<'static>` back down to a
    // borrow tied to this call so the buffer can't outlive
    // `audio_callback`. Same transmute pattern as
    // `truce-clap::clap_plugin_process` and `RawBufferScratch::build`.
    let mut audio_buffer = unsafe {
        let in_ptr: *mut Vec<&'static [f32]> = input_slices;
        let out_ptr: *mut Vec<&'static mut [f32]> = output_slices;
        transmute::<AudioBuffer<'static>, AudioBuffer<'_>>(AudioBuffer::from_slices(
            &*in_ptr,
            &mut *out_ptr,
            num_frames,
        ))
    };

    let transport_info = transport.tick_audio(num_frames);
    let mut context = ProcessContext::new(&transport_info, sample_rate, num_frames, output_events);

    plugin.process(&mut audio_buffer, event_list, &mut context);

    // `--output-file` capture: hand a copy of the post-process,
    // pre-mute output to the writer thread. Mute is *device*
    // silence (speakers off); the file should still get the
    // real plugin output, matching every DAW's mute-and-bounce.
    // The capture path transfers Vec ownership to the writer thread
    // (channel-bounded `mpsc::sync_channel`), so the per-block alloc
    // here can't be amortized without a free-list pool. Left as-is —
    // capture is a `--output-file` dev convenience, not a hot path.
    #[cfg(feature = "playback")]
    if let Some(pusher) = capture {
        let mut interleaved = vec![0.0_f32; num_frames * channels];
        for frame in 0..num_frames {
            for ch in 0..channels {
                let ch_idx = ch.min(channel_bufs.len() - 1);
                interleaved[frame * channels + ch] = channel_bufs[ch_idx][frame];
            }
        }
        pusher.submit(interleaved);
    }

    // Output mute: keep the plugin running (transport, MIDI, meters
    // all still tick) but zero-fill the device buffer so the
    // speakers stay silent. Cheaper and more responsive than
    // tearing down the cpal stream.
    if !output_enabled.load(Ordering::Relaxed) {
        data.fill(0.0);
        return;
    }

    for frame in 0..num_frames {
        for ch in 0..channels {
            let ch_idx = ch.min(channel_bufs.len() - 1);
            data[frame * channels + ch] = channel_bufs[ch_idx][frame];
        }
    }
}
