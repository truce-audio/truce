//! MIDI device input via `midir`.
//!
//! Two entry points:
//!
//! - [`list_midi`] — print available MIDI input devices and return.
//!   Exposed as the `--list-midi` CLI flag.
//! - [`MidiInputThread`] — open a MIDI input, forward decoded note /
//!   CC / bend events into the audio thread's queue, and poll every
//!   second for hot-plug (auto-reconnect on replug, fall-through to
//!   QWERTY on unplug). Held by the windowed / headless runners; drop
//!   stops the thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use midir::{MidiInput, MidiInputConnection};

use truce_core::events::EventBody;

use crate::audio::MidiEvent;
use crate::cli::Options;

const HOTPLUG_POLL: Duration = Duration::from_secs(1);

/// Print available MIDI input devices.
pub fn list_midi() {
    match MidiInput::new("truce-standalone-list") {
        Ok(midi_in) => {
            let ports = midi_in.ports();
            println!("=== MIDI inputs ===");
            if ports.is_empty() {
                println!("  (none)");
            } else {
                for port in &ports {
                    let name = midi_in
                        .port_name(port)
                        .unwrap_or_else(|_| "<unnamed>".into());
                    println!("  {name}");
                }
            }
        }
        Err(e) => eprintln!("[truce-standalone] MIDI init failed: {e}"),
    }
}

/// Background MIDI-input thread. Dropping stops the thread.
pub struct MidiInputThread {
    stop: Arc<AtomicBool>,
    // The midir connection is held inside the thread; we only keep
    // the stop flag on the outside.
}

impl MidiInputThread {
    /// Start a MIDI input thread for `opts.midi_input`. Returns
    /// `None` if no MIDI device was requested, or if the requested
    /// device isn't present on startup (in which case the thread
    /// still starts so it can auto-connect on hot-plug).
    pub fn start(opts: &Options, pending: Arc<Mutex<Vec<MidiEvent>>>) -> Option<Self> {
        let requested = opts.midi_input.clone()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);

        std::thread::Builder::new()
            .name("truce-standalone-midi".into())
            .spawn(move || midi_thread(requested, pending, stop_thread))
            .ok()?;

        Some(Self { stop })
    }
}

impl Drop for MidiInputThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn midi_thread(requested: String, pending: Arc<Mutex<Vec<MidiEvent>>>, stop: Arc<AtomicBool>) {
    let mut connection: Option<MidiInputConnection<()>> = None;
    let mut current_name = String::new();

    while !stop.load(Ordering::Relaxed) {
        // Already connected? Verify the device is still present.
        if let Some(_conn) = &connection {
            let still_present = {
                let midi_in = match MidiInput::new("truce-standalone-probe") {
                    Ok(m) => m,
                    Err(_) => {
                        std::thread::sleep(HOTPLUG_POLL);
                        continue;
                    }
                };
                midi_in.ports().iter().any(|p| {
                    midi_in
                        .port_name(p)
                        .map(|n| n == current_name)
                        .unwrap_or(false)
                })
            };
            if !still_present {
                eprintln!(
                    "[truce-standalone] MIDI device '{current_name}' \
                     disconnected — falling back to QWERTY."
                );
                connection = None;
                current_name.clear();
            }
        }

        // Not connected? Try to find + open the requested device.
        if connection.is_none() {
            let midi_in = match MidiInput::new("truce-standalone") {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[truce-standalone] MIDI init failed: {e}");
                    std::thread::sleep(HOTPLUG_POLL);
                    continue;
                }
            };
            let ports = midi_in.ports();
            let matched = ports.iter().find(|p| {
                midi_in
                    .port_name(p)
                    .map(|n| n.to_lowercase().contains(&requested.to_lowercase()))
                    .unwrap_or(false)
            });
            if let Some(port) = matched {
                let name = midi_in
                    .port_name(port)
                    .unwrap_or_else(|_| "<unnamed>".into());
                let pending = Arc::clone(&pending);
                match midi_in.connect(
                    port,
                    "truce-standalone-in",
                    move |_t, bytes, _| {
                        if let Some(body) = decode_midi(bytes) {
                            if let Ok(mut q) = pending.lock() {
                                q.push(MidiEvent { body });
                            }
                        }
                    },
                    (),
                ) {
                    Ok(conn) => {
                        eprintln!("[truce-standalone] MIDI input: {name}");
                        connection = Some(conn);
                        current_name = name;
                    }
                    Err(e) => {
                        eprintln!("[truce-standalone] MIDI connect failed: {e}");
                    }
                }
            }
        }

        std::thread::sleep(HOTPLUG_POLL);
    }

    drop(connection);
}

fn decode_midi(bytes: &[u8]) -> Option<EventBody> {
    if bytes.is_empty() {
        return None;
    }
    let status = bytes[0];
    let channel = status & 0x0F;
    let kind = status & 0xF0;

    match kind {
        0x90 if bytes.len() >= 3 && bytes[2] > 0 => Some(EventBody::NoteOn {
            channel,
            note: bytes[1],
            velocity: bytes[2] as f32 / 127.0,
        }),
        // NoteOn with velocity 0 is NoteOff per MIDI spec.
        0x90 if bytes.len() >= 3 => Some(EventBody::NoteOff {
            channel,
            note: bytes[1],
            velocity: 0.0,
        }),
        0x80 if bytes.len() >= 3 => Some(EventBody::NoteOff {
            channel,
            note: bytes[1],
            velocity: bytes[2] as f32 / 127.0,
        }),
        0xB0 if bytes.len() >= 3 => Some(EventBody::ControlChange {
            channel,
            cc: bytes[1],
            value: bytes[2] as f32 / 127.0,
        }),
        0xE0 if bytes.len() >= 3 => {
            let raw = (bytes[1] as u16) | ((bytes[2] as u16) << 7);
            let normalized = (raw as f32 - 8192.0) / 8192.0;
            Some(EventBody::PitchBend {
                channel,
                value: normalized,
            })
        }
        0xD0 if bytes.len() >= 2 => Some(EventBody::ChannelPressure {
            channel,
            pressure: bytes[1] as f32 / 127.0,
        }),
        _ => None,
    }
}
