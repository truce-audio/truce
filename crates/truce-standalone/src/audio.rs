//! Shared cpal audio setup + callback. One implementation used by
//! both `windowed` and `headless` runners.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList};
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_core::process::ProcessContext;
use truce_params::Params;

use crate::cli::Options;
use crate::transport::Transport;

/// A queued MIDI event the UI thread hands off to the audio callback.
pub struct MidiEvent {
    pub body: EventBody,
}

/// Shared audio-thread resources handed back from `start_audio`.
///
/// The output stream is `!Send` (cpal Streams hold thread-bound
/// CoreAudio handles on macOS), so `AudioHandles` is also `!Send`.
/// The outer thread owns the struct directly; cross-thread
/// communication for input-toggle requests goes through the
/// `InputController` handle below, which IS `Send + Sync`.
pub struct AudioHandles<P: PluginExport> {
    /// The live output stream. Hold on to it — dropping it stops audio.
    pub _stream: cpal::Stream,
    /// Event queue the caller pushes MIDI into; drained by the audio
    /// callback each block.
    pub pending: Arc<Mutex<Vec<MidiEvent>>>,
    /// Plugin instance shared between caller and audio callback.
    pub plugin: Arc<Mutex<P>>,
    /// Audio config (sample rate, channels) resolved from the device.
    pub sample_rate: f64,
    pub channels: usize,
    pub is_effect: bool,
    /// `Send + Sync` handle for toggling mic input from the UI
    /// thread. The actual cpal input stream lives on a worker
    /// thread that owns it.
    pub input: InputController,
    /// Shared transport state; UI thread toggles play/stop, audio
    /// thread advances position each block.
    pub transport: Transport,
}

/// `Send + Sync` handle for managing mic input from the UI thread.
///
/// Cloneable; multiple holders can request toggles. The actual
/// `cpal::Stream` (`!Send` on macOS) lives on a dedicated worker
/// thread spawned by `start_audio`; the worker receives toggle
/// requests via the channel and opens/closes the stream.
#[derive(Clone)]
pub struct InputController {
    /// Audio callback reads this every block to decide whether to
    /// drain the input ring or zero-fill. Worker thread updates it
    /// when a toggle completes (or fails).
    pub enabled: Arc<AtomicBool>,
    /// True if the resolved input device exists at launch. When
    /// false, toggling on is a no-op (toggle returns instantly,
    /// `enabled` stays false).
    pub has_device: bool,
    /// Sender for toggle requests. Worker thread blocks on the
    /// matching receiver. Drop the worker by dropping the sender
    /// (channel closes; worker exits its recv loop).
    toggle_tx: mpsc::Sender<bool>,
}

impl InputController {
    /// Toggle the input. Returns immediately; the worker thread
    /// processes the request asynchronously. Inspect `enabled`
    /// after a brief delay (or on the next audio callback) to see
    /// whether the toggle succeeded.
    pub fn set_enabled(&self, on: bool) {
        // If the channel is closed (worker exited), the send is a
        // no-op; the standalone is shutting down anyway.
        let _ = self.toggle_tx.send(on);
    }

    /// Read the current state. Source of truth for the audio
    /// callback's zero-fill decision.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
}

/// Print available audio devices and return. Used by `--list-devices`.
pub fn list_devices() {
    let host = cpal::default_host();
    println!("=== Audio devices ===");
    println!("Output:");
    if let Ok(devices) = host.output_devices() {
        let default_name = host
            .default_output_device()
            .and_then(|d| d.name().ok())
            .unwrap_or_default();
        for d in devices {
            let name = d.name().unwrap_or_default();
            let marker = if name == default_name {
                " (default)"
            } else {
                ""
            };
            println!("  {name}{marker}");
        }
    }
    println!("Input:");
    if let Ok(devices) = host.input_devices() {
        let default_name = host
            .default_input_device()
            .and_then(|d| d.name().ok())
            .unwrap_or_default();
        for d in devices {
            let name = d.name().unwrap_or_default();
            let marker = if name == default_name {
                " (default)"
            } else {
                ""
            };
            println!("  {name}{marker}");
        }
    }
}

/// Open the requested output device, instantiate the plugin, start
/// the stream. Returns the handles the caller needs to keep the
/// stream alive and push MIDI.
pub fn start_audio<P: PluginExport>(
    opts: &Options,
) -> Result<AudioHandles<P>, Box<dyn std::error::Error>> {
    let audio_host = cpal::default_host();

    let output_device = match &opts.output_device {
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

    let default_config = output_device
        .default_output_config()
        .map_err(|e| format!("could not query default config for the audio output: {e}"))?;

    // If the caller requested a specific sample rate / buffer size,
    // ask cpal for a matching config; fall through to default on miss.
    let config: cpal::StreamConfig = resolve_config(&output_device, &default_config, opts);
    let sample_format = default_config.sample_format();
    let sample_rate = config.sample_rate.0 as f64;
    let channels = config.channels as usize;
    let is_effect = P::info().category == PluginCategory::Effect;

    let pending: Arc<Mutex<Vec<MidiEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let plugin = Arc::new(Mutex::new({
        let mut p = P::create();
        p.init();
        p.reset(sample_rate, config.buffer_size_max_frames());
        p.params().set_sample_rate(sample_rate);
        p.params().snap_smoothers();
        p
    }));

    let input_ring: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));

    // Resolve the input device lazily on the worker thread (cpal
    // Device is also !Send). The worker thread receives toggle
    // requests and opens/closes the stream against the
    // resolved-by-name device on each enable.
    let input_device_name: Option<String> = if is_effect {
        let device = match &opts.input_device {
            Some(name) => find_device(&audio_host, name, false),
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
    let has_device = input_device_name.is_some();
    let (toggle_tx, toggle_rx) = mpsc::channel::<bool>();

    let input_controller = InputController {
        enabled: Arc::clone(&input_enabled),
        has_device,
        toggle_tx: toggle_tx.clone(),
    };

    // Spawn the input-stream worker thread. It owns the cpal
    // input stream (!Send), receives toggle requests via the
    // channel, and updates `input_enabled` to reflect the actual
    // open/closed state. Exits when the channel sender drops.
    if is_effect {
        let device_name = input_device_name.clone();
        let ring = Arc::clone(&input_ring);
        let enabled_flag = Arc::clone(&input_enabled);
        let chans = channels;
        let sr = sample_rate;
        std::thread::Builder::new()
            .name("truce-standalone-input".into())
            .spawn(move || {
                input_worker(toggle_rx, device_name, chans, sr, ring, enabled_flag);
            })
            .ok();
    }

    // CLI / env / config can override the privacy default to launch
    // with mic on. Default is off when unspecified.
    let want_input_enabled = is_effect && opts.input_enabled.unwrap_or(false);
    if want_input_enabled {
        input_controller.set_enabled(true);
    }

    if is_effect {
        eprintln!(
            "Input:  {} ({})",
            input_device_name.as_deref().unwrap_or("(none)"),
            if want_input_enabled {
                "enabled"
            } else {
                "disabled — press 'I' in the window or pass --input-enabled on"
            }
        );
    }

    let transport = Transport::new(opts.bpm.unwrap_or(120.0), sample_rate);

    let plugin_audio = Arc::clone(&plugin);
    let pending_audio = Arc::clone(&pending);
    let ring_audio = Arc::clone(&input_ring);
    let transport_audio = transport.clone();
    let input_enabled_audio = Arc::clone(&input_enabled);

    let stream = match sample_format {
        cpal::SampleFormat::F32 => output_device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    audio_callback::<P>(
                        data,
                        channels,
                        sample_rate,
                        is_effect,
                        &plugin_audio,
                        &pending_audio,
                        &ring_audio,
                        &input_enabled_audio,
                        &transport_audio,
                    );
                },
                |err| eprintln!("Audio error: {err}"),
                None,
            )
            .map_err(|e| format!("could not build output stream: {e}"))?,
        format => {
            return Err(format!(
                "audio output format {format:?} is not supported (truce standalone \
                 currently only handles f32). Try a different output device."
            )
            .into())
        }
    };

    stream
        .play()
        .map_err(|e| format!("could not start output stream: {e}"))?;

    eprintln!(
        "Output: {} @ {} Hz, {} ch",
        output_device.name().unwrap_or_default(),
        sample_rate,
        channels
    );

    Ok(AudioHandles {
        _stream: stream,
        pending,
        plugin,
        sample_rate,
        channels,
        is_effect,
        input: input_controller,
        transport,
    })
}

/// Worker that owns the (`!Send`) cpal input stream + responds to
/// toggle requests on its channel. Lives for the duration of the
/// standalone process; exits when the sender side of the channel
/// is dropped (which happens when `AudioHandles` is dropped at
/// shutdown).
fn input_worker(
    toggle_rx: mpsc::Receiver<bool>,
    device_name: Option<String>,
    channels: usize,
    sample_rate: f64,
    ring: Arc<Mutex<Vec<f32>>>,
    enabled_flag: Arc<AtomicBool>,
) {
    let mut stream: Option<cpal::Stream> = None;

    while let Ok(want) = toggle_rx.recv() {
        let currently = stream.is_some();
        if want == currently {
            continue;
        }
        if want {
            // Resolve device fresh each time — the user may have
            // plugged/unplugged hardware between toggles.
            let host = cpal::default_host();
            let device = match device_name.as_ref() {
                Some(name) => find_device(&host, name, false),
                None => host.default_input_device(),
            };
            match device {
                Some(dev) => match build_and_play_input_stream(
                    &dev,
                    channels,
                    sample_rate,
                    Arc::clone(&ring),
                ) {
                    Ok(s) => {
                        stream = Some(s);
                        enabled_flag.store(true, Ordering::Relaxed);
                    }
                    Err(e) => {
                        eprintln!("[truce-standalone] mic enable failed: {e}");
                        enabled_flag.store(false, Ordering::Relaxed);
                    }
                },
                None => {
                    eprintln!("[truce-standalone] mic enable failed: no input device available");
                    enabled_flag.store(false, Ordering::Relaxed);
                }
            }
        } else {
            // Dropping the stream stops capture cleanly.
            stream = None;
            enabled_flag.store(false, Ordering::Relaxed);
            if let Ok(mut r) = ring.lock() {
                r.clear();
            }
        }
    }
    // Channel closed → drop the stream and exit.
    drop(stream);
}

/// Build an input stream against the given device that drains
/// captured samples into `ring`. Called from the worker thread.
fn build_and_play_input_stream(
    device: &cpal::Device,
    channels: usize,
    sample_rate: f64,
    ring: Arc<Mutex<Vec<f32>>>,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    let input_config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: cpal::SampleRate(sample_rate as u32),
        buffer_size: cpal::BufferSize::Default,
    };
    let stream = device
        .build_input_stream(
            &input_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if let Ok(mut buf) = ring.lock() {
                    let max_size = (sample_rate as usize) * channels / 10;
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
        .map_err(|e| -> Box<dyn std::error::Error> {
            format!("could not build input stream: {e}").into()
        })?;
    stream.play().map_err(|e| -> Box<dyn std::error::Error> {
        format!("could not start input stream: {e}").into()
    })?;
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
            .map(|n| n.to_lowercase().contains(&name.to_lowercase()))
            .unwrap_or(false)
    })
}

/// Build the cpal StreamConfig honoring opts where possible. Falls
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
                    "[truce-standalone] sample rate {sr} Hz not supported; \
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
    transport: &Transport,
) {
    let num_frames = data.len() / channels;

    let mut event_list = EventList::new();
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

    let mut channel_bufs: Vec<Vec<f32>> = (0..channels).map(|_| vec![0.0f32; num_frames]).collect();

    // Drain the input ring only when the user has explicitly
    // enabled input. When disabled (the default), channel_bufs
    // stays zeroed → plugin sees silence.
    if is_effect && input_enabled.load(Ordering::Relaxed) {
        if let Ok(mut ring) = input_ring.try_lock() {
            let needed = num_frames * channels;
            let available = ring.len().min(needed);
            for i in 0..available / channels {
                for ch in 0..channels {
                    if i < num_frames {
                        channel_bufs[ch][i] = ring[i * channels + ch];
                    }
                }
            }
            if available > 0 {
                ring.drain(..available);
            }
        }
    }

    let input_bufs: Vec<Vec<f32>> = if is_effect {
        channel_bufs.clone()
    } else {
        Vec::new()
    };
    let input_slices: Vec<&[f32]> = input_bufs.iter().map(|b| b.as_slice()).collect();
    let mut output_slices: Vec<&mut [f32]> =
        channel_bufs.iter_mut().map(|b| b.as_mut_slice()).collect();

    let mut audio_buffer =
        unsafe { AudioBuffer::from_slices(&input_slices, &mut output_slices, num_frames) };

    let transport_info = transport.tick_audio(num_frames);
    let mut output_events = EventList::new();
    let mut context =
        ProcessContext::new(&transport_info, sample_rate, num_frames, &mut output_events);

    plugin.process(&mut audio_buffer, &event_list, &mut context);

    for frame in 0..num_frames {
        for ch in 0..channels {
            let ch_idx = ch.min(channel_bufs.len() - 1);
            data[frame * channels + ch] = channel_bufs[ch_idx][frame];
        }
    }
}
