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
//!   off - saves CPU and skips the OS mic permission prompt) and
//!   output (mute - keep the stream open so processing keeps ticking,
//!   just zero-fill the speaker buffer).
//! - **Switch device** for either side. Worker drops the old stream
//!   and opens a new one against the requested device name; on
//!   failure the previous device's name remains in place and the
//!   audio callback keeps running unchanged.

use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use truce_core::buffer::RawBufferScratch;
use truce_core::cast::{sample_count_usize, sample_rate_u32};
use truce_core::chunked_process::{ChunkedProcess, process_chunked};
use truce_core::config::{AudioConfig, ProcessMode};
use truce_core::events::{EVENT_LIST_PREALLOC, Event, EventBody, EventList};
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_params::{ParamInfo, Params};

use crate::cli::Options;
use crate::transport::Transport;
use crate::vlog;

type BoxErr = Box<dyn std::error::Error>;

/// Per-stream audio-callback scratch for the channel-pointer arrays
/// fed into [`RawBufferScratch::build`]. cpal's stream closure must
/// be `Send + 'static`, but `Vec<*const f32>` / `Vec<*mut f32>` are
/// `!Send` because raw pointers don't carry thread-safety. The cpal
/// closure runs on a single dedicated audio thread per stream and the
/// pointer values are written + consumed within one callback (they
/// always alias `input_bufs` / `channel_bufs` on the same stack
/// frame), so the closure capture is sound.
struct CallbackPtrScratch {
    inputs: Vec<*const f32>,
    outputs: Vec<*mut f32>,
}
// SAFETY: see [`CallbackPtrScratch`] doc.
unsafe impl Send for CallbackPtrScratch {}

/// A queued MIDI event the UI thread hands off to the audio callback.
pub struct MidiEvent {
    pub body: EventBody,
    /// Plugin MIDI input port this event targets. Device input stamps
    /// the port its `--midi-input` slot maps to; the QWERTY keyboard
    /// uses `0`.
    pub port: u8,
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
    pub pending: Arc<ArrayQueue<MidiEvent>>,
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
// Channel routing
// ---------------------------------------------------------------------------

/// How the plugin's channels map onto the audio device's channels.
///
/// Stored encoded in an `AtomicUsize` on each controller (so the menu
/// can update it lock-free) and decoded in the audio callback. The
/// default, `Direct`, is the historical 1:1 mapping and is what every
/// non-multichannel-aware caller gets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelRoute {
    /// Plugin channel N ↔ device channel N, across every channel.
    /// Preserves multichannel plugins; for a stereo plugin on a
    /// 4-out interface this is the same as `Stereo { base: 0 }`.
    Direct,
    /// Plugin channels 0 and 1 ↔ device channels `base` and `base+1`.
    /// Other device outputs are silenced; other device inputs ignored.
    Stereo { base: usize },
    /// A single device channel `base`. On output the plugin's first
    /// two channels fold down (sum) into it; on input it feeds both
    /// plugin input channels.
    Mono { base: usize },
}

impl ChannelRoute {
    /// Pack into the `usize` the controllers store. `Direct` is 0 so a
    /// freshly-zeroed atomic decodes to the default mapping.
    #[must_use]
    pub fn encode(self) -> usize {
        match self {
            ChannelRoute::Direct => 0,
            ChannelRoute::Stereo { base } => 1 + base * 2,
            ChannelRoute::Mono { base } => 2 + base * 2,
        }
    }

    /// Parse a CLI / env spec into a route. Channel numbers are
    /// 1-based (matching the menu labels): `direct` / `all` →
    /// [`Self::Direct`], `N` → [`Self::Mono`] on channel N, `N-M` →
    /// [`Self::Stereo`] pair starting at N (requires `M == N + 1`).
    /// Returns `None` for anything malformed.
    #[must_use]
    pub fn parse(spec: &str) -> Option<Self> {
        let s = spec.trim().to_ascii_lowercase();
        if s == "direct" || s == "all" {
            return Some(ChannelRoute::Direct);
        }
        if let Some((a, b)) = s.split_once('-') {
            let a: usize = a.trim().parse().ok()?;
            let b: usize = b.trim().parse().ok()?;
            if a >= 1 && b == a + 1 {
                return Some(ChannelRoute::Stereo { base: a - 1 });
            }
            return None;
        }
        let c: usize = s.parse().ok()?;
        (c >= 1).then(|| ChannelRoute::Mono { base: c - 1 })
    }

    /// Inverse of [`Self::encode`].
    #[must_use]
    pub fn decode(v: usize) -> Self {
        if v == 0 {
            return ChannelRoute::Direct;
        }
        let k = v - 1;
        if k.is_multiple_of(2) {
            ChannelRoute::Stereo { base: k / 2 }
        } else {
            ChannelRoute::Mono { base: (k - 1) / 2 }
        }
    }
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
    /// How device input channels map onto the plugin's input bus,
    /// encoded per [`ChannelRoute`]. Read by the output callback when
    /// summing the input ring into the plugin bus; lets a stereo
    /// plugin pull from device inputs 3-4, or a mono source feed both
    /// plugin inputs. Shared with the callback.
    channel_route: Arc<AtomicUsize>,
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

    /// Choose how device input channels feed the plugin's input bus.
    /// Takes effect on the next audio block.
    pub fn set_channel_route(&self, route: ChannelRoute) {
        self.channel_route.store(route.encode(), Ordering::Relaxed);
    }

    /// The current input channel routing.
    #[must_use]
    pub fn channel_route(&self) -> ChannelRoute {
        ChannelRoute::decode(self.channel_route.load(Ordering::Relaxed))
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
    /// How the plugin's output bus maps onto device output channels,
    /// encoded per [`ChannelRoute`]. Read by the output callback when
    /// writing the plugin bus into the device buffer; lets a stereo
    /// plugin drive device outputs 3-4, or fold down to a mono output.
    /// Shared with the callback.
    channel_route: Arc<AtomicUsize>,
    /// The plugin's current channel count (= the active bus layout's
    /// width). The worker updates it after a `SetChannels` rebuild; the
    /// Bus Layout menu reads it to mark the active entry.
    channels: Arc<AtomicUsize>,
}

enum OutputCmd {
    SetDevice(Option<String>),
    /// Switch the plugin to a bus layout of this channel count. The
    /// worker rebuilds the stream at the new width (if the device
    /// supports it), so the plugin's `process` sees the new channel count.
    SetChannels(u16),
}

impl OutputController {
    /// Mute / unmute the output. The cpal stream stays open either
    /// way - disabling just makes the audio callback zero-fill its
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
    /// non-fatal - the previous stream remains running.
    pub fn set_device(&self, name: Option<String>) {
        let _ = self.cmd_tx.send(OutputCmd::SetDevice(name));
    }

    /// Switch the plugin to a bus layout of `channels` width. The audio
    /// stream is torn down and rebuilt at the new channel count; on a
    /// device that can't provide it, the previous stream stays running
    /// (the worker logs and keeps the old width).
    pub fn set_channels(&self, channels: u16) {
        let _ = self.cmd_tx.send(OutputCmd::SetChannels(channels));
    }

    /// The plugin's current channel count (the active bus layout's width).
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels.load(Ordering::Relaxed)
    }

    /// Currently-resolved output device name, or `None` if not
    /// resolvable.
    #[must_use]
    pub fn current_name(&self) -> Option<String> {
        self.current_name.lock().ok().and_then(|g| g.clone())
    }

    /// Choose how the plugin's output bus maps onto device output
    /// channels. Takes effect on the next audio block.
    pub fn set_channel_route(&self, route: ChannelRoute) {
        self.channel_route.store(route.encode(), Ordering::Relaxed);
    }

    /// The current output channel routing.
    #[must_use]
    pub fn channel_route(&self) -> ChannelRoute {
        ChannelRoute::decode(self.channel_route.load(Ordering::Relaxed))
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
        host.default_output_device()
            .and_then(|d| d.description().map(|desc| desc.name().to_string()).ok())
    } else {
        host.default_input_device()
            .and_then(|d| d.description().map(|desc| desc.name().to_string()).ok())
    };
    let names = if output {
        host.output_devices()
            .map(|it| {
                it.filter_map(|d| d.description().map(|desc| desc.name().to_string()).ok())
                    .collect()
            })
            .unwrap_or_default()
    } else {
        host.input_devices()
            .map(|it| {
                it.filter_map(|d| d.description().map(|desc| desc.name().to_string()).ok())
                    .collect()
            })
            .unwrap_or_default()
    };
    (default_name, names)
}

/// Background-refreshed cache of cpal device names.
///
/// `CoreAudio` enumeration is slow - hundreds of milliseconds with a
/// few devices connected - and the macOS menu used to run it
/// synchronously on the main thread every time a device submenu
/// opened, which made the submenu visibly lag. The menu now reads
/// cached names (instant) and fires an off-thread refresh so the
/// *next* open reflects hot-plugged or removed devices. Cloneable;
/// all clones share one cache.
#[derive(Clone)]
pub struct DeviceCache {
    inner: Arc<Mutex<DeviceNames>>,
    refreshing: Arc<AtomicBool>,
}

impl Default for DeviceCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct DeviceNames {
    outputs: Vec<String>,
    inputs: Vec<String>,
    /// False until the first enumeration lands. Distinguishes "not
    /// warmed yet" from "warmed and genuinely empty" so a machine with
    /// no inputs doesn't re-enumerate on every read.
    warmed: bool,
}

impl DeviceCache {
    /// Create the cache and warm it on a background thread.
    #[must_use]
    pub fn new() -> Self {
        let cache = Self {
            inner: Arc::new(Mutex::new(DeviceNames::default())),
            refreshing: Arc::new(AtomicBool::new(false)),
        };
        cache.refresh_async();
        cache
    }

    /// Cached output device names. Blocks once on a cold read (a menu
    /// opened before the warm-up finished) so the list is never
    /// spuriously empty.
    #[must_use]
    pub fn outputs(&self) -> Vec<String> {
        self.ensure_warm();
        self.inner
            .lock()
            .map(|g| g.outputs.clone())
            .unwrap_or_default()
    }

    /// Cached input device names. Same cold-read guarantee as
    /// [`Self::outputs`].
    #[must_use]
    pub fn inputs(&self) -> Vec<String> {
        self.ensure_warm();
        self.inner
            .lock()
            .map(|g| g.inputs.clone())
            .unwrap_or_default()
    }

    fn ensure_warm(&self) {
        // Check (and release the lock) before the slow enumeration so
        // we never hold the mutex across a CoreAudio round-trip.
        match self.inner.lock() {
            Ok(g) if g.warmed => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let (_, outputs) = enumerate_devices(true);
        let (_, inputs) = enumerate_devices(false);
        if let Ok(mut names) = self.inner.lock() {
            names.outputs = outputs;
            names.inputs = inputs;
            names.warmed = true;
        }
    }

    /// Re-enumerate both device lists on a background thread, storing
    /// the result for the next read. No-op while a refresh is already
    /// in flight, so rapid menu reopens don't pile up threads.
    pub fn refresh_async(&self) {
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let refreshing = Arc::clone(&self.refreshing);
        std::thread::spawn(move || {
            let (_, outputs) = enumerate_devices(true);
            let (_, inputs) = enumerate_devices(false);
            if let Ok(mut names) = inner.lock() {
                names.outputs = outputs;
                names.inputs = inputs;
                names.warmed = true;
            }
            refreshing.store(false, Ordering::Release);
        });
    }
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
#[allow(clippy::too_many_lines)]
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

    let requested_channels = bus_layout_channels::<P>(opts);
    let config: cpal::StreamConfig =
        resolve_config(&initial_output, &default_config, opts, requested_channels);
    let sample_format = default_config.sample_format();
    let sample_rate = f64::from(config.sample_rate);
    let channels = config.channels as usize;
    let is_effect = P::info().category == PluginCategory::Effect;

    // `std::sync::Mutex`, like the format wrappers' mediation lock
    // (on macOS it sits on `os_unfair_lock`, which donates the
    // waiting audio thread's priority to the lock owner). One
    // difference in poison policy: the standalone runs on the
    // developer's machine during iteration, so a panic on either
    // side keeps the poison and the next try_lock fails loudly
    // rather than silently handing out half-mutated state - the
    // wrappers forgive poison (`lock_plugin`) because inside a DAW
    // permanent silence is the worse failure.
    // Capacity 256: covers a generous MIDI burst within a single
    // audio callback period. ArrayQueue is lock-free MPMC - the MIDI
    // input thread pushes, the audio thread drains, neither blocks.
    // On overflow the producer drops the oldest event (see midi.rs).
    let pending: Arc<ArrayQueue<MidiEvent>> = Arc::new(ArrayQueue::new(256));
    let initial_max_frames = config.buffer_size_max_frames(&default_config);
    let plugin = Arc::new(Mutex::new({
        let mut p = P::create();
        p.init();
        p.reset(&AudioConfig::new(sample_rate, initial_max_frames));
        // Apply `--state <path>` BEFORE snapping smoothers so the
        // first audio block sees the restored values, not defaults
        // ramping toward them.
        if let Some(path) = opts.state_path.as_deref() {
            crate::state::load_into(&mut p, path);
        }
        // `--preset` layers on top of `--state` (both pre-snap so
        // the first block sees restored values), resolved through
        // the same store `--list-presets` uses.
        if let Some(sel) = opts.preset.as_deref() {
            crate::presets::apply_on_launch::<P>(opts.presets_dir.as_deref(), &mut p, sel);
        }
        p.params().snap_smoothers();
        p
    }));

    let input_setup = setup_input_pipeline(&audio_host, opts, is_effect, channels, sample_rate);
    let input_ring = input_setup.ring;
    let input_enabled = input_setup.enabled;
    let input_controller = input_setup.controller;
    if let Some(spec) = opts.input_channels.as_deref() {
        match ChannelRoute::parse(spec) {
            Some(route) => input_controller.set_channel_route(route),
            None => eprintln!(
                "--input-channels: ignoring invalid '{spec}' \
                 (expected 'direct', a channel like '3', or a pair like '3-4')"
            ),
        }
    }

    let transport = Transport::new(opts.bpm.unwrap_or(120.0), sample_rate);

    // Initial output device name (may differ from the resolver's
    // requested name if it matched by substring). When the user did
    // not pass `--output`, leave this `None` so the worker re-resolves
    // via `default_output_device()` on each open - the cpal ALSA
    // backend's virtual default reports a description ("Default Audio
    // Device") that doesn't appear in `output_devices()`, so a
    // name-based re-resolve would fail.
    let initial_output_name = if opts.output_device.is_some() {
        initial_output
            .description()
            .map(|d| d.name().to_string())
            .ok()
    } else {
        None
    };
    let output_current_name = Arc::new(Mutex::new(initial_output_name.clone()));
    let (output_cmd_tx, output_cmd_rx) = mpsc::channel::<OutputCmd>();
    let (open_result_tx, open_result_rx) = mpsc::channel::<Result<(), String>>();

    // Output defaults to enabled - the user launched standalone to
    // hear the plugin. `--output-enabled off` (or the config file)
    // can flip the launch state.
    let output_enabled = Arc::new(AtomicBool::new(opts.output_enabled.unwrap_or(true)));

    let output_channel_route = Arc::new(AtomicUsize::new(0));
    // Shared plugin channel count (active bus layout width). Seeded with
    // the launch value; the worker updates it on a `SetChannels` rebuild.
    let output_channels_shared = Arc::new(AtomicUsize::new(channels));
    let output_controller = OutputController {
        enabled: Arc::clone(&output_enabled),
        cmd_tx: output_cmd_tx,
        current_name: Arc::clone(&output_current_name),
        channel_route: Arc::clone(&output_channel_route),
        channels: Arc::clone(&output_channels_shared),
    };
    // Apply `--output-channels` (and its env var) once at launch. The
    // native menus override this live; on Linux (no menu) the CLI is
    // the only way to pick channels. Input is applied below, once its
    // controller exists.
    if let Some(spec) = opts.output_channels.as_deref() {
        match ChannelRoute::parse(spec) {
            Some(route) => output_controller.set_channel_route(route),
            None => eprintln!(
                "--output-channels: ignoring invalid '{spec}' \
                 (expected 'direct', a channel like '3', or a pair like '3-4')"
            ),
        }
    }

    // Decode `--input-file` (if set) once at startup against the
    // resolved device sample-rate / channel-count. Hard error on
    // unreadable / unparseable file - we fail noisily here rather
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
                "Capture: {} ({} Hz, {} ch, f32) - pre-mute output",
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
        input_channel_route: Arc::clone(&input_controller.channel_route),
        output_channel_route: Arc::clone(&output_channel_route),
        channels: Arc::clone(&output_channels_shared),
        promised_max_frames: Arc::new(AtomicUsize::new(initial_max_frames)),
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

    // Wait for the worker to confirm initial open so any error
    // propagates back to `start_audio`'s caller synchronously.
    match open_result_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e.into()),
        Err(e) => return Err(format!("output worker exited before reporting: {e}").into()),
    }

    if !output_enabled.load(Ordering::Relaxed) {
        vlog!(
            "Output: muted at launch - toggle from the Plugin menu or \
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
        let name = device.and_then(|d| d.description().map(|desc| desc.name().to_string()).ok());
        if name.is_none() {
            eprintln!("Note: no input device found - input-enable will be a no-op.");
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
        channel_route: Arc::new(AtomicUsize::new(0)),
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
                        "disabled - press Cmd+I in the window or pass --input-enabled on"
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        "disabled - press Ctrl+I in the window or pass --input-enabled on"
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
    pending: Arc<ArrayQueue<MidiEvent>>,
    input_ring: Arc<Mutex<Vec<f32>>>,
    input_enabled: Arc<AtomicBool>,
    /// Drives the audio callback's mute / unmute decision (UI thread
    /// flips it via `OutputController::set_enabled`).
    output_enabled: Arc<AtomicBool>,
    transport: Transport,
    current_name: Arc<Mutex<Option<String>>>,
    /// Input channel routing (encoded [`ChannelRoute`]). Shared with
    /// `InputController` (menu writes it).
    input_channel_route: Arc<AtomicUsize>,
    /// Output channel routing (encoded [`ChannelRoute`]). Shared with
    /// `OutputController` (menu writes it).
    output_channel_route: Arc<AtomicUsize>,
    /// The plugin's current channel count, shared with `OutputController`
    /// so the Bus Layout menu can mark the active entry. The worker
    /// updates it after a `SetChannels` rebuild.
    channels: Arc<AtomicUsize>,
    /// The `max_frames` the plugin was last `reset()` with. A device
    /// switch onto a larger buffer bound must renew the promise
    /// before the new stream's first callback.
    promised_max_frames: Arc<AtomicUsize>,
    /// Optional `.wav` playback source (gated on the `playback`
    /// feature). When present, summed into the input bus alongside
    /// the mic ring - see the matrix in `cli.rs::HELP`.
    #[cfg(feature = "playback")]
    playback: Option<Arc<crate::playback::PlaybackSource>>,
    /// Optional `--output-file` capture pusher. Cloned into each
    /// cpal callback closure, so device switches don't tear down
    /// the capture. The owning `CaptureSink` lives on
    /// `AudioHandles`; finalize is the runner's responsibility.
    #[cfg(feature = "playback")]
    capture: Option<crate::playback::CapturePusher>,
}

// Spawned-thread body - owns its state across the worker's lifetime.
// Switching to refs would force the caller to outlive the thread.
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
fn output_worker<P: PluginExport>(
    cmd_rx: mpsc::Receiver<OutputCmd>,
    open_result: mpsc::Sender<Result<(), String>>,
    initial_device_name: Option<String>,
    mut config: cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    sample_rate: f64,
    mut channels: usize,
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
                // - some backends won't open a second exclusive
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
            OutputCmd::SetChannels(new_ch) => {
                // Validate against the current device *before* tearing the
                // stream down, so a width the hardware can't provide leaves
                // the running audio intact.
                let host = cpal::default_host();
                let name = res.current_name.lock().ok().and_then(|g| g.clone());
                let device = match name.as_deref() {
                    Some(n) => find_device(&host, n, true),
                    None => host.default_output_device(),
                };
                match device {
                    Some(dev) if device_supports_output_channels(&dev, new_ch) => {
                        config.channels = new_ch;
                        channels = new_ch as usize;
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
                            eprintln!("bus-layout switch failed: {e}");
                        } else {
                            res.channels.store(channels, Ordering::Relaxed);
                        }
                    }
                    _ => eprintln!("bus-layout: device doesn't offer {new_ch} output channels"),
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
    // Resolve fresh each open - hot-plug may have changed the
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
    let resolved_name = device.description().map(|d| d.name().to_string()).ok();

    let plugin_a = Arc::clone(&res.plugin);
    let pending_a = Arc::clone(&res.pending);
    let ring_a = Arc::clone(&res.input_ring);
    let enabled_a = Arc::clone(&res.input_enabled);
    let out_enabled_a = Arc::clone(&res.output_enabled);
    let in_route_a = Arc::clone(&res.input_channel_route);
    let out_route_a = Arc::clone(&res.output_channel_route);
    let transport_a = res.transport.clone();
    #[cfg(feature = "playback")]
    let playback_a = res.playback.clone();
    #[cfg(feature = "playback")]
    let capture_a = res.capture.clone();

    // Per-stream audio-callback scratch. Owned by the move-closure so
    // it lives across callbacks but never crosses threads - cpal calls
    // the closure on a single dedicated audio thread per stream.
    // Amortizes the `vec![0.0; num_frames]` per-channel allocation and
    // the `channel_bufs.clone()` for the effect input mirror, plus the
    // two `EventList::default()`s per block (input drain + plugin output)
    // - both `clear()`ed and reused, capacity-preserving.
    let mut channel_bufs: Vec<Vec<f32>> = Vec::with_capacity(channels);
    let mut input_bufs: Vec<Vec<f32>> = Vec::with_capacity(channels);
    let mut event_list = EventList::with_capacity(EVENT_LIST_PREALLOC);
    let mut output_events = EventList::with_capacity(EVENT_LIST_PREALLOC);
    let mut sub_event_scratch = EventList::with_capacity(EVENT_LIST_PREALLOC);
    // Cached for `chunked_process::process_chunked`. Built once at
    // callback setup; static for the lifetime of the cpal stream.
    // `plugin_a` is locked once here (stream setup, not audio thread)
    // to pull `param_infos` + the `params_arc` clone the chunker uses
    // as its `&dyn Params` handle. The chunker can't call
    // `plugin.params()` itself because process_chunked already holds
    // `&mut plugin` for the duration of the `process()` calls.
    let (param_infos, params_arc) = {
        let p = plugin_a
            .lock()
            .expect("plugin mutex poisoned at audio setup");
        (p.params().param_infos(), p.params_arc())
    };
    let min_subblock_samples = P::info().automation.min_subblock_samples;
    // Raw-pointer arrays + `RawBufferScratch` reused across callbacks.
    // The pointer arrays mirror `input_bufs` / `channel_bufs` each
    // block; the scratch wraps the raw->slice conversion plus the
    // alias-detection copy fallback used by every format wrapper.
    let mut ptr_scratch = CallbackPtrScratch {
        inputs: Vec::with_capacity(channels),
        outputs: Vec::with_capacity(channels),
    };
    let mut scratch: RawBufferScratch<<P as truce_core::plugin::PluginRuntime>::Sample> =
        RawBufferScratch::default();
    // Pre-grow the widening / alias-copy scratch to the stream's frame
    // bound (the same number `reset` hands the plugin) so an f64
    // plugin's first callback doesn't allocate its per-channel scratch
    // inside cpal's real-time callback. Every format wrapper does this
    // at its setup hook; the standalone was the one caller that didn't.
    // The bound comes from *this* device's reported range - hot-plug
    // re-opens may land on a device with a different maximum.
    let supported = device
        .default_output_config()
        .map_err(|e| format!("could not query the output config for the scratch bound: {e}"))?;
    let frame_bound = config.buffer_size_max_frames(&supported);
    // The plugin sized its DSP for the bound it was last `reset()`
    // with; a device whose maximum exceeds it could deliver blocks
    // past that promise. Renew it before the stream opens (no
    // callback is running - the old stream is already dropped).
    if frame_bound > res.promised_max_frames.load(Ordering::Relaxed) {
        res.promised_max_frames
            .store(frame_bound, Ordering::Relaxed);
        let mut p = plugin_a
            .lock()
            .expect("plugin mutex poisoned at audio setup");
        p.reset(&AudioConfig::new(sample_rate, frame_bound));
    }
    scratch.ensure_capacity(channels, channels, frame_bound);

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
                        &in_route_a,
                        &out_route_a,
                        &transport_a,
                        &mut channel_bufs,
                        &mut input_bufs,
                        &mut event_list,
                        &mut output_events,
                        &mut sub_event_scratch,
                        &param_infos,
                        &params_arc,
                        min_subblock_samples,
                        &mut ptr_scratch,
                        &mut scratch,
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
                 (truce standalone handles f32 only)"
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

// Spawned-thread body - owns its state across the worker's lifetime.
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
                    // Drop old before opening new - some backends
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
                    // though we haven't opened a stream - the menu
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
            let resolved = dev.description().map(|d| d.name().to_string()).ok();
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
        sample_rate: sample_rate_u32(sample_rate),
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
        d.description()
            .is_ok_and(|desc| desc.name().to_lowercase().contains(&name.to_lowercase()))
    })
}

/// Build the cpal `StreamConfig` honoring opts where possible. Falls
/// back to the device's default config for any unspecified or
/// unsupported choice.
/// Channel count to request for a `--bus-layout <index>` selection: the
/// wider of that layout's total input / output channels. `None` (no
/// selection) keeps the device default. Out-of-range warns and falls back.
fn bus_layout_channels<P: PluginExport>(opts: &Options) -> Option<u16> {
    let idx = opts.bus_layout?;
    let layouts = P::bus_layouts();
    let Some(layout) = layouts.get(idx) else {
        eprintln!(
            "--bus-layout {idx}: out of range (plugin declares {} layout(s)); \
             using the device default",
            layouts.len()
        );
        return None;
    };
    let ch = layout
        .total_output_channels()
        .max(layout.total_input_channels());
    u16::try_from(ch).ok().filter(|&c| c > 0)
}

/// Whether the output device advertises a config with exactly `ch`
/// channels. Used to reject a `--bus-layout` wider than the hardware.
fn device_supports_output_channels(device: &cpal::Device, ch: u16) -> bool {
    device
        .supported_output_configs()
        .is_ok_and(|mut r| r.any(|c| c.channels() == ch))
}

fn resolve_config(
    device: &cpal::Device,
    default: &cpal::SupportedStreamConfig,
    opts: &Options,
    requested_channels: Option<u16>,
) -> cpal::StreamConfig {
    let mut channels = default.channels();
    // A `--bus-layout` selection runs the plugin at that layout's channel
    // count, so request it from the device. The device has to support the
    // count (you can't get 6 channels from a stereo interface); otherwise
    // keep the device default and warn.
    if let Some(req) = requested_channels {
        if req == channels {
            // Already the default - nothing to do.
        } else if device_supports_output_channels(device, req) {
            channels = req;
        } else {
            eprintln!(
                "--bus-layout: device doesn't offer {req} output channels; \
                 running the plugin at the device's {channels}"
            );
        }
    }
    let mut sample_rate = default.sample_rate();
    let mut buffer_size = cpal::BufferSize::Default;

    if let Some(sr) = opts.sample_rate {
        // Verify the requested rate is in the supported set; fall
        // back silently if not.
        if let Ok(mut ranges) = device.supported_output_configs() {
            let desired = sr;
            let supported =
                ranges.any(|r| r.min_sample_rate() <= desired && r.max_sample_rate() >= desired);
            if supported {
                sample_rate = desired;
            } else {
                eprintln!(
                    "sample rate {sr} Hz not supported; \
                     using device default {}",
                    default.sample_rate()
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
    fn buffer_size_max_frames(&self, supported: &cpal::SupportedStreamConfig) -> usize;
}
impl BufferSizeMax for cpal::StreamConfig {
    fn buffer_size_max_frames(&self, supported: &cpal::SupportedStreamConfig) -> usize {
        match self.buffer_size {
            cpal::BufferSize::Fixed(n) => n as usize,
            // `Default` leaves the callback size to the device: bound
            // by the size range the device itself reports, so `reset`
            // and the scratch pre-grow cover whatever it delivers. A
            // device that reports no range gets the same generous
            // fallback the VST3 wrapper uses for hosts that skip
            // `setupProcessing`.
            cpal::BufferSize::Default => match supported.buffer_size() {
                cpal::SupportedBufferSize::Range { max, .. } => *max as usize,
                cpal::SupportedBufferSize::Unknown => 8192,
            },
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn audio_callback<P: PluginExport>(
    data: &mut [f32],
    channels: usize,
    sample_rate: f64,
    is_effect: bool,
    plugin: &Arc<Mutex<P>>,
    pending: &Arc<ArrayQueue<MidiEvent>>,
    input_ring: &Arc<Mutex<Vec<f32>>>,
    input_enabled: &Arc<AtomicBool>,
    output_enabled: &Arc<AtomicBool>,
    input_channel_route: &Arc<AtomicUsize>,
    output_channel_route: &Arc<AtomicUsize>,
    transport: &Transport,
    channel_bufs: &mut Vec<Vec<f32>>,
    input_bufs: &mut Vec<Vec<f32>>,
    event_list: &mut EventList,
    output_events: &mut EventList,
    sub_event_scratch: &mut EventList,
    param_infos: &[ParamInfo],
    params_arc: &Arc<P::Params>,
    min_subblock_samples: u32,
    ptr_scratch: &mut CallbackPtrScratch,
    scratch: &mut RawBufferScratch<<P as truce_core::plugin::PluginRuntime>::Sample>,
    #[cfg(feature = "playback")] playback: Option<&Arc<crate::playback::PlaybackSource>>,
    #[cfg(feature = "playback")] capture: Option<&crate::playback::CapturePusher>,
) {
    let num_frames = data.len() / channels;

    let Ok(mut plugin) = plugin.try_lock() else {
        data.fill(0.0);
        return;
    };

    // Drain queued MIDI only after the lock is held. A lost try_lock
    // (an editor state read in flight) returns early above, and events
    // drained before it would be wiped by the next callback's
    // `event_list.clear()` - dropped notes. Left in `pending` they
    // arrive one block late at offset 0 instead.
    event_list.clear();
    output_events.clear();
    while let Some(ev) = pending.pop() {
        event_list.push(Event {
            sample_offset: 0,
            port: ev.port,
            body: ev.body,
        });
    }

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
    // ("only effects have inputs") stays in one place - instruments
    // skip the whole block and just clear `input_bufs`.
    if is_effect {
        // (1) Mic ring → channel_bufs (per-block sum).
        if input_enabled.load(Ordering::Relaxed)
            && let Ok(mut ring) = input_ring.try_lock()
        {
            // Map device input channels onto the plugin's input bus per
            // the selected route. `Direct` is the 1:1 default; `Stereo`
            // pulls a chosen device pair into plugin in 0/1; `Mono`
            // feeds one device channel into both plugin inputs. Bounds
            // checks keep an out-of-range base (e.g. after a device
            // swap to fewer channels) from indexing past the frame.
            let route = ChannelRoute::decode(input_channel_route.load(Ordering::Relaxed));
            let n_buf = channel_bufs.len();
            let needed = num_frames * channels;
            let available = ring.len().min(needed);
            for i in 0..available / channels {
                if i >= num_frames {
                    break;
                }
                let frame = &ring[i * channels..i * channels + channels];
                match route {
                    ChannelRoute::Direct => {
                        for ch in 0..channels {
                            channel_bufs[ch][i] += frame[ch];
                        }
                    }
                    ChannelRoute::Stereo { base } => {
                        if base < channels {
                            channel_bufs[0][i] += frame[base];
                        }
                        if n_buf > 1 && base + 1 < channels {
                            channel_bufs[1][i] += frame[base + 1];
                        }
                    }
                    ChannelRoute::Mono { base } => {
                        if base < channels {
                            let s = frame[base];
                            channel_bufs[0][i] += s;
                            if n_buf > 1 {
                                channel_bufs[1][i] += s;
                            }
                        }
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
    // Build raw-pointer arrays that mirror `input_bufs` / `channel_bufs`
    // and feed them through the shared `RawBufferScratch::build` helper.
    // Standalone never aliases input and output buffers (`input_bufs`
    // is a copy of `channel_bufs` for effects, plain empty for
    // instruments), so the alias-detection scratch path inside
    // `build` is dormant; routing through the same helper keeps
    // every host on a single unsafe path.
    ptr_scratch.inputs.clear();
    ptr_scratch.outputs.clear();
    for buf in input_bufs.iter() {
        ptr_scratch.inputs.push(buf.as_ptr());
    }
    for buf in channel_bufs.iter_mut() {
        ptr_scratch.outputs.push(buf.as_mut_ptr());
    }
    let num_frames_u32 = u32::try_from(num_frames).unwrap_or(u32::MAX);
    let num_in_u32 = u32::try_from(ptr_scratch.inputs.len()).unwrap_or(u32::MAX);
    let num_out_u32 = u32::try_from(ptr_scratch.outputs.len()).unwrap_or(u32::MAX);
    // SAFETY: the `*const f32` / `*mut f32` entries above all point
    // into `input_bufs` / `channel_bufs`, both alive for the rest of
    // this call (they're owned by the cpal closure and not mutated
    // until the next block); each `Vec<f32>` was sized to
    // `num_frames` above, satisfying `build`'s readability /
    // writability requirements.
    let mut audio_buffer = unsafe {
        scratch.build(
            ptr_scratch.inputs.as_ptr(),
            ptr_scratch.outputs.as_mut_ptr(),
            num_in_u32,
            num_out_u32,
            num_frames_u32,
            P::supports_in_place(),
        )
    };

    let transport_info = transport.tick_audio(num_frames);
    let mut transport_snap = transport_info;
    let chunk_args = ChunkedProcess {
        events: event_list,
        sub_event_scratch,
        transport: &mut transport_snap,
        sample_rate,
        // The standalone is a live host; offline render goes through
        // `offline.rs` + the driver, not this realtime path.
        process_mode: ProcessMode::Realtime,
        output_events,
        params_fn: None,
        meters_fn: None,
        param_infos,
        min_subblock_samples,
    };
    process_chunked(
        &mut *plugin,
        params_arc.as_ref() as &dyn Params,
        &mut audio_buffer,
        chunk_args,
    );
    let _ = audio_buffer;
    // Narrow rendered f64 output back to host f32 when the plugin's
    // `Sample = f64`. No-op for `f32` plugins.
    // SAFETY: `ptr_scratch.outputs` lives through this function;
    // `num_out_u32` / `num_frames_u32` match the build above.
    unsafe {
        scratch.finish_widening(
            ptr_scratch.outputs.as_mut_ptr(),
            num_out_u32,
            num_frames_u32,
        );
    }

    // `--output-file` capture: hand a copy of the post-process,
    // pre-mute output to the writer thread. Mute is *device*
    // silence (speakers off); the file should still get the
    // real plugin output, matching every DAW's mute-and-bounce.
    // The capture path transfers Vec ownership to the writer thread
    // (channel-bounded `mpsc::sync_channel`), so the per-block alloc
    // here can't be amortized without a free-list pool. Left as-is -
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

    // Map the plugin's output bus onto device output channels per the
    // selected route. `Direct` is the 1:1 default; `Stereo` drives a
    // chosen device pair (other channels silent); `Mono` folds the
    // plugin's first two channels down into one device channel. The
    // non-Direct modes clear the buffer first since they only write
    // the selected channels.
    let route = ChannelRoute::decode(output_channel_route.load(Ordering::Relaxed));
    let n_buf = channel_bufs.len();
    match route {
        ChannelRoute::Direct => {
            for frame in 0..num_frames {
                for ch in 0..channels {
                    let ch_idx = ch.min(n_buf - 1);
                    data[frame * channels + ch] = channel_bufs[ch_idx][frame];
                }
            }
        }
        ChannelRoute::Stereo { base } => {
            data.fill(0.0);
            for frame in 0..num_frames {
                if base < channels {
                    data[frame * channels + base] = channel_bufs[0][frame];
                }
                if n_buf > 1 && base + 1 < channels {
                    data[frame * channels + base + 1] = channel_bufs[1][frame];
                }
            }
        }
        ChannelRoute::Mono { base } => {
            data.fill(0.0);
            if base < channels {
                for frame in 0..num_frames {
                    let mut v = channel_bufs[0][frame];
                    if n_buf > 1 {
                        v += channel_bufs[1][frame];
                    }
                    data[frame * channels + base] = v;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ChannelRoute;

    #[test]
    fn encode_decode_roundtrips() {
        let cases = [
            ChannelRoute::Direct,
            ChannelRoute::Stereo { base: 0 },
            ChannelRoute::Stereo { base: 2 },
            ChannelRoute::Mono { base: 0 },
            ChannelRoute::Mono { base: 3 },
        ];
        for route in cases {
            assert_eq!(ChannelRoute::decode(route.encode()), route);
        }
    }

    #[test]
    fn direct_encodes_to_zero() {
        // A freshly-zeroed atomic must decode to the default mapping.
        assert_eq!(ChannelRoute::Direct.encode(), 0);
        assert_eq!(ChannelRoute::decode(0), ChannelRoute::Direct);
    }

    #[test]
    fn parse_specs() {
        assert_eq!(ChannelRoute::parse("direct"), Some(ChannelRoute::Direct));
        assert_eq!(ChannelRoute::parse(" ALL "), Some(ChannelRoute::Direct));
        // 1-based in the spec, 0-based base internally.
        assert_eq!(
            ChannelRoute::parse("1"),
            Some(ChannelRoute::Mono { base: 0 })
        );
        assert_eq!(
            ChannelRoute::parse("3"),
            Some(ChannelRoute::Mono { base: 2 })
        );
        assert_eq!(
            ChannelRoute::parse("3-4"),
            Some(ChannelRoute::Stereo { base: 2 })
        );
        // Non-adjacent / reversed / zero / garbage are rejected.
        assert_eq!(ChannelRoute::parse("3-5"), None);
        assert_eq!(ChannelRoute::parse("4-3"), None);
        assert_eq!(ChannelRoute::parse("0"), None);
        assert_eq!(ChannelRoute::parse("x"), None);
    }
}
