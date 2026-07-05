//! Regression test: `#[derive(State)]` must compile and round-trip.
//!
//! The generated `deserialize` bounded the forward-compat skip loop
//! by adding the cursor's remaining `usize` byte count to a `u32`
//! field count interpolated as a literal. The addition failed to
//! compile (no `Add<u32> for usize`) for any struct annotated with
//! the derive. The bug went unnoticed because nothing in-tree used
//! the derive - fixed by casting the field count at the arithmetic
//! site.

use truce_core::custom_state::State;
use truce_derive::State;

#[derive(State, Default, Debug, PartialEq, Clone)]
struct PrimitiveState {
    flag: bool,
    count: u32,
    rate: f64,
    tag: u8,
}

#[derive(State, Default, Debug, PartialEq)]
struct CollectionState {
    name: String,
    names: Vec<String>,
    flags: Vec<bool>,
    maybe: Option<u32>,
}

#[test]
fn primitive_state_round_trips() {
    let original = PrimitiveState {
        flag: true,
        count: 0xDEAD_BEEF,
        rate: 48_000.5,
        tag: 7,
    };
    let bytes = original.serialize();
    let decoded = PrimitiveState::deserialize(&bytes).expect("deserialize");
    assert_eq!(decoded, original);
}

#[test]
fn collection_state_round_trips() {
    let original = CollectionState {
        name: "instance-1".to_string(),
        names: vec!["a".to_string(), "b".to_string(), "ç".to_string()],
        flags: vec![true, false, true],
        maybe: Some(42),
    };
    let bytes = original.serialize();
    let decoded = CollectionState::deserialize(&bytes).expect("deserialize");
    assert_eq!(decoded, original);
}

#[test]
fn default_round_trips() {
    let original = CollectionState::default();
    let bytes = original.serialize();
    let decoded = CollectionState::deserialize(&bytes).expect("deserialize");
    assert_eq!(decoded, original);
}

#[test]
fn deserialize_garbage_does_not_panic() {
    assert!(PrimitiveState::deserialize(&[]).is_none());
    assert!(PrimitiveState::deserialize(&[0xFF; 3]).is_none());
    // A 4-byte stored_count past usize::MAX shouldn't loop forever -
    // the `cursor.remaining() / 4 + N` bound caps it.
    let mut bogus = vec![0xFF, 0xFF, 0xFF, 0xFF];
    bogus.extend_from_slice(&[0u8; 16]);
    let _ = PrimitiveState::deserialize(&bogus);
}

#[test]
fn serialize_into_matches_serialize_and_reuses_buffer() {
    let original = PrimitiveState {
        flag: true,
        count: 1,
        rate: 1.0,
        tag: 2,
    };
    let mut buf = Vec::new();
    original.serialize_into(&mut buf);
    assert_eq!(buf, original.serialize());

    // A second call clears and refills without growing the allocation:
    // the audio-thread path must not allocate once the buffer is warm.
    let cap = buf.capacity();
    original.serialize_into(&mut buf);
    assert_eq!(buf.capacity(), cap);
    assert_eq!(PrimitiveState::deserialize(&buf), Some(original.clone()));

    // Leftover bytes from a larger prior payload are dropped, not appended.
    buf.extend_from_slice(&[0xAA; 32]);
    original.serialize_into(&mut buf);
    assert_eq!(PrimitiveState::deserialize(&buf), Some(original));
}
