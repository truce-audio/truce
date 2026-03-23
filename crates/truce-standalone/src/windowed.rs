//! Windowed standalone host using minifb + tiny-skia GUI.

use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use minifb::{Key, MouseButton, MouseMode, Window, WindowOptions};

use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_core::process::ProcessContext;
use truce_gui::interaction::InteractionState;
use truce_gui::layout::PluginLayout;
use truce_gui::BuiltinEditor;
use truce_params::Params;

use crate::keyboard;

struct MidiEvent {
    body: EventBody,
}

/// Run the plugin standalone with a GUI window.
pub fn run_windowed<P: PluginExport>(layout: PluginLayout)
where
    P::Params: 'static,
{
    let audio_host = cpal::default_host();
    let output_device = audio_host
        .default_output_device()
        .expect("No audio output device found");
    let config = output_device.default_output_config().unwrap();
    let sample_rate = config.sample_rate().0 as f64;
    let channels = config.channels() as usize;

    let is_effect = P::info().category == PluginCategory::Effect;

    println!("=== truce Standalone (windowed) ===");
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

    let pending_events: Arc<Mutex<Vec<MidiEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let plugin = Arc::new(Mutex::new({
        let mut p = P::create();
        p.init();
        p.reset(sample_rate, 512);
        p.params().set_sample_rate(sample_rate);
        p.params().snap_smoothers();
        p
    }));

    let params_for_gui: Arc<P::Params> = {
        let p = plugin.lock().unwrap();
        p.params_arc()
    };

    // Input capture for effects: ring buffer shared between input and output callbacks
    let input_ring: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));

    // Start input stream for effects
    let _input_stream = if is_effect {
        let input_device = audio_host.default_input_device();
        if let Some(input_device) = input_device {
            println!("Input device: {}", input_device.name().unwrap_or_default());
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
                            // Keep a bounded buffer (~100ms)
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
            println!("Warning: No input device found. Effect will process silence.");
            None
        }
    } else {
        None
    };

    // GUI setup
    let w = layout.width as usize;
    let h = layout.height as usize;

    let mut window = Window::new(
        &format!("{} — Standalone", P::info().name),
        w,
        h,
        WindowOptions {
            resize: false,
            ..WindowOptions::default()
        },
    )
    .expect("Failed to create window");

    window.set_target_fps(60);

    let mut editor = BuiltinEditor::new(params_for_gui.clone(), layout.clone());
    let mut interaction = InteractionState::new();
    interaction.build_regions(&layout);
    let mut pixel_buf = vec![0u32; w * h];
    let mut octave_offset: i8 = 0;
    let mut prev_keys: Vec<Key> = Vec::new();

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

    println!("Window open. Close window or press Esc to quit.");

    while window.is_open() && !window.is_key_down(Key::Escape) {
        // --- Keyboard → MIDI ---
        let current_keys: Vec<Key> = window.get_keys();

        if window.is_key_pressed(Key::Z, minifb::KeyRepeat::No) {
            octave_offset = (octave_offset - 1).clamp(-3, 3);
        }
        if window.is_key_pressed(Key::X, minifb::KeyRepeat::No) {
            octave_offset = (octave_offset + 1).clamp(-3, 3);
        }

        for key in &current_keys {
            if !prev_keys.contains(key) {
                if let Some(note) = minifb_key_to_midi(*key, octave_offset) {
                    if let Ok(mut events) = pending_events.lock() {
                        events.push(MidiEvent {
                            body: EventBody::NoteOn {
                                channel: 0,
                                note,
                                velocity: 0.8,
                            },
                        });
                    }
                }
            }
        }

        for key in &prev_keys {
            if !current_keys.contains(key) {
                if let Some(note) = minifb_key_to_midi(*key, octave_offset) {
                    if let Ok(mut events) = pending_events.lock() {
                        events.push(MidiEvent {
                            body: EventBody::NoteOff {
                                channel: 0,
                                note,
                                velocity: 0.0,
                            },
                        });
                    }
                }
            }
        }
        prev_keys = current_keys;

        // --- Mouse → Knob interaction ---
        if let Some((mx, my)) = window.get_mouse_pos(MouseMode::Clamp) {
            if window.get_mouse_down(MouseButton::Left) {
                if interaction.dragging.is_none() {
                    if let Some(param_id) = interaction.hit_test(mx, my) {
                        if let Ok(plugin) = plugin.lock() {
                            let norm = plugin.params().get_normalized(param_id).unwrap_or(0.0);
                            interaction.begin_drag(param_id, norm, my);
                        }
                    }
                }
                if let Some((param_id, new_norm)) = interaction.update_drag(my) {
                    if let Ok(plugin) = plugin.lock() {
                        plugin.params().set_normalized(param_id, new_norm);
                    }
                }
            } else {
                interaction.end_drag();
            }
        }

        // Params are shared via Arc — GUI reads the same atomics as DSP.
        // No manual sync needed.

        // --- Render GUI ---
        let pixmap = editor.render();
        let data = pixmap.data();
        for i in 0..w * h {
            let r = data[i * 4] as u32;
            let g = data[i * 4 + 1] as u32;
            let b = data[i * 4 + 2] as u32;
            let a = data[i * 4 + 3] as u32;
            pixel_buf[i] = (a << 24) | (r << 16) | (g << 8) | b;
        }

        window
            .update_with_buffer(&pixel_buf, w, h)
            .expect("Failed to update window");
    }

    println!("Goodbye!");
}

fn minifb_key_to_midi(key: Key, octave_offset: i8) -> Option<u8> {
    use crossterm::event::KeyCode;
    let code = match key {
        Key::A => KeyCode::Char('a'),
        Key::S => KeyCode::Char('s'),
        Key::D => KeyCode::Char('d'),
        Key::F => KeyCode::Char('f'),
        Key::G => KeyCode::Char('g'),
        Key::H => KeyCode::Char('h'),
        Key::J => KeyCode::Char('j'),
        Key::K => KeyCode::Char('k'),
        Key::L => KeyCode::Char('l'),
        Key::Semicolon => KeyCode::Char(';'),
        Key::W => KeyCode::Char('w'),
        Key::E => KeyCode::Char('e'),
        Key::T => KeyCode::Char('t'),
        Key::Y => KeyCode::Char('y'),
        Key::U => KeyCode::Char('u'),
        Key::O => KeyCode::Char('o'),
        Key::P => KeyCode::Char('p'),
        _ => return None,
    };
    keyboard::key_to_midi_note(code, octave_offset)
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

    // Build deinterleaved channel buffers
    let mut channel_bufs: Vec<Vec<f32>> = (0..channels).map(|_| vec![0.0f32; num_frames]).collect();

    // For effects: fill output buffers with captured input (pass-through)
    if is_effect {
        if let Ok(mut ring) = input_ring.try_lock() {
            let needed = num_frames * channels;
            let available = ring.len().min(needed);
            // Deinterleave captured input into channel buffers
            for i in 0..available / channels {
                for ch in 0..channels {
                    if i < num_frames {
                        channel_bufs[ch][i] = ring[i * channels + ch];
                    }
                }
            }
            // Consume used samples
            if available > 0 {
                ring.drain(..available);
            }
        }
    }

    // For effects: use separate input buffers (already filled from ring).
    // For instruments: no input buffers.
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

    // Interleave output back to cpal buffer
    for frame in 0..num_frames {
        for ch in 0..channels {
            let ch_idx = ch.min(channel_bufs.len() - 1);
            data[frame * channels + ch] = channel_bufs[ch_idx][frame];
        }
    }
}
