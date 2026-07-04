//! Fuzz harness bodies, shared between the libFuzzer targets in
//! `fuzz_targets/` and the corpus replay binary (`src/bin/replay.rs`,
//! which re-runs an accumulated corpus under Miri to promote
//! "interesting input" to "UB-checked input").
//!
//! Each harness holds the oracles, not just a call: the state
//! harness pins the round-trip and the strict-shim agreement, the
//! preset harness pins writer/parser agreement. "Doesn't panic" is
//! the floor, not the goal.

use truce_core::midi::{decode_short_message, event_to_midi1, parse_midi1};
use truce_core::ump::{SysExAssembler, decode_ump_channel_voice_2, encode_ump_channel_voice_2};
use truce_utils::preset::{parse_preset_file, parse_preset_meta, write_preset_file};
use truce_utils::state::{StateParse, deserialize_state, parse_state, serialize_state};

/// Host session blobs. First 8 bytes steer the expected plugin id so
/// the fuzzer controls the match/mismatch axis; the rest is the blob.
pub fn state_envelope(data: &[u8]) {
    if data.len() < 8 {
        return;
    }
    let (id_bytes, blob) = data.split_at(8);
    let expected = u64::from_le_bytes(id_bytes.try_into().expect("split_at(8)"));

    let parsed = parse_state(blob, expected);

    // The strict shim must never drift from the split parser:
    // `Some` iff `Ok`.
    let strict = deserialize_state(blob, expected);
    match &parsed {
        StateParse::Ok(_) => assert!(strict.is_some(), "parse Ok but deserialize None"),
        _ => assert!(strict.is_none(), "parse non-Ok but deserialize Some"),
    }

    // Decoded envelopes (matching or renamed-plugin) must round-trip
    // bit-exactly through the writer.
    let (found, state) = match parsed {
        StateParse::Ok(state) => (expected, state),
        StateParse::WrongPlugin { found, state } => (found, state),
        _ => return,
    };
    let ids: Vec<u32> = state.params.iter().map(|(id, _)| *id).collect();
    let values: Vec<f64> = state.params.iter().map(|(_, v)| *v).collect();
    let extra = state.extra.clone().unwrap_or_default();
    let rewritten = serialize_state(found, &ids, &values, &extra);
    match parse_state(&rewritten, found) {
        StateParse::Ok(reparsed) => {
            assert_eq!(reparsed.params.len(), state.params.len());
            for ((id_a, v_a), (id_b, v_b)) in state.params.iter().zip(reparsed.params.iter()) {
                assert_eq!(id_a, id_b);
                // Bit compare: fuzzed values include NaN, where `==`
                // would false-negative.
                assert_eq!(v_a.to_bits(), v_b.to_bits());
            }
            assert_eq!(reparsed.extra.unwrap_or_default(), extra);
        }
        _ => panic!("re-serialized envelope failed to parse"),
    }
}

/// `.trucepreset` container files (disk / preset packs).
pub fn preset_container(data: &[u8]) {
    let _ = parse_preset_meta(data);
    if let Some((meta, envelope)) = parse_preset_file(data) {
        // Whatever the parser accepted, the writer must reproduce a
        // parseable container carrying the same envelope.
        let rewritten = write_preset_file(&meta, &envelope);
        let (_, reparsed_envelope) =
            parse_preset_file(&rewritten).expect("writer output must parse");
        assert_eq!(reparsed_envelope, envelope);
    }
}

/// 3-byte MIDI short messages + the byte-stream parser.
pub fn midi_short(data: &[u8]) {
    if data.len() < 3 {
        return;
    }
    if let Some(body) = decode_short_message(data[0], data[1], data[2]) {
        // Anything the decoder accepts must be re-encodable or be a
        // deliberately encode-less variant; encoding must not panic.
        let _ = event_to_midi1(&body);
    }
    let _ = parse_midi1(data[0] & 0x0F, &data[1..]);
}

/// Host UMP words (MIDI 2.0 channel voice).
pub fn ump_decode(data: &[u8]) {
    if data.len() < 16 {
        return;
    }
    let word = |i: usize| u32::from_le_bytes(data[4 * i..4 * i + 4].try_into().expect("len 16"));
    let words = [word(0), word(1), word(2), word(3)];
    if let Some(body) = decode_ump_channel_voice_2(words) {
        let _ = encode_ump_channel_voice_2(&body);
    }
}

/// Stateful SysEx reassembly from packet sequences a host can
/// interleave, truncate, or repeat. A selector byte picks SysEx-7 vs
/// SysEx-8, then the packet's own words follow: 8 bytes (two words)
/// for a 64-bit SysEx-7 packet, 16 (four) for a 128-bit SysEx-8 one -
/// mirroring two words into four would halve the 8-bit target's
/// explorable space.
pub fn sysex_assembler(data: &[u8]) {
    let mut asm = SysExAssembler::with_capacity(512);
    let mut rest = data;
    while let Some((&selector, tail)) = rest.split_first() {
        let word = |i: usize| {
            u32::from_le_bytes(tail[4 * i..4 * i + 4].try_into().expect("length checked"))
        };
        if selector & 1 == 0 {
            if tail.len() < 8 {
                return;
            }
            let _ = asm.push_sysex7_packet([word(0), word(1)]);
            rest = &tail[8..];
        } else {
            if tail.len() < 16 {
                return;
            }
            let _ = asm.push_sysex8_packet([word(0), word(1), word(2), word(3)]);
            rest = &tail[16..];
        }
    }
}
