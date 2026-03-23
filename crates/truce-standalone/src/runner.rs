//! Main run loop for standalone mode (terminal, no GUI window).

use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind};
use crossterm::terminal;

use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_core::process::ProcessContext;
use truce_params::Params;

use crate::keyboard;

struct MidiEvent {
    body: EventBody,
}

/// Run the plugin as a standalone terminal application.
pub fn run<P: PluginExport>() {
    let audio_host = cpal::default_host();
    let output_device = audio_host
        .default_output_device()
        .expect("No audio output device found");

    let config = output_device.default_output_config().unwrap();
    let sample_rate = config.sample_rate().0 as f64;
    let channels = config.channels() as usize;
    let is_effect = P::info().category == PluginCategory::Effect;

    println!("=== truce Standalone ===");
    println!(
        "Plugin: {} ({})",
        P::info().name,
        if is_effect { "effect" } else { "instrument" }
    );
    println!(
        "Audio:  {} @ {} Hz, {} ch",
        output_device.name().unwrap_or_default(),
        sample_rate,
        channels
    );
    if is_effect {
        println!("Input:  system default (for effect processing)");
    }
    println!();
    println!("QWERTY keyboard -> MIDI:");
    println!("  A S D F G H J K L ; = white keys");
    println!("  W E T Y U O P       = black keys");
    println!("  Z / X = octave down / up");
    println!("  Esc   = quit");
    println!();

    let pending_events: Arc<Mutex<Vec<MidiEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let plugin = Arc::new(Mutex::new({
        let mut p = P::create();
        p.init();
        p.reset(sample_rate, 512);
        p.params().set_sample_rate(sample_rate);
        p.params().snap_smoothers();
        p
    }));

    // Input capture for effects
    let input_ring: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let _input_stream = if is_effect {
        let input_device = audio_host.default_input_device();
        if let Some(input_device) = input_device {
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
                s.play().ok();
            }
            stream
        } else {
            None
        }
    } else {
        None
    };

    let plugin_audio = Arc::clone(&plugin);
    let events_audio = Arc::clone(&pending_events);
    let ring_audio = Arc::clone(&input_ring);

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let config: cpal::StreamConfig = config.into();
            output_device
                .build_output_stream(
                    &config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        audio_callback::<P>(
                            data,
                            channels,
                            sample_rate,
                            is_effect,
                            &plugin_audio,
                            &events_audio,
                            &ring_audio,
                        );
                    },
                    |err| eprintln!("Audio error: {err}"),
                    None,
                )
                .expect("Failed to build audio stream")
        }
        format => panic!("Unsupported sample format: {format:?}"),
    };

    stream.play().expect("Failed to start audio stream");

    terminal::enable_raw_mode().expect("Failed to enable raw mode");
    let mut octave_offset: i8 = 0;

    loop {
        if event::poll(std::time::Duration::from_millis(10)).unwrap_or(false) {
            if let Ok(TermEvent::Key(key_event)) = event::read() {
                if key_event.code == KeyCode::Esc {
                    break;
                }
                if key_event.kind == KeyEventKind::Press {
                    if let Some(shift) = keyboard::key_to_octave_shift(key_event.code) {
                        octave_offset = (octave_offset + shift).clamp(-3, 3);
                        println!("\r  Octave offset: {octave_offset:+}    ");
                        continue;
                    }
                }
                if let Some(note) = keyboard::key_to_midi_note(key_event.code, octave_offset) {
                    let body = match key_event.kind {
                        KeyEventKind::Press => EventBody::NoteOn {
                            channel: 0,
                            note,
                            velocity: 0.8,
                        },
                        KeyEventKind::Release => EventBody::NoteOff {
                            channel: 0,
                            note,
                            velocity: 0.0,
                        },
                        _ => continue,
                    };
                    if let Ok(mut events) = pending_events.lock() {
                        events.push(MidiEvent { body });
                    }
                }
            }
        }
    }

    terminal::disable_raw_mode().expect("Failed to disable raw mode");
    println!("\rGoodbye!");
}

fn audio_callback<P: PluginExport>(
    data: &mut [f32],
    channels: usize,
    sample_rate: f64,
    is_effect: bool,
    plugin: &Arc<Mutex<P>>,
    pending: &Arc<Mutex<Vec<MidiEvent>>>,
    input_ring: &Arc<Mutex<Vec<f32>>>,
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

    // For effects: fill buffers from captured input
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

    let input_slices: Vec<&[f32]> = input_bufs.iter().map(|buf| buf.as_slice()).collect();
    let mut output_slices: Vec<&mut [f32]> = channel_bufs
        .iter_mut()
        .map(|buf| buf.as_mut_slice())
        .collect();

    let mut audio_buffer =
        unsafe { AudioBuffer::from_slices(&input_slices, &mut output_slices, num_frames) };

    let transport = TransportInfo::default();
    let mut output_events = EventList::new();
    let mut context = ProcessContext::new(&transport, sample_rate, num_frames, &mut output_events);

    plugin.process(&mut audio_buffer, &event_list, &mut context);

    for frame in 0..num_frames {
        for ch in 0..channels {
            let ch_idx = ch.min(channel_bufs.len() - 1);
            data[frame * channels + ch] = channel_bufs[ch_idx][frame];
        }
    }
}
