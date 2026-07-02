//! Universal MIDI Packet (UMP) codec for MIDI 2.0 channel-voice
//! messages.
//!
//! UMP is the MIDI 2.0 wire format. Channel-voice 2.0 messages live
//! in 64-bit packets: word 0 carries `mt | group | status |
//! channel | <status-specific 16 bits>`, word 1 carries the
//! status-specific value (velocity + attribute, controller value,
//! pitch-bend, etc.). Many UMP transports embed 64-bit packets in a
//! fixed 128-bit slot, so [`decode_ump_channel_voice_2`] and
//! [`encode_ump_channel_voice_2`] work in terms of `[u32; 4]` with the
//! upper two words zeroed for channel-voice. Encoding is gated behind a
//! port's declared [`crate::MidiDialect::Midi2`] so a MIDI-1.0 host
//! never receives UMP.
//!
//! Spec reference: MIDI 2.0 M2-104-UM §4.1.
//!
//! Format wrappers that speak UMP (AU v3 on macOS 12+ / iOS 15+ via
//! `MIDIEventList`, CLAP's `CLAP_EVENT_MIDI2`) call into here so the
//! channel-voice + `SysEx` codecs aren't reimplemented per wrapper.
//!
//! MIDI 1.0 channel voice over UMP (mt 0x2) has an *encoder*
//! ([`encode_ump_channel_voice_1`]) for emitting 1.0 events on a
//! 2.0-protocol transport; there's no mt-0x2 decoder yet (wrappers
//! receive 1.0 as bytes). Out of scope: utility (mt 0x0), system
//! real-time (mt 0x1), flex-data (mt 0xD), UMP stream (mt 0xF).
//! `SysEx`-7 (mt 0x3) has both the assembler and an encoder
//! ([`encode_sysex7_packet`]); `SysEx`-8 / data (mt 0x5) has the
//! assembler only; everything else awaits demand.

use crate::events::EventBody;

/// UMP message type for MIDI 2.0 channel-voice messages.
const MT_CHANNEL_VOICE_2: u8 = 0x4;
/// UMP message type for 7-bit `SysEx` payloads.
const MT_SYSEX_7: u8 = 0x3;
/// UMP message type for 8-bit `SysEx` / data payloads.
const MT_SYSEX_8: u8 = 0x5;

/// `SysEx` packet status nibble shared by `SysEx`-7 (mt 0x3) and
/// `SysEx`-8 (mt 0x5). Lives in word 0 bits 23..20.
const SYSEX_STATUS_COMPLETE: u8 = 0x0;
const SYSEX_STATUS_START: u8 = 0x1;
const SYSEX_STATUS_CONTINUE: u8 = 0x2;
const SYSEX_STATUS_END: u8 = 0x3;

/// Decode the first two words of a UMP packet into a MIDI 2.0
/// channel-voice [`EventBody`]. Returns `None` for non-channel-voice
/// packets (utility, system, `SysEx`, data) - those are handled by
/// dedicated decoders (or not at all). `words[2]` and `words[3]` are
/// ignored - channel-voice 2.0 is 64 bits and the upper half of a
/// 128-bit slot is undefined for it.
#[must_use]
#[allow(clippy::cast_possible_truncation)] // UMP fields are bit-packed; truncation is intentional
pub fn decode_ump_channel_voice_2(words: [u32; 4]) -> Option<EventBody> {
    // Bit layout (word 0):
    //   31..28 mt (message type, 0x4 = MIDI 2.0 CV)
    //   27..24 group (0..=15)
    //   23..20 status nibble (0x8 = NoteOff, 0x9 = NoteOn, ...)
    //   19..16 channel (0..=15)
    //   15..0  status-specific (note + attribute-type, cc number, ...)
    let w0 = words[0];
    let w1 = words[1];
    let mt = ((w0 >> 28) & 0xF) as u8;
    if mt != MT_CHANNEL_VOICE_2 {
        return None;
    }
    let group = ((w0 >> 24) & 0xF) as u8;
    let status = ((w0 >> 20) & 0xF) as u8;
    let channel = ((w0 >> 16) & 0xF) as u8;
    let byte_a = ((w0 >> 8) & 0xFF) as u8; // note / cc number / etc.
    let byte_b = (w0 & 0xFF) as u8; // attribute-type / index / etc.
    let body = match status {
        0x8 => EventBody::NoteOff2 {
            group,
            channel,
            note: byte_a & 0x7F,
            velocity: (w1 >> 16) as u16,
            attribute_type: byte_b,
            attribute: (w1 & 0xFFFF) as u16,
        },
        0x9 => EventBody::NoteOn2 {
            group,
            channel,
            note: byte_a & 0x7F,
            velocity: (w1 >> 16) as u16,
            attribute_type: byte_b,
            attribute: (w1 & 0xFFFF) as u16,
        },
        0xA => EventBody::PolyPressure2 {
            group,
            channel,
            note: byte_a & 0x7F,
            pressure: w1,
        },
        // 0x0 = Registered Per-Note (RPN-like), 0x1 = Assignable
        // Per-Note. MIDI 2.0 §4.1.4. The lower 8 bits of word 0
        // carry the per-note controller index; word 1 is the value.
        0x0 | 0x1 => EventBody::PerNoteCC {
            group,
            channel,
            note: byte_a & 0x7F,
            cc: byte_b,
            value: w1,
            registered: status == 0x0,
        },
        // 0x6 = Per-Note Pitch Bend.
        0x6 => EventBody::PerNotePitchBend {
            group,
            channel,
            note: byte_a & 0x7F,
            value: w1,
        },
        // 0xF = Per-Note Management. The flags live in byte_b (per
        // §4.1.6); only the low two bits are defined today.
        0xF => EventBody::PerNoteManagement {
            group,
            channel,
            note: byte_a & 0x7F,
            flags: byte_b,
        },
        0xB => EventBody::ControlChange2 {
            group,
            channel,
            cc: byte_a & 0x7F,
            value: w1,
        },
        0xD => EventBody::ChannelPressure2 {
            group,
            channel,
            pressure: w1,
        },
        0xE => EventBody::PitchBend2 {
            group,
            channel,
            value: w1,
        },
        // 0x2 = Registered Controller (RPN), 0x3 = Assignable
        // Controller (NRPN). Bank lives in `byte_a` (lower 7 bits),
        // index in `byte_b` (lower 7 bits).
        0x2 => EventBody::RegisteredController {
            group,
            channel,
            bank: byte_a & 0x7F,
            index: byte_b & 0x7F,
            value: w1,
        },
        0x3 => EventBody::AssignableController {
            group,
            channel,
            bank: byte_a & 0x7F,
            index: byte_b & 0x7F,
            value: w1,
        },
        0xC => EventBody::ProgramChange2 {
            group,
            channel,
            program: (w1 >> 24) as u8 & 0x7F,
            // Word 0 bit 0 carries the "B" (bank-valid) flag; the
            // bank bytes live in word 1's bottom 16 bits (MSB then
            // LSB).
            bank: if w0 & 0x01 == 1 {
                Some(((w1 >> 8) as u8 & 0x7F, w1 as u8 & 0x7F))
            } else {
                None
            },
        },
        _ => return None,
    };
    Some(body)
}

/// Encode a MIDI 2.0 channel-voice [`EventBody`] into a 64-bit UMP
/// packet, returned in the low two words of a `[u32; 4]` (the upper two
/// are zero, matching the 128-bit slot [`decode_ump_channel_voice_2`]
/// reads). Returns `None` for bodies that aren't MIDI 2.0 channel voice
/// (MIDI 1.0 variants, transport, param automation, `SysEx`) - those ride
/// their own emit paths. Only a port whose declared dialect is
/// [`crate::MidiDialect::Midi2`] routes output through here; MIDI 1.0
/// ports keep down-converting, so a dormant encoder can't leak 2.0
/// packets to a host that negotiated 1.0.
#[must_use]
pub fn encode_ump_channel_voice_2(body: &EventBody) -> Option<[u32; 4]> {
    // Inverse of `decode_ump_channel_voice_2`: `byte_a` is word 0 bits
    // 15..8 (note / cc / bank), `byte_b` bits 7..0 (attribute-type /
    // index / flags / bank-valid), and `w1` the 32-bit value word.
    let (status, byte_a, byte_b, w1): (u8, u8, u8, u32) = match *body {
        EventBody::NoteOff2 {
            note,
            velocity,
            attribute_type,
            attribute,
            ..
        } => (0x8, note, attribute_type, cv2_value16(velocity, attribute)),
        EventBody::NoteOn2 {
            note,
            velocity,
            attribute_type,
            attribute,
            ..
        } => (0x9, note, attribute_type, cv2_value16(velocity, attribute)),
        EventBody::PolyPressure2 { note, pressure, .. } => (0xA, note, 0, pressure),
        EventBody::PerNoteCC {
            note,
            cc,
            value,
            registered,
            ..
            // Status 0x0 = registered per-note controller, 0x1 = assignable.
        } => (u8::from(!registered), note, cc, value),
        EventBody::PerNotePitchBend { note, value, .. } => (0x6, note, 0, value),
        EventBody::PerNoteManagement { note, flags, .. } => (0xF, note, flags, 0),
        EventBody::ControlChange2 { cc, value, .. } => (0xB, cc, 0, value),
        EventBody::ChannelPressure2 { pressure, .. } => (0xD, 0, 0, pressure),
        EventBody::PitchBend2 { value, .. } => (0xE, 0, 0, value),
        EventBody::RegisteredController {
            bank, index, value, ..
        } => (0x2, bank, index, value),
        EventBody::AssignableController {
            bank, index, value, ..
        } => (0x3, bank, index, value),
        EventBody::ProgramChange2 { program, bank, .. } => {
            // "B" (bank-valid) flag rides byte_b bit 0; the bank pair
            // sits in word 1 bytes 1..0, program in word 1 byte 3.
            let (option, w1) = match bank {
                Some((msb, lsb)) => (
                    0x01,
                    (u32::from(program & 0x7F) << 24)
                        | (u32::from(msb & 0x7F) << 8)
                        | u32::from(lsb & 0x7F),
                ),
                None => (0x00, u32::from(program & 0x7F) << 24),
            };
            (0xC, 0, option, w1)
        }
        _ => return None,
    };
    let (group, channel) = cv2_addr(body)?;
    let w0 = (0x4 << 28)
        | (u32::from(group & 0x0F) << 24)
        | (u32::from(status) << 20)
        | (u32::from(channel & 0x0F) << 16)
        | (u32::from(byte_a) << 8)
        | u32::from(byte_b);
    Some([w0, w1, 0, 0])
}

const fn cv2_value16(hi: u16, lo: u16) -> u32 {
    ((hi as u32) << 16) | lo as u32
}

/// UMP message type for MIDI 1.0 channel-voice messages (one word).
const MT_CHANNEL_VOICE_1: u8 = 0x2;

/// Encode a MIDI 1.0 channel-voice [`EventBody`] into a 32-bit UMP
/// packet (message type 0x2), returned in the low word of a `[u32; 4]`.
/// Used to carry a plugin's MIDI 1.0 output over a UMP transport (AU
/// v3's `midiOutputEventListBlock`) when the negotiated protocol is 2.0,
/// where the byte-based output path is unavailable so even 1.0 events
/// must ride UMP. Returns `None` for bodies that aren't MIDI 1.0 channel
/// voice (the 2.0 variants go through [`encode_ump_channel_voice_2`]).
#[must_use]
pub fn encode_ump_channel_voice_1(body: &EventBody) -> Option<[u32; 4]> {
    let (opcode, channel, group, data1, data2): (u8, u8, u8, u8, u8) = match *body {
        EventBody::NoteOff {
            group,
            channel,
            note,
            velocity,
        } => (0x8, channel, group, note, velocity),
        EventBody::NoteOn {
            group,
            channel,
            note,
            velocity,
        } => (0x9, channel, group, note, velocity),
        EventBody::Aftertouch {
            group,
            channel,
            note,
            pressure,
        } => (0xA, channel, group, note, pressure),
        EventBody::ControlChange {
            group,
            channel,
            cc,
            value,
        } => (0xB, channel, group, cc, value),
        EventBody::ProgramChange {
            group,
            channel,
            program,
        } => (0xC, channel, group, program, 0),
        EventBody::ChannelPressure {
            group,
            channel,
            pressure,
        } => (0xD, channel, group, pressure, 0),
        EventBody::PitchBend {
            group,
            channel,
            value,
        } => {
            // 14-bit value splits into LSB (low 7 bits) then MSB. The
            // masked `try_from`s can't fail, so no truncating cast.
            let lsb = u8::try_from(value & 0x7F).unwrap_or(0);
            let msb = u8::try_from((value >> 7) & 0x7F).unwrap_or(0);
            (0xE, channel, group, lsb, msb)
        }
        _ => return None,
    };
    let w0 = (u32::from(MT_CHANNEL_VOICE_1) << 28)
        | (u32::from(group & 0x0F) << 24)
        | (u32::from((opcode << 4) | (channel & 0x0F)) << 16)
        | (u32::from(data1 & 0x7F) << 8)
        | u32::from(data2 & 0x7F);
    Some([w0, 0, 0, 0])
}

/// Payload bytes carried per `SysEx`-7 UMP: the 64-bit packet holds
/// 16 bits of header + 6 data slots.
const SYSEX_7_BYTES_PER_PACKET: usize = 6;

/// Number of `SysEx`-7 UMPs needed to carry `payload_len` inner bytes.
/// A zero-length message still takes one `Complete` packet.
#[must_use]
pub const fn sysex7_packet_count(payload_len: usize) -> usize {
    if payload_len == 0 {
        1
    } else {
        payload_len.div_ceil(SYSEX_7_BYTES_PER_PACKET)
    }
}

/// Encode packet `packet_index` of the `SysEx`-7 chain carrying
/// `payload` (the inner bytes - no `0xF0` / `0xF7` framing, which UMP
/// doesn't transmit). A payload of up to 6 bytes is one `Complete`
/// packet; longer payloads chain `Start` / `Continue`… / `End`. Returns
/// `None` past the end of the chain ([`sysex7_packet_count`] gives its
/// length). Data bytes are masked to 7 bits per spec.
///
/// The inverse of [`SysExAssembler::push_sysex7_packet`]: feeding the
/// full chain through the assembler yields `payload` back.
#[must_use]
pub fn encode_sysex7_packet(group: u8, payload: &[u8], packet_index: usize) -> Option<[u32; 2]> {
    let total = sysex7_packet_count(payload.len());
    if packet_index >= total {
        return None;
    }
    let start = packet_index * SYSEX_7_BYTES_PER_PACKET;
    let chunk = &payload[start..(start + SYSEX_7_BYTES_PER_PACKET).min(payload.len())];
    let status = match (total, packet_index) {
        (1, _) => SYSEX_STATUS_COMPLETE,
        (_, 0) => SYSEX_STATUS_START,
        (_, i) if i == total - 1 => SYSEX_STATUS_END,
        _ => SYSEX_STATUS_CONTINUE,
    };
    let mut padded = [0u8; SYSEX_7_BYTES_PER_PACKET];
    for (dst, src) in padded.iter_mut().zip(chunk) {
        *dst = src & 0x7F;
    }
    // chunk.len() is 0..=6 by construction.
    #[allow(clippy::cast_possible_truncation)]
    let n = chunk.len() as u32;
    let w0 = (u32::from(MT_SYSEX_7) << 28)
        | (u32::from(group & 0x0F) << 24)
        | (u32::from(status) << 20)
        | (n << 16)
        | (u32::from(padded[0]) << 8)
        | u32::from(padded[1]);
    let w1 = (u32::from(padded[2]) << 24)
        | (u32::from(padded[3]) << 16)
        | (u32::from(padded[4]) << 8)
        | u32::from(padded[5]);
    Some([w0, w1])
}

/// Pull `(group, channel)` off any channel-voice body. Returns `None`
/// for non-channel-voice bodies (which `encode_ump_channel_voice_2`
/// has already rejected before calling this).
fn cv2_addr(body: &EventBody) -> Option<(u8, u8)> {
    Some(match *body {
        EventBody::NoteOff2 { group, channel, .. }
        | EventBody::NoteOn2 { group, channel, .. }
        | EventBody::PolyPressure2 { group, channel, .. }
        | EventBody::PerNoteCC { group, channel, .. }
        | EventBody::PerNotePitchBend { group, channel, .. }
        | EventBody::PerNoteManagement { group, channel, .. }
        | EventBody::ControlChange2 { group, channel, .. }
        | EventBody::ChannelPressure2 { group, channel, .. }
        | EventBody::PitchBend2 { group, channel, .. }
        | EventBody::RegisteredController { group, channel, .. }
        | EventBody::AssignableController { group, channel, .. }
        | EventBody::ProgramChange2 { group, channel, .. } => (group, channel),
        _ => return None,
    })
}

/// One reassembled `SysEx` payload, in the form
/// [`crate::events::EventList::push_sysex`] expects: just the inner
/// bytes (no leading `0xF0`, no trailing `0xF7`), plus the UMP
/// routing keys (`group` + `stream_id`) for callers that care
/// about per-stream demux. `stream_id` is always 0 for `SysEx`-7
/// (the format has no stream identifier).
pub struct SysExPacket<'a> {
    /// UMP group (0..=15) the message arrived on.
    pub group: u8,
    /// `SysEx`-8 stream identifier (0..=255); always 0 for
    /// `SysEx`-7.
    pub stream_id: u8,
    /// The reassembled inner bytes. Valid until the next call into
    /// the assembler.
    pub bytes: &'a [u8],
}

/// What [`SysExAssembler::push_sysex7_packet`] /
/// [`SysExAssembler::push_sysex8_packet`] does with the input UMP.
pub enum SysExFeed<'a> {
    /// Packet was a `Continue` / `Start` - buffered, nothing to
    /// emit yet.
    Buffered,
    /// Packet was `Complete` or `End` - `payload` is ready to push
    /// to the host's event list. The slice is invalidated by the
    /// next call into the assembler.
    Complete(SysExPacket<'a>),
    /// Packet was malformed (length > 6 for `SysEx`-7, > 13 for
    /// `SysEx`-8, or status nibble we don't recognise). Caller
    /// should drop the message; assembler state is unchanged.
    Invalid,
    /// Buffer overflowed before the `End` packet arrived. The
    /// partial message has been dropped; the caller may want to
    /// surface this via a counter.
    Overflow,
}

/// Maximum number of concurrent `SysEx` streams the assembler
/// reassembles in parallel. `(group, stream_id)` identifies each
/// stream uniquely; a fifth concurrent stream evicts the
/// least-recently-touched one (dropping its in-progress message).
///
/// 4 is enough for any host pattern we've observed: a single
/// MIDI 2.0 host typically uses one stream per group, and four
/// simultaneously-active groups is already past the realistic
/// upper bound for `SysEx` traffic in a single audio block.
pub const SYSEX_ASSEMBLER_SLOTS: usize = 4;

struct StreamSlot {
    /// Pre-allocated buffer for this slot's in-progress message.
    /// Sized at construction; never grows on the audio thread.
    buffer: Vec<u8>,
    group: u8,
    stream_id: u8,
    /// `true` between `Start` and `End`; `false` when the slot
    /// holds the bytes from a just-completed `Complete` / `End`
    /// (waiting for the caller to read them before reuse).
    in_progress: bool,
    /// `true` when the slot is allocated to a stream. Distinct
    /// from `in_progress` so a just-completed slot can hold its
    /// bytes for the caller's borrow without being evictable on
    /// the same call.
    in_use: bool,
    /// Monotonic counter set on every packet that touches the
    /// slot, used as the LRU key when allocation needs to evict.
    last_touch: u64,
}

/// Stateful reassembler for UMP `SysEx` streams.
///
/// Maintains [`SYSEX_ASSEMBLER_SLOTS`] independent buffers, each
/// keyed by `(group, stream_id)`, so hosts that interleave
/// `SysEx` traffic across UMP groups (or across `SysEx`-8 streams
/// within one group) don't see corrupt concatenations.
///
/// Each slot's buffer is bounded by the per-slot capacity passed
/// to [`Self::with_capacity`]; pushing past it returns
/// [`SysExFeed::Overflow`] and discards that slot's partial
/// message - truncated `SysEx` is corrupt by definition.
pub struct SysExAssembler {
    slots: [StreamSlot; SYSEX_ASSEMBLER_SLOTS],
    /// Monotonically increases on every packet; used to break ties
    /// when LRU-evicting a slot to make room for a new stream.
    touch_counter: u64,
}

impl SysExAssembler {
    /// Allocate per-slot buffers up front. `capacity` is the
    /// largest `SysEx` payload (in bytes) **per stream** the
    /// assembler will accept - total memory is
    /// `SYSEX_ASSEMBLER_SLOTS × capacity`.
    ///
    /// Matching `capacity` to the consuming
    /// [`crate::events::EventList::sysex_pool_capacity`] is the
    /// typical choice; smaller values trade memory for the
    /// maximum single-message length.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        // Per-slot init done by array literal - each `StreamSlot`
        // allocates its own `Vec::with_capacity(capacity)`.
        let slots = std::array::from_fn(|_| StreamSlot {
            buffer: Vec::with_capacity(capacity),
            group: 0,
            stream_id: 0,
            in_progress: false,
            in_use: false,
            last_touch: 0,
        });
        Self {
            slots,
            touch_counter: 0,
        }
    }

    /// Drop every in-progress message and free every slot. Call
    /// between `process()` blocks when the host's contract
    /// guarantees no `SysEx` continues across the block boundary,
    /// or on the first packet of a fresh session.
    pub fn reset(&mut self) {
        for slot in &mut self.slots {
            slot.buffer.clear();
            slot.in_progress = false;
            slot.in_use = false;
            slot.last_touch = 0;
        }
        self.touch_counter = 0;
    }

    /// Find the slot currently servicing `(group, stream_id)`, or
    /// `None` if no slot matches. Returns the slot index.
    fn find_slot(&self, group: u8, stream_id: u8) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.in_use && s.group == group && s.stream_id == stream_id)
    }

    /// Claim a slot for `(group, stream_id)` - preferring an empty
    /// one, falling back to LRU eviction. Eviction drops the
    /// victim's in-progress message (we have no way to surface
    /// the loss back to the host other than the eventual missing
    /// final message).
    fn claim_slot(&mut self, group: u8, stream_id: u8) -> usize {
        // Pick: empty slot if any; otherwise the least-recently-
        // touched one. `unwrap` on the LRU fallback is safe because
        // the slot table is fixed-size and non-empty by construction.
        let idx = self
            .slots
            .iter()
            .position(|s| !s.in_use)
            .unwrap_or_else(|| {
                self.slots
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, s)| s.last_touch)
                    .map(|(i, _)| i)
                    .expect("non-empty slot table")
            });
        let slot = &mut self.slots[idx];
        slot.buffer.clear();
        slot.group = group;
        slot.stream_id = stream_id;
        slot.in_use = true;
        slot.in_progress = false;
        idx
    }

    /// Feed one `SysEx`-7 UMP (`words[0]`, `words[1]`). Group is
    /// extracted from word 0 bits 27..24; `stream_id` is always 0
    /// (the format reserves no slot for it).
    #[allow(clippy::cast_possible_truncation)] // UMP bit-packing
    pub fn push_sysex7_packet(&mut self, words: [u32; 2]) -> SysExFeed<'_> {
        let w0 = words[0];
        let w1 = words[1];
        let mt = ((w0 >> 28) & 0xF) as u8;
        if mt != MT_SYSEX_7 {
            return SysExFeed::Invalid;
        }
        let group = ((w0 >> 24) & 0xF) as u8;
        let status = ((w0 >> 20) & 0xF) as u8;
        let n = ((w0 >> 16) & 0xF) as u8;
        if n > 6 {
            return SysExFeed::Invalid;
        }
        // Bytes packed into the bottom 16 bits of w0 + all of w1 -
        // each in its own 8-bit slot, top bit always 0 per spec.
        let raw = [
            ((w0 >> 8) & 0xFF) as u8,
            (w0 & 0xFF) as u8,
            ((w1 >> 24) & 0xFF) as u8,
            ((w1 >> 16) & 0xFF) as u8,
            ((w1 >> 8) & 0xFF) as u8,
            (w1 & 0xFF) as u8,
        ];
        self.feed_payload(group, 0, status, &raw[..n as usize])
    }

    /// Feed one `SysEx`-8 UMP (all four words). Group at word 0
    /// bits 27..24; `stream_id` at word 0 bits 15..8 (`SysEx`-8
    /// reserves one byte for a per-group stream identifier so
    /// hosts can interleave concurrent `SysEx` payloads).
    #[allow(clippy::cast_possible_truncation)] // UMP bit-packing
    pub fn push_sysex8_packet(&mut self, words: [u32; 4]) -> SysExFeed<'_> {
        let w0 = words[0];
        let mt = ((w0 >> 28) & 0xF) as u8;
        if mt != MT_SYSEX_8 {
            return SysExFeed::Invalid;
        }
        let group = ((w0 >> 24) & 0xF) as u8;
        let status = ((w0 >> 20) & 0xF) as u8;
        let n = ((w0 >> 16) & 0xF) as u8;
        let stream_id = ((w0 >> 8) & 0xFF) as u8;
        // `SysEx`-8 reserves 1 byte for `stream_id`, leaving 13
        // bytes for payload; `n` is the count of those payload
        // bytes used in this packet.
        if n > 13 {
            return SysExFeed::Invalid;
        }
        // word 0: stream_id at bits 15..8, byte 0 at bits 7..0
        // words 1..3: bytes 1..12, MSB-first
        let raw = [
            (w0 & 0xFF) as u8, // byte 0
            ((words[1] >> 24) & 0xFF) as u8,
            ((words[1] >> 16) & 0xFF) as u8,
            ((words[1] >> 8) & 0xFF) as u8,
            (words[1] & 0xFF) as u8,
            ((words[2] >> 24) & 0xFF) as u8,
            ((words[2] >> 16) & 0xFF) as u8,
            ((words[2] >> 8) & 0xFF) as u8,
            (words[2] & 0xFF) as u8,
            ((words[3] >> 24) & 0xFF) as u8,
            ((words[3] >> 16) & 0xFF) as u8,
            ((words[3] >> 8) & 0xFF) as u8,
            (words[3] & 0xFF) as u8,
        ];
        self.feed_payload(group, stream_id, status, &raw[..n as usize])
    }

    fn feed_payload(
        &mut self,
        group: u8,
        stream_id: u8,
        status: u8,
        bytes: &[u8],
    ) -> SysExFeed<'_> {
        self.touch_counter += 1;
        let now = self.touch_counter;

        match status {
            SYSEX_STATUS_COMPLETE => {
                // Single-packet message - claim a slot, fill it,
                // mark it not-in-progress so the next call can
                // evict it. Reuse any existing slot for this
                // (group, stream_id) (in case the previous stream
                // for this pair leaked an in-progress state).
                let idx = match self.find_slot(group, stream_id) {
                    Some(i) => i,
                    None => self.claim_slot(group, stream_id),
                };
                let slot = &mut self.slots[idx];
                slot.buffer.clear();
                if slot.buffer.capacity() < bytes.len() {
                    // Release the slot on overflow so the next call
                    // can reclaim it - otherwise an oversize
                    // `Complete` would leave an `in_use` slot
                    // occupying the table forever (until LRU evicted
                    // it manually). Mirror the `Start` arm.
                    slot.in_progress = false;
                    slot.in_use = false;
                    slot.last_touch = now;
                    return SysExFeed::Overflow;
                }
                slot.buffer.extend_from_slice(bytes);
                slot.in_progress = false;
                slot.last_touch = now;
                SysExFeed::Complete(SysExPacket {
                    group,
                    stream_id,
                    bytes: &slot.buffer,
                })
            }
            SYSEX_STATUS_START => {
                let idx = match self.find_slot(group, stream_id) {
                    Some(i) => i,
                    None => self.claim_slot(group, stream_id),
                };
                let slot = &mut self.slots[idx];
                slot.buffer.clear();
                if slot.buffer.capacity() < bytes.len() {
                    slot.in_progress = false;
                    slot.in_use = false;
                    slot.last_touch = now;
                    return SysExFeed::Overflow;
                }
                slot.buffer.extend_from_slice(bytes);
                slot.in_progress = true;
                slot.last_touch = now;
                SysExFeed::Buffered
            }
            SYSEX_STATUS_CONTINUE | SYSEX_STATUS_END => {
                let Some(idx) = self.find_slot(group, stream_id) else {
                    // Out-of-band continuation - drop.
                    return SysExFeed::Invalid;
                };
                let slot = &mut self.slots[idx];
                if !slot.in_progress {
                    return SysExFeed::Invalid;
                }
                if slot.buffer.len() + bytes.len() > slot.buffer.capacity() {
                    slot.buffer.clear();
                    slot.in_progress = false;
                    slot.in_use = false;
                    slot.last_touch = now;
                    return SysExFeed::Overflow;
                }
                slot.buffer.extend_from_slice(bytes);
                slot.last_touch = now;
                if status == SYSEX_STATUS_END {
                    slot.in_progress = false;
                    SysExFeed::Complete(SysExPacket {
                        group,
                        stream_id,
                        bytes: &slot.buffer,
                    })
                } else {
                    SysExFeed::Buffered
                }
            }
            _ => SysExFeed::Invalid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_note_on_2() {
        // Hand-crafted UMP 2.0 channel-voice NoteOn:
        // mt=0x4, group=0, status=0x9, channel=2, note=60,
        // velocity=0x8000, attribute_type=3, attribute=0x1234.
        let w0 = (0x4u32 << 28) | (0x9u32 << 20) | (0x2u32 << 16) | (60u32 << 8) | 0x03;
        let w1 = (0x8000u32 << 16) | 0x1234;
        let decoded = decode_ump_channel_voice_2([w0, w1, 0, 0]).expect("decodes");
        if let EventBody::NoteOn2 {
            channel,
            note,
            velocity,
            attribute_type,
            attribute,
            ..
        } = decoded
        {
            assert_eq!(channel, 2);
            assert_eq!(note, 60);
            assert_eq!(velocity, 0x8000);
            assert_eq!(attribute_type, 3);
            assert_eq!(attribute, 0x1234);
        } else {
            panic!("expected NoteOn2");
        }
    }

    #[test]
    fn non_channel_voice_packet_returns_none() {
        // mt = 0x0 (utility)
        assert!(decode_ump_channel_voice_2([0x0000_0000, 0, 0, 0]).is_none());
        // mt = 0x3 (SysEx-7)
        assert!(decode_ump_channel_voice_2([0x3000_0000, 0, 0, 0]).is_none());
    }

    // `EventBody` has no `PartialEq` (it carries float-bearing
    // `TransportInfo`), so assert the encode/decode pair is mutually
    // consistent by re-encoding the decoded body and comparing the
    // 128-bit packet. `encode_bits_match_spec` anchors the encoder to
    // the wire independently, so a shared-bug false pass can't hide.
    #[track_caller]
    fn cv2_round_trip(body: EventBody) {
        let packet = encode_ump_channel_voice_2(&body).expect("2.0 channel voice encodes");
        let decoded = decode_ump_channel_voice_2(packet).expect("decodes");
        let re_encoded = encode_ump_channel_voice_2(&decoded).expect("re-encodes");
        assert_eq!(packet, re_encoded, "round trip mismatch for {body:?}");
    }

    #[test]
    fn encode_bits_match_spec() {
        // mt=0x4, group=4, status=0x9 (NoteOn), channel=2, note=64,
        // attribute_type=3; word 1 = velocity<<16 | attribute.
        let packet = encode_ump_channel_voice_2(&EventBody::NoteOn2 {
            group: 4,
            channel: 2,
            note: 64,
            velocity: 0xBEEF,
            attribute_type: 3,
            attribute: 0x1234,
        })
        .unwrap();
        assert_eq!(
            packet,
            [
                (0x4 << 28) | (4 << 24) | (0x9 << 20) | (2 << 16) | (64 << 8) | 0x03,
                (0xBEEF << 16) | 0x1234,
                0,
                0,
            ]
        );
    }

    #[test]
    fn channel_voice_2_round_trips() {
        cv2_round_trip(EventBody::NoteOn2 {
            group: 4,
            channel: 2,
            note: 64,
            velocity: 0xBEEF,
            attribute_type: 3,
            attribute: 0x1234,
        });
        cv2_round_trip(EventBody::NoteOff2 {
            group: 0,
            channel: 15,
            note: 127,
            velocity: 0,
            attribute_type: 0,
            attribute: 0,
        });
        cv2_round_trip(EventBody::ControlChange2 {
            group: 15,
            channel: 0,
            cc: 11,
            value: 0xDEAD_BEEF,
        });
        cv2_round_trip(EventBody::PerNoteCC {
            group: 1,
            channel: 3,
            note: 72,
            cc: 5,
            value: 0x0102_0304,
            registered: true,
        });
        cv2_round_trip(EventBody::PerNoteCC {
            group: 1,
            channel: 3,
            note: 72,
            cc: 5,
            value: 0x0102_0304,
            registered: false,
        });
        cv2_round_trip(EventBody::PitchBend2 {
            group: 1,
            channel: 8,
            value: 0x8000_0000,
        });
        cv2_round_trip(EventBody::RegisteredController {
            group: 2,
            channel: 4,
            bank: 1,
            index: 2,
            value: 0xCAFE_0000,
        });
        cv2_round_trip(EventBody::PolyPressure2 {
            group: 0,
            channel: 0,
            note: 60,
            pressure: 0x1234_5678,
        });
        cv2_round_trip(EventBody::PerNoteManagement {
            group: 0,
            channel: 0,
            note: 60,
            flags: 0x03,
        });
    }

    #[test]
    fn program_change_2_bank_option_round_trips() {
        cv2_round_trip(EventBody::ProgramChange2 {
            group: 0,
            channel: 0,
            program: 10,
            bank: Some((3, 7)),
        });
        cv2_round_trip(EventBody::ProgramChange2 {
            group: 0,
            channel: 0,
            program: 10,
            bank: None,
        });
    }

    #[test]
    fn group_nibble_survives_round_trip() {
        for group in 0..=15u8 {
            let packet = encode_ump_channel_voice_2(&EventBody::NoteOn2 {
                group,
                channel: 0,
                note: 60,
                velocity: 1,
                attribute_type: 0,
                attribute: 0,
            })
            .unwrap();
            let Some(EventBody::NoteOn2 { group: g, .. }) = decode_ump_channel_voice_2(packet)
            else {
                panic!("expected NoteOn2");
            };
            assert_eq!(g, group);
        }
    }

    #[test]
    fn non_channel_voice_2_body_does_not_encode() {
        // MIDI 1.0 variants and automation aren't 2.0 channel voice.
        assert!(
            encode_ump_channel_voice_2(&EventBody::NoteOn {
                group: 0,
                channel: 0,
                note: 60,
                velocity: 100,
            })
            .is_none()
        );
        assert!(
            encode_ump_channel_voice_2(&EventBody::ParamChange { id: 0, value: 0.0 }).is_none()
        );
    }

    #[test]
    fn channel_voice_1_encodes_mt2() {
        // NoteOn: mt=0x2, group=3, status=0x9|channel(5), note=60, vel=100.
        let packet = encode_ump_channel_voice_1(&EventBody::NoteOn {
            group: 3,
            channel: 5,
            note: 60,
            velocity: 100,
        })
        .expect("note on encodes");
        assert_eq!(
            packet,
            [
                (0x2 << 28) | (0x3 << 24) | (0x95 << 16) | (0x3C << 8) | 0x64,
                0,
                0,
                0
            ]
        );

        // PitchBend 14-bit splits LSB then MSB: 0x2000 -> lsb 0, msb 0x40.
        let bend = encode_ump_channel_voice_1(&EventBody::PitchBend {
            group: 0,
            channel: 0,
            value: 0x2000,
        })
        .unwrap();
        assert_eq!(bend[0], (0x2 << 28) | (0xE0 << 16) | 0x40);

        // 2.0 variants and automation don't encode as MT 0x2.
        assert!(
            encode_ump_channel_voice_1(&EventBody::NoteOn2 {
                group: 0,
                channel: 0,
                note: 60,
                velocity: 1,
                attribute_type: 0,
                attribute: 0,
            })
            .is_none()
        );
    }

    // -- SysEx-7 assembler --

    fn sysex7_packet(status: u8, bytes: &[u8]) -> [u32; 2] {
        assert!(bytes.len() <= 6);
        // assert above bounds `len()` to 0..=6, well within `u32`.
        #[allow(clippy::cast_possible_truncation)]
        let n = bytes.len() as u32;
        let mut padded = [0u8; 6];
        padded[..bytes.len()].copy_from_slice(bytes);
        // group is implicitly 0 - `<< 24` would be a no-op so we omit it.
        let w0 = (0x3u32 << 28)
            | (u32::from(status) << 20)
            | (n << 16)
            | (u32::from(padded[0]) << 8)
            | u32::from(padded[1]);
        let w1 = (u32::from(padded[2]) << 24)
            | (u32::from(padded[3]) << 16)
            | (u32::from(padded[4]) << 8)
            | u32::from(padded[5]);
        [w0, w1]
    }

    #[test]
    fn assembler_single_complete_packet() {
        let mut a = SysExAssembler::with_capacity(64);
        let packet = sysex7_packet(SYSEX_STATUS_COMPLETE, &[0x7E, 0x00, 0x06, 0x01]);
        match a.push_sysex7_packet(packet) {
            SysExFeed::Complete(p) => assert_eq!(p.bytes, &[0x7E, 0x00, 0x06, 0x01]),
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn assembler_multi_packet_reassembly() {
        let mut a = SysExAssembler::with_capacity(64);
        // Start: 6 bytes.
        let start = sysex7_packet(SYSEX_STATUS_START, &[1, 2, 3, 4, 5, 6]);
        assert!(matches!(a.push_sysex7_packet(start), SysExFeed::Buffered));
        // Continue: 6 more bytes.
        let cont = sysex7_packet(SYSEX_STATUS_CONTINUE, &[7, 8, 9, 10, 11, 12]);
        assert!(matches!(a.push_sysex7_packet(cont), SysExFeed::Buffered));
        // End: 3 bytes.
        let end = sysex7_packet(SYSEX_STATUS_END, &[13, 14, 15]);
        match a.push_sysex7_packet(end) {
            SysExFeed::Complete(p) => assert_eq!(
                p.bytes,
                &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
            ),
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn assembler_overflow_returns_overflow_and_drops_partial() {
        let mut a = SysExAssembler::with_capacity(8); // tiny
        let start = sysex7_packet(SYSEX_STATUS_START, &[1, 2, 3, 4, 5, 6]);
        assert!(matches!(a.push_sysex7_packet(start), SysExFeed::Buffered));
        // 6 + 6 > 8 → overflow.
        let cont = sysex7_packet(SYSEX_STATUS_CONTINUE, &[7, 8, 9, 10, 11, 12]);
        assert!(matches!(a.push_sysex7_packet(cont), SysExFeed::Overflow));
        // After overflow the assembler is reset; a fresh Start works.
        let start2 = sysex7_packet(SYSEX_STATUS_COMPLETE, &[42]);
        match a.push_sysex7_packet(start2) {
            SysExFeed::Complete(p) => assert_eq!(p.bytes, &[42]),
            _ => panic!("expected Complete after reset"),
        }
    }

    #[test]
    fn assembler_continue_without_start_is_invalid() {
        let mut a = SysExAssembler::with_capacity(64);
        let cont = sysex7_packet(SYSEX_STATUS_CONTINUE, &[1, 2, 3]);
        assert!(matches!(a.push_sysex7_packet(cont), SysExFeed::Invalid));
    }

    #[test]
    fn assembler_complete_overflow_releases_slot() {
        // Per-slot capacity smaller than a single COMPLETE message.
        // After the overflow, the slot must be releasable so a
        // later stream on a fresh `(group, stream_id)` can still
        // claim it instead of getting LRU-evicted.
        let mut a = SysExAssembler::with_capacity(4);
        let oversize = sysex7_packet(SYSEX_STATUS_COMPLETE, &[1, 2, 3, 4, 5]);
        assert!(matches!(
            a.push_sysex7_packet(oversize),
            SysExFeed::Overflow
        ));
        // Three more streams on distinct groups should now all
        // claim cleanly: the overflowed slot is back in the pool.
        for group in 1..=3u8 {
            let p = sysex7_packet_for_group(group, SYSEX_STATUS_START, &[group]);
            assert!(matches!(a.push_sysex7_packet(p), SysExFeed::Buffered));
        }
        // And the fourth too - confirms total slot count is `SYSEX_ASSEMBLER_SLOTS`,
        // not `SYSEX_ASSEMBLER_SLOTS - 1`.
        let p = sysex7_packet_for_group(4, SYSEX_STATUS_START, &[4]);
        assert!(matches!(a.push_sysex7_packet(p), SysExFeed::Buffered));
    }

    #[test]
    fn assembler_reset_drops_partial() {
        let mut a = SysExAssembler::with_capacity(64);
        let start = sysex7_packet(SYSEX_STATUS_START, &[1, 2, 3]);
        assert!(matches!(a.push_sysex7_packet(start), SysExFeed::Buffered));
        a.reset();
        // After reset, a fresh Continue should fail (no in-progress).
        let cont = sysex7_packet(SYSEX_STATUS_CONTINUE, &[4]);
        assert!(matches!(a.push_sysex7_packet(cont), SysExFeed::Invalid));
    }

    #[test]
    fn assembler_sysex8_complete_packet() {
        let mut a = SysExAssembler::with_capacity(64);
        // SysEx-8: mt=0x5, status=0 (complete), n=4, stream_id=0,
        // payload [0xAA, 0xBB, 0xCC, 0xDD] in bytes 0..3.
        // status=0 means we don't need to shift anything into bits 23..20.
        let w0 = (0x5u32 << 28) | (4u32 << 16) | 0xAA;
        let w1 = (0xBBu32 << 24) | (0xCCu32 << 16) | (0xDDu32 << 8);
        match a.push_sysex8_packet([w0, w1, 0, 0]) {
            SysExFeed::Complete(p) => {
                assert_eq!(p.bytes, &[0xAA, 0xBB, 0xCC, 0xDD]);
                assert_eq!(p.group, 0);
                assert_eq!(p.stream_id, 0);
            }
            _ => panic!("expected Complete"),
        }
    }

    // Helper: build a SysEx-7 packet with explicit group.
    fn sysex7_packet_for_group(group: u8, status: u8, bytes: &[u8]) -> [u32; 2] {
        assert!(bytes.len() <= 6);
        #[allow(clippy::cast_possible_truncation)]
        let n = bytes.len() as u32;
        let mut padded = [0u8; 6];
        padded[..bytes.len()].copy_from_slice(bytes);
        let w0 = (0x3u32 << 28)
            | (u32::from(group & 0xF) << 24)
            | (u32::from(status) << 20)
            | (n << 16)
            | (u32::from(padded[0]) << 8)
            | u32::from(padded[1]);
        let w1 = (u32::from(padded[2]) << 24)
            | (u32::from(padded[3]) << 16)
            | (u32::from(padded[4]) << 8)
            | u32::from(padded[5]);
        [w0, w1]
    }

    #[test]
    fn assembler_concurrent_streams_across_groups() {
        // Two SysEx-7 streams interleaved on groups 3 and 7. Both
        // should reassemble independently; neither's bytes should
        // bleed into the other.
        let mut a = SysExAssembler::with_capacity(64);

        // Group 3: Start with [0x10, 0x11].
        let g3_start = sysex7_packet_for_group(3, SYSEX_STATUS_START, &[0x10, 0x11]);
        assert!(matches!(
            a.push_sysex7_packet(g3_start),
            SysExFeed::Buffered
        ));

        // Group 7: Start with [0x20, 0x21, 0x22].
        let g7_start = sysex7_packet_for_group(7, SYSEX_STATUS_START, &[0x20, 0x21, 0x22]);
        assert!(matches!(
            a.push_sysex7_packet(g7_start),
            SysExFeed::Buffered
        ));

        // Group 3: End with [0x12].
        let g3_end = sysex7_packet_for_group(3, SYSEX_STATUS_END, &[0x12]);
        match a.push_sysex7_packet(g3_end) {
            SysExFeed::Complete(p) => {
                assert_eq!(p.group, 3);
                assert_eq!(p.bytes, &[0x10, 0x11, 0x12]);
            }
            _ => panic!("expected Complete on group 3"),
        }

        // Group 7: End with [0x23, 0x24].
        let g7_end = sysex7_packet_for_group(7, SYSEX_STATUS_END, &[0x23, 0x24]);
        match a.push_sysex7_packet(g7_end) {
            SysExFeed::Complete(p) => {
                assert_eq!(p.group, 7);
                assert_eq!(p.bytes, &[0x20, 0x21, 0x22, 0x23, 0x24]);
            }
            _ => panic!("expected Complete on group 7"),
        }
    }

    #[test]
    fn assembler_sysex8_stream_id_isolates_concurrent_streams() {
        // Two SysEx-8 streams on the same group (0) but different
        // stream_ids (5 and 9), interleaved.
        let mut a = SysExAssembler::with_capacity(64);

        // Helper to build a SysEx-8 packet with explicit
        // status / n / stream_id / first 4 payload bytes (the
        // assembler only reads the first `n` payload bytes).
        let mk = |status: u8, n: u32, stream_id: u8, bytes: [u8; 4]| -> [u32; 4] {
            let w0 = (0x5u32 << 28)
                | (u32::from(status) << 20)
                | (n << 16)
                | (u32::from(stream_id) << 8)
                | u32::from(bytes[0]);
            let w1 = (u32::from(bytes[1]) << 24)
                | (u32::from(bytes[2]) << 16)
                | (u32::from(bytes[3]) << 8);
            [w0, w1, 0, 0]
        };

        // stream 5 Start: 4 bytes [0xA0, 0xA1, 0xA2, 0xA3]
        assert!(matches!(
            a.push_sysex8_packet(mk(SYSEX_STATUS_START, 4, 5, [0xA0, 0xA1, 0xA2, 0xA3])),
            SysExFeed::Buffered
        ));
        // stream 9 Start: 4 bytes [0xB0, 0xB1, 0xB2, 0xB3]
        assert!(matches!(
            a.push_sysex8_packet(mk(SYSEX_STATUS_START, 4, 9, [0xB0, 0xB1, 0xB2, 0xB3])),
            SysExFeed::Buffered
        ));
        // stream 5 End: 1 byte [0xA4]
        match a.push_sysex8_packet(mk(SYSEX_STATUS_END, 1, 5, [0xA4, 0, 0, 0])) {
            SysExFeed::Complete(p) => {
                assert_eq!(p.stream_id, 5);
                assert_eq!(p.bytes, &[0xA0, 0xA1, 0xA2, 0xA3, 0xA4]);
            }
            _ => panic!("expected Complete on stream 5"),
        }
        // stream 9 End: 2 bytes [0xB4, 0xB5]
        match a.push_sysex8_packet(mk(SYSEX_STATUS_END, 2, 9, [0xB4, 0xB5, 0, 0])) {
            SysExFeed::Complete(p) => {
                assert_eq!(p.stream_id, 9);
                assert_eq!(p.bytes, &[0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5]);
            }
            _ => panic!("expected Complete on stream 9"),
        }
    }

    // -- SysEx-7 encoder --

    // Feed an encoded chain back through the assembler and return the
    // reassembled payload; the encoder and assembler anchor each other.
    #[track_caller]
    fn sysex7_encode_round_trip(group: u8, payload: &[u8]) {
        let mut a = SysExAssembler::with_capacity(payload.len().max(1));
        let total = sysex7_packet_count(payload.len());
        for i in 0..total {
            let packet = encode_sysex7_packet(group, payload, i).expect("in-range packet");
            match a.push_sysex7_packet(packet) {
                SysExFeed::Buffered => assert!(i + 1 < total, "premature Buffered"),
                SysExFeed::Complete(p) => {
                    assert_eq!(i + 1, total, "Complete before the last packet");
                    assert_eq!(p.group, group);
                    assert_eq!(p.bytes, payload);
                }
                _ => panic!("assembler rejected encoder output"),
            }
        }
        assert!(encode_sysex7_packet(group, payload, total).is_none());
    }

    #[test]
    fn sysex7_encoder_round_trips_through_assembler() {
        sysex7_encode_round_trip(0, &[]);
        sysex7_encode_round_trip(0, &[0x7E]);
        sysex7_encode_round_trip(3, &[1, 2, 3, 4, 5, 6]); // exactly one packet
        sysex7_encode_round_trip(7, &[1, 2, 3, 4, 5, 6, 7]); // Start + End
        sysex7_encode_round_trip(15, &(0..=40u8).collect::<Vec<_>>()); // long chain
    }

    #[test]
    fn sysex7_encoder_bits_match_spec() {
        // Complete, 2 bytes, group 5: mt=0x3, status=0x0, n=2.
        let packet = encode_sysex7_packet(5, &[0x7E, 0x09], 0).unwrap();
        assert_eq!(
            packet,
            [(0x3 << 28) | (5 << 24) | (2 << 16) | (0x7E << 8) | 0x09, 0]
        );
        // 7 bytes: packet 0 is Start (n=6), packet 1 is End (n=1).
        let payload = [1, 2, 3, 4, 5, 6, 7];
        let start = encode_sysex7_packet(0, &payload, 0).unwrap();
        assert_eq!(
            (start[0] >> 20) & 0xF,
            u32::from(SYSEX_STATUS_START),
            "first of a chain is Start"
        );
        let end = encode_sysex7_packet(0, &payload, 1).unwrap();
        assert_eq!((end[0] >> 20) & 0xF, u32::from(SYSEX_STATUS_END));
        assert_eq!((end[0] >> 16) & 0xF, 1, "End carries the 1 leftover byte");
        assert_eq!((end[0] >> 8) & 0xFF, 7);
    }

    #[test]
    fn sysex7_encoder_masks_to_7_bit() {
        let packet = encode_sysex7_packet(0, &[0xFF], 0).unwrap();
        assert_eq!((packet[0] >> 8) & 0xFF, 0x7F);
    }

    #[test]
    fn assembler_lru_evicts_when_slots_exhausted() {
        // Start more concurrent streams than slots exist; the
        // oldest in-progress should be evicted.
        let slots_u8 = u8::try_from(SYSEX_ASSEMBLER_SLOTS).expect("slot count fits u8");
        let mut a = SysExAssembler::with_capacity(64);
        for group in 0..slots_u8 {
            let start = sysex7_packet_for_group(group, SYSEX_STATUS_START, &[group]);
            assert!(matches!(a.push_sysex7_packet(start), SysExFeed::Buffered));
        }
        // One more - must evict group 0 (LRU).
        let new_group = slots_u8;
        let evictor = sysex7_packet_for_group(new_group, SYSEX_STATUS_START, &[new_group]);
        assert!(matches!(a.push_sysex7_packet(evictor), SysExFeed::Buffered));
        // Group 0's End should now fail (its slot got reused).
        let g0_end = sysex7_packet_for_group(0, SYSEX_STATUS_END, &[0x99]);
        assert!(matches!(a.push_sysex7_packet(g0_end), SysExFeed::Invalid));
        // The evictor's End should work.
        let new_end = sysex7_packet_for_group(new_group, SYSEX_STATUS_END, &[0xEE]);
        match a.push_sysex7_packet(new_end) {
            SysExFeed::Complete(p) => {
                assert_eq!(p.group, new_group);
                assert_eq!(p.bytes, &[new_group, 0xEE]);
            }
            _ => panic!("expected Complete on evicting group"),
        }
    }
}
