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

/// Most plugin MIDI input ports the platform menus build a device
/// picker for, shared by the macOS and Windows menu bars so the two
/// platforms cap identically. Ports past the cap still route via the
/// repeatable `--midi-input` CLI flag, which has no cap. Linux has no
/// menu bar, so no consumer exists there.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) const MIDI_MENU_MAX_PORTS: usize = 16;

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
    /// Point a plugin MIDI port at a device (by name substring), or
    /// `None` to disconnect it. The runner's QWERTY keyboard still
    /// feeds port 0 in windowed mode.
    SetDevice { port: u8, name: Option<String> },
}

/// `Send + Sync` handle for steering the MIDI input thread from the UI
/// thread. Cloneable; clones share the worker.
#[derive(Clone)]
pub struct MidiController {
    cmd_tx: mpsc::Sender<MidiCmd>,
    /// Worker mirrors each port's connected device name here (indexed
    /// by plugin MIDI input port) so the per-port menus can checkmark
    /// the active device. `None` = that port has nothing connected.
    current_names: Arc<Mutex<Vec<Option<String>>>>,
    /// Number of plugin MIDI input port slots (>= 1). The menus build
    /// this many device submenus.
    port_count: usize,
    /// Channel filter, read live in the `midir` callback. [`OMNI`] or a
    /// 0-based channel.
    channel: Arc<AtomicU8>,
}

impl MidiController {
    /// Switch the device feeding the plugin's first MIDI port (`None`
    /// to disconnect). The interactive menus are single-device, so
    /// this is the port they steer; CLI `--midi-input` feeds the
    /// higher ports. Applied immediately by the worker.
    pub fn set_device(&self, name: Option<String>) {
        self.set_device_on(0, name);
    }

    /// Point a specific plugin MIDI port at a device (`None` to
    /// disconnect it).
    pub fn set_device_on(&self, port: u8, name: Option<String>) {
        let _ = self.cmd_tx.send(MidiCmd::SetDevice { port, name });
    }

    /// Name of the device feeding the plugin's first MIDI port, or
    /// `None`.
    #[must_use]
    pub fn current_name(&self) -> Option<String> {
        self.current_name_on(0)
    }

    /// Name of the device feeding plugin MIDI port `port`, or `None`.
    #[must_use]
    pub fn current_name_on(&self, port: u8) -> Option<String> {
        self.current_names
            .lock()
            .ok()
            .and_then(|g| g.get(usize::from(port)).cloned().flatten())
    }

    /// Number of plugin MIDI input ports the menus should expose (>= 1).
    #[must_use]
    pub fn port_count(&self) -> usize {
        self.port_count
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
    /// `opts.midi_inputs` / `opts.midi_channel` if set.
    ///
    /// `num_ports` is the plugin's declared MIDI input port count
    /// (`PluginInfo::midi_input_ports`); the i-th `--midi-input`
    /// device is routed to port i. At least one slot always exists so
    /// the single-device case and the QWERTY keyboard keep feeding
    /// port 0.
    pub fn start(
        opts: &Options,
        num_ports: usize,
        pending: Arc<ArrayQueue<MidiEvent>>,
    ) -> (Self, MidiController) {
        let stop = Arc::new(AtomicBool::new(false));
        let slots = num_ports.max(1);
        let current_names = Arc::new(Mutex::new(vec![None; slots]));
        let initial_channel = opts
            .midi_channel
            .as_deref()
            .and_then(MidiChannel::parse)
            .unwrap_or(MidiChannel::Omni);
        let channel = Arc::new(AtomicU8::new(initial_channel.encode()));
        let (cmd_tx, cmd_rx) = mpsc::channel::<MidiCmd>();

        if opts.midi_inputs.len() > slots {
            eprintln!(
                "(--midi-input: {} devices given but the plugin has {slots} MIDI input port(s); \
                 ignoring the extra device(s))",
                opts.midi_inputs.len(),
            );
        }
        let initial_devices = opts.midi_inputs.clone();

        let stop_thread = Arc::clone(&stop);
        let names_thread = Arc::clone(&current_names);
        let channel_thread = Arc::clone(&channel);
        std::thread::Builder::new()
            .name("truce-standalone-midi".into())
            .spawn(move || {
                midi_thread(
                    initial_devices,
                    slots,
                    cmd_rx,
                    pending,
                    stop_thread,
                    names_thread,
                    channel_thread,
                );
            })
            .ok();

        let controller = MidiController {
            cmd_tx,
            current_names,
            port_count: slots,
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

/// One plugin MIDI input port's device binding, owned by the worker.
struct Slot {
    /// The plugin MIDI input port this slot feeds.
    port: u8,
    /// Device name substring to connect to, or `None` (disconnected).
    desired: Option<String>,
    connection: Option<MidiInputConnection<()>>,
    connected_name: String,
}

// Spawned-thread body - owns its state across the worker's lifetime.
#[allow(clippy::needless_pass_by_value)]
fn midi_thread(
    initial_devices: Vec<String>,
    num_slots: usize,
    cmd_rx: mpsc::Receiver<MidiCmd>,
    pending: Arc<ArrayQueue<MidiEvent>>,
    stop: Arc<AtomicBool>,
    current_names: Arc<Mutex<Vec<Option<String>>>>,
    channel: Arc<AtomicU8>,
) {
    // One slot per plugin MIDI input port; the i-th initial
    // `--midi-input` seeds port i. `port` is `u8` because it maps to
    // `Event::port`; the slot count never exceeds the plugin's port
    // count, which is itself `u8`.
    #[allow(clippy::cast_possible_truncation)]
    let mut slots: Vec<Slot> = (0..num_slots)
        .map(|i| Slot {
            port: i as u8,
            desired: initial_devices.get(i).cloned(),
            connection: None,
            connected_name: String::new(),
        })
        .collect();

    while !stop.load(Ordering::Relaxed) {
        for slot in &mut slots {
            reconcile(slot, &pending, &channel, &current_names);
        }

        // Block until the menu changes a device or the hot-plug poll
        // fires. `recv_timeout` keeps device switches immediate while
        // still re-checking presence every `HOTPLUG_POLL`.
        match cmd_rx.recv_timeout(HOTPLUG_POLL) {
            Ok(MidiCmd::SetDevice { port, name }) => {
                if let Some(slot) = slots.iter_mut().find(|s| s.port == port) {
                    slot.desired = name;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Connections drop with `slots`.
}

/// Bring one slot's connection in line with its `desired` device:
/// drop it if the desired device changed / vanished, then (re)connect
/// when a matching port is present. Mirrors the slot's device name
/// into `current_names[port]` so the per-port menus checkmark it.
fn reconcile(
    slot: &mut Slot,
    pending: &Arc<ArrayQueue<MidiEvent>>,
    channel: &Arc<AtomicU8>,
    current_names: &Arc<Mutex<Vec<Option<String>>>>,
) {
    if slot.connection.is_some() {
        let keep = match slot.desired.as_deref() {
            None => false,
            Some(want) => {
                port_present(&slot.connected_name)
                    && slot
                        .connected_name
                        .to_lowercase()
                        .contains(&want.to_lowercase())
            }
        };
        if !keep {
            if !slot.connected_name.is_empty() {
                vlog!(
                    "MIDI input (port {}): disconnected from {}",
                    slot.port,
                    slot.connected_name
                );
            }
            slot.connection = None;
            slot.connected_name.clear();
            set_current(current_names, slot.port, None);
        }
    }

    if slot.connection.is_none()
        && let Some(want) = slot.desired.clone()
        && let Some((conn, name)) = try_connect(&want, pending, channel, slot.port)
    {
        vlog!("MIDI input (port {}): {name}", slot.port);
        slot.connection = Some(conn);
        slot.connected_name.clone_from(&name);
        set_current(current_names, slot.port, Some(name));
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

/// Open the first device whose name contains `requested` and route
/// its events to plugin MIDI input `port`. The callback applies the
/// live channel filter before decoding and stamps the port.
fn try_connect(
    requested: &str,
    pending: &Arc<ArrayQueue<MidiEvent>>,
    channel: &Arc<AtomicU8>,
    port: u8,
) -> Option<(MidiInputConnection<()>, String)> {
    let midi_in = MidiInput::new("truce-standalone").ok()?;
    let ports = midi_in.ports();
    let midi_port = ports.iter().find(|p| {
        midi_in
            .port_name(p)
            .is_ok_and(|n| n.to_lowercase().contains(&requested.to_lowercase()))
    })?;
    let name = midi_in
        .port_name(midi_port)
        .unwrap_or_else(|_| "<unnamed>".into());

    let pending = Arc::clone(pending);
    let channel = Arc::clone(channel);
    let conn = midi_in
        .connect(
            midi_port,
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
                    let _ = pending.force_push(MidiEvent { body, port });
                }
            },
            (),
        )
        .ok()?;
    Some((conn, name))
}

fn set_current(current_names: &Arc<Mutex<Vec<Option<String>>>>, port: u8, value: Option<String>) {
    if let Ok(mut g) = current_names.lock()
        && let Some(slot) = g.get_mut(usize::from(port))
    {
        *slot = value;
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
