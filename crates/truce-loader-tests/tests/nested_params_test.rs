//! Behavioral tests for `#[nested]` params, focused on the path where
//! a struct mixes its own `#[param]` fields with `#[nested]` children
//! and relies on the derive-generated `new()` to initialize both. That
//! `new()` default-initializes the nested fields; before that a mixed
//! struct's `new()` wouldn't compile, so this file pins the behavior it
//! enables: correct nested defaults, id ordering, cross-boundary
//! get/set dispatch, the construction-time collision panic, and the
//! same checks under multi-level nesting.

// Reading a param's declared default back through `get_plain` is the
// point of several of these, so values compare bit-exact.
#![allow(clippy::float_cmp)]

use truce_derive::Params;
use truce_params::{FloatParam, MeterSlot, Params};

#[derive(Params)]
struct Filter {
    #[param(id = 0, name = "Cutoff", range = "log(20, 20000)", default = 2000.0)]
    cutoff: FloatParam,
    #[param(id = 1, name = "Reso", range = "linear(0, 1)", default = 0.3)]
    reso: FloatParam,
}

// Mixes an own param (`gain`) with a `#[nested]` child. This is the
// shape whose derived `new()` only compiles once nested fields are
// default-initialized in the struct literal.
#[derive(Params)]
struct Synth {
    #[param(id = 10, name = "Gain", range = "linear(0, 1)", default = 0.8)]
    gain: FloatParam,
    #[nested]
    filter: Filter,
}

#[test]
fn mixed_new_initializes_nested_defaults() {
    let s = Synth::new();
    assert_eq!(s.count(), 3, "own param + two nested params");

    // Own param first, then nested children in field order.
    let ids: Vec<u32> = s.param_infos().iter().map(|p| p.id).collect();
    assert_eq!(ids, vec![10, 0, 1]);

    // Nested params carry their *declared* defaults, not a zeroed
    // `FloatParam` - i.e. `Default::default()` resolved to the nested
    // `new()`, not a blank value.
    assert_eq!(s.get_plain(10), Some(0.8));
    assert_eq!(s.get_plain(0), Some(2000.0));
    assert_eq!(s.get_plain(1), Some(0.3));
}

#[test]
fn default_matches_new_for_mixed() {
    let from_new = Synth::new();
    let from_default = Synth::default();
    let new_pairs: Vec<(u32, f64)> = from_new
        .param_infos()
        .iter()
        .map(|p| (p.id, p.default_plain))
        .collect();
    let default_pairs: Vec<(u32, f64)> = from_default
        .param_infos()
        .iter()
        .map(|p| (p.id, p.default_plain))
        .collect();
    assert_eq!(new_pairs, default_pairs);
}

#[test]
fn set_get_reaches_nested_param() {
    let s = Synth::new();

    // Plain write lands on the nested param through the parent's
    // `get_plain`/`set_plain` fallthrough.
    s.set_plain(0, 500.0);
    assert_eq!(s.get_plain(0), Some(500.0));

    // Normalized write routes the same way (linear(0,1) -> 1.0 plain).
    s.set_normalized(1, 1.0);
    assert_eq!(s.get_plain(1), Some(1.0));

    // Unknown id resolves to None after exhausting nested children.
    assert_eq!(s.get_plain(9_999), None);
}

#[test]
fn static_infos_match_instance_for_mixed() {
    let inst = Synth::new();
    let from_instance = inst.param_infos();
    let from_static = Synth::param_infos_static();
    assert_eq!(from_static.len(), from_instance.len());
    for (s, i) in from_static.iter().zip(from_instance.iter()) {
        assert_eq!(s.id, i.id);
        assert_eq!(s.name, i.name);
        assert_eq!(s.default_plain, i.default_plain);
        assert_eq!(s.range.min(), i.range.min());
        assert_eq!(s.range.max(), i.range.max());
    }
}

// --- construction-time collision panic ----------------------------------

#[derive(Params)]
struct BadInner {
    #[param(id = 5, name = "X", range = "linear(0, 1)")]
    x: FloatParam,
}

// Parent param id 5 collides with the nested param id 5. The per-struct
// compile-time check can't see across nested types, so the derived
// `new()` calls `assert_no_id_collisions`, which panics at construction.
#[derive(Params)]
struct BadParent {
    #[param(id = 5, name = "Dup", range = "linear(0, 1)")]
    dup: FloatParam,
    #[nested]
    inner: BadInner,
}

#[test]
#[should_panic(expected = "duplicate parameter ID 5")]
fn parent_nested_id_collision_panics() {
    let _ = BadParent::new();
}

// --- multi-level nesting -------------------------------------------------

#[derive(Params)]
struct L2 {
    #[param(id = 300, name = "Z", range = "linear(0, 1)", default = 0.9)]
    z: FloatParam,
}

#[derive(Params)]
struct L1 {
    #[param(id = 200, name = "Y", range = "linear(0, 1)", default = 0.5)]
    y: FloatParam,
    #[nested]
    l2: L2,
}

#[derive(Params)]
struct L0 {
    #[param(id = 100, name = "X", range = "linear(0, 1)", default = 0.1)]
    x: FloatParam,
    #[nested]
    l1: L1,
}

#[test]
fn deep_nesting_flattens_in_order() {
    let p = L0::new();
    assert_eq!(p.count(), 3);
    let ids: Vec<u32> = p.param_infos().iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![100, 200, 300]);

    // Defaults reach two levels down, and a write to the deepest leaf
    // round-trips through both fallthrough hops.
    assert_eq!(p.get_plain(300), Some(0.9));
    p.set_plain(300, 0.25);
    assert_eq!(p.get_plain(300), Some(0.25));
}

// --- meters inside a nested struct --------------------------------------

#[derive(Params)]
struct MeterPack {
    #[meter]
    level: MeterSlot,
}

#[derive(Params)]
struct WithNestedMeter {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)", default = 1.0)]
    gain: FloatParam,
    #[nested]
    meters: MeterPack,
}

#[test]
fn nested_meter_surfaces_through_parent() {
    let p = WithNestedMeter::new();
    let meter_ids = p.meter_ids();
    assert_eq!(
        meter_ids.len(),
        1,
        "the nested meter is visible on the parent"
    );
    // Meters live in their own high id range, disjoint from params.
    assert!(meter_ids[0] >= truce_params::METER_ID_BASE);
}

// Meter ids auto-assign per struct from a shared base, so two nested
// meter-bearing structs alias on the same id. That can't be expressed
// safely, so construction must panic rather than silently alias.
#[derive(Params)]
struct MeterPackB {
    #[meter]
    peak: MeterSlot,
}

#[derive(Params)]
struct TwoNestedMeters {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)", default = 1.0)]
    gain: FloatParam,
    #[nested]
    a: MeterPack,
    #[nested]
    b: MeterPackB,
}

#[test]
#[should_panic(expected = "duplicate meter ID")]
fn two_nested_meters_collide_at_construction() {
    let _ = TwoNestedMeters::new();
}
