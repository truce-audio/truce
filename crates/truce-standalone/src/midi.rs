//! MIDI device input via `midir`.
//!
//! Entry points:
//!
//! - [`list_midi`] - print available MIDI input devices and return.
//!   Exposed as the `--list-midi` CLI flag.
//! - [`list_midi_devices`] - the same list as a `Vec<String>`, for the
//!   native device menus.
//! - [`MidiInputThread`] - the background thread that owns the `midir`
//!   connection. Always running; it connects to whatever device the
//!   [`MidiController`] points it at, decodes note / CC / bend events
//!   into the audio thread's queue, and polls for hot-plug. Dropping it
//!   stops the thread.
//! - [`MidiController`] - `Send + Sync` handle the menu / CLI use to
//!   switch the input device and pick a channel filter live.

use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use midir::{MidiInput, MidiInputConnection};

use truce_core::events::EventBody;
use truce_core::midi::pitch_bend_from_bytes;

use crate::audio::MidiEvent;
use crate::cli::Options;
use crate::vlog;

const HOTPLUG_POLL: Duration = Duration::from_secs(1);

/// `channel_filter` sentinel meaning "accept every channel".
const OMNI: u8 = 0xFF;

/// Print available MIDI input devices.
pub fn list_midi() {
    let names = list_midi_devices();
    println!("MIDI inputs");
    if names.is_empty() {
        println!("  (none)");
    } else {
        for name in names {
            println!("  {name}");
        }
    }
}

/// MIDI input port names, in `midir` order. Empty if MIDI is
/// unavailable. Used by the native MIDI-input menus.
#[must_use]
pub fn list_midi_devices() -> Vec<String> {
    let Ok(midi_in) = MidiInput::new("truce-standalone-enum") else {
        return Vec::new();
    };
    midi_in
        .ports()
        .iter()
        .filter_map(|p| midi_in.port_name(p).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Channel filter
// ---------------------------------------------------------------------------

/// Which MIDI channel the input forwards to the plugin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MidiChannel {
    /// Accept every channel (the default).
    Omni,
    /// Accept only this channel. 0-based internally; the menu / CLI
    /// present it 1-based (1-16).
    Channel(u8),
}

impl MidiChannel {
    /// Pack into the `AtomicU8` the controller stores. `Omni` is an
    /// out-of-range sentinel (`0xFF`) so 0-15 stay real channels.
    #[must_use]
    pub fn encode(self) -> u8 {
        match self {
            MidiChannel::Omni => OMNI,
            MidiChannel::Channel(n) => n,
        }
    }

    /// Inverse of [`Self::encode`]; anything out of `0..16` is `Omni`.
    #[must_use]
    pub fn decode(v: u8) -> Self {
        if v < 16 {
            MidiChannel::Channel(v)
        } else {
            MidiChannel::Omni
        }
    }

    /// Parse a CLI / env spec: `omni` / `all` → [`Self::Omni`], or a
    /// 1-based channel `1`..=`16` → [`Self::Channel`]. `None` if
    /// malformed or out of range.
    #[must_use]
    pub fn parse(spec: &str) -> Option<Self> {
        let s = spec.trim().to_ascii_lowercase();
        if s == "omni" || s == "all" {
            return Some(MidiChannel::Omni);
        }
        let n: u8 = s.parse().ok()?;
        (1..=16).contains(&n).then(|| MidiChannel::Channel(n - 1))
    }
}

// ---------------------------------------------------------------------------
// MidiController
// ---------------------------------------------------------------------------

enum MidiCmd {
    /// Switch the input device by substring. `None` disconnects (the
    /// runner's QWERTY keyboard still feeds notes in windowed mode).
    SetDevice(Option<String>),
}

/// `Send + Sync` handle for steering the MIDI input thread from the UI
/// thread. Cloneable; clones share the worker.
#[derive(Clone)]
pub struct MidiController {
    cmd_tx: mpsc::Sender<MidiCmd>,
    /// Worker mirrors the connected device name here so the menu can
    /// check the active device. `None` = nothing connected.
    current_name: Arc<Mutex<Option<String>>>,
    /// Channel filter, read live in the `midir` callback. [`OMNI`] or a
    /// 0-based channel.
    channel: Arc<AtomicU8>,
}

impl MidiController {
    /// Switch the input device by name substring (`None` to
    /// disconnect). Applied immediately by the worker.
    pub fn set_device(&self, name: Option<String>) {
        let _ = self.cmd_tx.send(MidiCmd::SetDevice(name));
    }

    /// Name of the currently-connected device, or `None`.
    #[must_use]
    pub fn current_name(&self) -> Option<String> {
        self.current_name.lock().ok().and_then(|g| g.clone())
    }

    /// Restrict the input to one channel (or `Omni` for all). Takes
    /// effect on the next incoming message.
    pub fn set_channel(&self, channel: MidiChannel) {
        self.channel.store(channel.encode(), Ordering::Relaxed);
    }

    /// The current channel filter.
    #[must_use]
    pub fn channel(&self) -> MidiChannel {
        MidiChannel::decode(self.channel.load(Ordering::Relaxed))
    }
}

// ---------------------------------------------------------------------------
// MidiInputThread
// ---------------------------------------------------------------------------

/// Background MIDI-input thread. Dropping stops the thread.
pub struct MidiInputThread {
    stop: Arc<AtomicBool>,
    // The midir connection is held inside the thread; we only keep
    // the stop flag on the outside.
}

impl MidiInputThread {
    /// Start the MIDI input thread and return it alongside the
    /// [`MidiController`] that steers it. The thread always runs (so a
    /// device picked later from the menu connects), starting on
    /// `opts.midi_input` / `opts.midi_channel` if set.
    pub fn start(opts: &Options, pending: Arc<ArrayQueue<MidiEvent>>) -> (Self, MidiController) {
        let stop = Arc::new(AtomicBool::new(false));
        let current_name = Arc::new(Mutex::new(None));
        let initial_channel = opts
            .midi_channel
            .as_deref()
            .and_then(MidiChannel::parse)
            .unwrap_or(MidiChannel::Omni);
        let channel = Arc::new(AtomicU8::new(initial_channel.encode()));
        let (cmd_tx, cmd_rx) = mpsc::channel::<MidiCmd>();

        let initial_device = opts.midi_input.clone();
        let stop_thread = Arc::clone(&stop);
        let current_thread = Arc::clone(&current_name);
        let channel_thread = Arc::clone(&channel);
        std::thread::Builder::new()
            .name("truce-standalone-midi".into())
            .spawn(move || {
                midi_thread(
                    initial_device,
                    cmd_rx,
                    pending,
                    stop_thread,
                    current_thread,
                    channel_thread,
                );
            })
            .ok();

        let controller = MidiController {
            cmd_tx,
            current_name,
            channel,
        };
        (Self { stop }, controller)
    }
}

impl Drop for MidiInputThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// Spawned-thread body - owns its state across the worker's lifetime.
#[allow(clippy::needless_pass_by_value)]
fn midi_thread(
    initial_device: Option<String>,
    cmd_rx: mpsc::Receiver<MidiCmd>,
    pending: Arc<ArrayQueue<MidiEvent>>,
    stop: Arc<AtomicBool>,
    current_name: Arc<Mutex<Option<String>>>,
    channel: Arc<AtomicU8>,
) {
    let mut desired = initial_device;
    let mut connection: Option<MidiInputConnection<()>> = None;
    let mut connected_name = String::new();

    while !stop.load(Ordering::Relaxed) {
        reconcile(
            desired.as_deref(),
            &mut connection,
            &mut connected_name,
            &pending,
            &channel,
            &current_name,
        );

        // Block until the menu changes the device or the hot-plug poll
        // fires. `recv_timeout` keeps device switches immediate while
        // still re-checking presence every `HOTPLUG_POLL`.
        match cmd_rx.recv_timeout(HOTPLUG_POLL) {
            Ok(MidiCmd::SetDevice(name)) => desired = name,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    drop(connection);
}

/// Bring `connection` in line with `desired`: drop it if the desired
/// device changed / vanished, then (re)connect when a matching port is
/// present.
fn reconcile(
    desired: Option<&str>,
    connection: &mut Option<MidiInputConnection<()>>,
    connected_name: &mut String,
    pending: &Arc<ArrayQueue<MidiEvent>>,
    channel: &Arc<AtomicU8>,
    current_name: &Arc<Mutex<Option<String>>>,
) {
    if connection.is_some() {
        let keep = match desired {
            None => false,
            Some(want) => {
                port_present(connected_name)
                    && connected_name.to_lowercase().contains(&want.to_lowercase())
            }
        };
        if !keep {
            if !connected_name.is_empty() {
                vlog!("MIDI input: disconnected from {connected_name}");
            }
            *connection = None;
            connected_name.clear();
            set_current(current_name, None);
        }
    }

    if connection.is_none()
        && let Some(want) = desired
        && let Some((conn, name)) = try_connect(want, pending, channel)
    {
        vlog!("MIDI input: {name}");
        *connection = Some(conn);
        connected_name.clone_from(&name);
        set_current(current_name, Some(name));
    }
}

/// Is a MIDI input port with this exact name still present?
fn port_present(name: &str) -> bool {
    MidiInput::new("truce-standalone-probe").is_ok_and(|midi_in| {
        midi_in
            .ports()
            .iter()
            .any(|p| midi_in.port_name(p).is_ok_and(|n| n == name))
    })
}

/// Open the first port whose name contains `requested`. The callback
/// applies the live channel filter before decoding.
fn try_connect(
    requested: &str,
    pending: &Arc<ArrayQueue<MidiEvent>>,
    channel: &Arc<AtomicU8>,
) -> Option<(MidiInputConnection<()>, String)> {
    let midi_in = MidiInput::new("truce-standalone").ok()?;
    let ports = midi_in.ports();
    let port = ports.iter().find(|p| {
        midi_in
            .port_name(p)
            .is_ok_and(|n| n.to_lowercase().contains(&requested.to_lowercase()))
    })?;
    let name = midi_in
        .port_name(port)
        .unwrap_or_else(|_| "<unnamed>".into());

    let pending = Arc::clone(pending);
    let channel = Arc::clone(channel);
    let conn = midi_in
        .connect(
            port,
            "truce-standalone-in",
            move |_t, bytes, ()| {
                // Channel-voice messages (0x80-0xEF) carry a channel in
                // the low nibble; drop them when a single-channel filter
                // is active and they're on another channel. System
                // messages (>= 0xF0) have no channel and pass through.
                if let Some(&status) = bytes.first()
                    && (0x80..=0xEF).contains(&status)
                {
                    let filter = channel.load(Ordering::Relaxed);
                    if filter != OMNI && filter != (status & 0x0F) {
                        return;
                    }
                }
                if let Some(body) = decode_midi(bytes) {
                    // `force_push` drops the oldest event on overflow.
                    // The audio thread is the only consumer; a flooded
                    // queue means the callback is starved, where
                    // dropping ancient note events beats mutex
                    // contention.
                    let _ = pending.force_push(MidiEvent { body });
                }
            },
            (),
        )
        .ok()?;
    Some((conn, name))
}

fn set_current(current_name: &Arc<Mutex<Option<String>>>, value: Option<String>) {
    if let Ok(mut g) = current_name.lock() {
        *g = value;
    }
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
            group: 0,
            channel,
            note: bytes[1],
            velocity: bytes[2],
        }),
        // NoteOn with velocity 0 is NoteOff per MIDI spec.
        0x90 if bytes.len() >= 3 => Some(EventBody::NoteOff {
            group: 0,
            channel,
            note: bytes[1],
            velocity: 0,
        }),
        0x80 if bytes.len() >= 3 => Some(EventBody::NoteOff {
            group: 0,
            channel,
            note: bytes[1],
            velocity: bytes[2],
        }),
        0xA0 if bytes.len() >= 3 => Some(EventBody::Aftertouch {
            group: 0,
            channel,
            note: bytes[1],
            pressure: bytes[2],
        }),
        0xB0 if bytes.len() >= 3 => Some(EventBody::ControlChange {
            group: 0,
            channel,
            cc: bytes[1],
            value: bytes[2],
        }),
        0xE0 if bytes.len() >= 3 => Some(EventBody::PitchBend {
            group: 0,
            channel,
            value: pitch_bend_from_bytes(bytes[1], bytes[2]),
        }),
        0xD0 if bytes.len() >= 2 => Some(EventBody::ChannelPressure {
            group: 0,
            channel,
            pressure: bytes[1],
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::MidiChannel;

    #[test]
    fn channel_encode_decode_roundtrips() {
        assert_eq!(
            MidiChannel::decode(MidiChannel::Omni.encode()),
            MidiChannel::Omni
        );
        for n in 0..16u8 {
            let c = MidiChannel::Channel(n);
            assert_eq!(MidiChannel::decode(c.encode()), c);
        }
    }

    #[test]
    fn channel_parse() {
        assert_eq!(MidiChannel::parse("omni"), Some(MidiChannel::Omni));
        assert_eq!(MidiChannel::parse(" ALL "), Some(MidiChannel::Omni));
        assert_eq!(MidiChannel::parse("1"), Some(MidiChannel::Channel(0)));
        assert_eq!(MidiChannel::parse("16"), Some(MidiChannel::Channel(15)));
        assert_eq!(MidiChannel::parse("0"), None);
        assert_eq!(MidiChannel::parse("17"), None);
        assert_eq!(MidiChannel::parse("x"), None);
    }
}
