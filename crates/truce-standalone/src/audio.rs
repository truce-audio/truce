//! Shared cpal audio setup + callback. One implementation used by
//! both `windowed` and `headless` runners.

use std::sync::{Arc, Mutex};

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
pub struct AudioHandles<P: PluginExport> {
    /// The live output stream. Hold on to it — dropping it stops audio.
    pub _stream: cpal::Stream,
    /// Held only for effects; dropping stops input capture. `None` for
    /// instrument plugins.
    pub _input_stream: Option<cpal::Stream>,
    /// Event queue the caller pushes MIDI into; drained by the audio
    /// callback each block.
    pub pending: Arc<Mutex<Vec<MidiEvent>>>,
    /// Plugin instance shared between caller and audio callback.
    pub plugin: Arc<Mutex<P>>,
    /// Audio config (sample rate, channels) resolved from the device.
    pub sample_rate: f64,
    pub channels: usize,
    pub is_effect: bool,
    /// Shared transport state; UI thread toggles play/stop, audio
    /// thread advances position each block.
    pub transport: Transport,
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
pub fn start_audio<P: PluginExport>(opts: &Options) -> AudioHandles<P> {
    let audio_host = cpal::default_host();

    let output_device = match &opts.output_device {
        Some(name) => find_device(&audio_host, name, true)
            .unwrap_or_else(|| panic!("no output device matching '{name}'")),
        None => audio_host
            .default_output_device()
            .expect("no default audio output device"),
    };

    let default_config = output_device
        .default_output_config()
        .expect("no default output config");

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
    let input_stream = if is_effect {
        let input_device = match &opts.input_device {
            Some(name) => find_device(&audio_host, name, false),
            None => audio_host.default_input_device(),
        };
        match input_device {
            Some(input_device) => {
                eprintln!("Input:  {}", input_device.name().unwrap_or_default());
                let input_config = cpal::StreamConfig {
                    channels: channels as u16,
                    sample_rate: cpal::SampleRate(sample_rate as u32),
                    buffer_size: cpal::BufferSize::Default,
                };
                let ring = Arc::clone(&input_ring);
                let stream = input_device
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
                    .ok();
                if let Some(ref s) = stream {
                    let _ = s.play();
                }
                stream
            }
            None => {
                eprintln!("Warning: no input device — effect will process silence.");
                None
            }
        }
    } else {
        None
    };

    let transport = Transport::new(opts.bpm.unwrap_or(120.0), sample_rate);

    let plugin_audio = Arc::clone(&plugin);
    let pending_audio = Arc::clone(&pending);
    let ring_audio = Arc::clone(&input_ring);
    let transport_audio = transport.clone();

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
                        &transport_audio,
                    );
                },
                |err| eprintln!("Audio error: {err}"),
                None,
            )
            .expect("Failed to build output stream"),
        format => panic!("Unsupported sample format: {format:?}"),
    };

    stream.play().expect("Failed to start output stream");

    eprintln!(
        "Output: {} @ {} Hz, {} ch",
        output_device.name().unwrap_or_default(),
        sample_rate,
        channels
    );

    AudioHandles {
        _stream: stream,
        _input_stream: input_stream,
        pending,
        plugin,
        sample_rate,
        channels,
        is_effect,
        transport,
    }
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

    if is_effect {
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
