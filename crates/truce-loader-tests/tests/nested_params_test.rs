//! Behavioral tests for `#[nested]` params.
//!
//! A nested group's params auto-number locally (0, 1, ...); the parent
//! places the group at a base (explicit `#[nested(base = N)]` or
//! auto-packed after the preceding params) and rebases the group's ids
//! into its own id space at construction. That base offset is what lets
//! the same Params type be reused in two slots without an id clash, and
//! lets group authors write no ids at all.

// Reading a param's declared default back through `get_plain` is the
// point of several of these, so values compare bit-exact.
#![allow(clippy::float_cmp)]

use truce_derive::Params;
use truce_params::{FloatParam, MeterSlot, Params};

// A reusable group. No ids: its params number locally from 0.
#[derive(Params)]
struct Filter {
    #[param(name = "Cutoff", range = "log(20, 20000)", default = 2000.0)]
    cutoff: FloatParam,
    #[param(name = "Reso", range = "linear(0, 1)", default = 0.3)]
    resonance: FloatParam,
}

// Mixes an own param with a nested group at a pinned base, so the
// flattened ids are fixed: gain 0, cutoff 1, reso 2.
#[derive(Params)]
struct Synth {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)", default = 0.8)]
    gain: FloatParam,
    #[nested(base = 1)]
    filter: Filter,
}

#[test]
fn mixed_new_initializes_nested_defaults() {
    let s = Synth::new();
    assert_eq!(s.count(), 3);

    // Own param first, then the rebased group.
    let ids: Vec<u32> = s.param_infos().iter().map(|p| p.id).collect();
    assert_eq!(ids, vec![0, 1, 2]);

    // Group params carry their declared defaults at the rebased ids.
    assert_eq!(s.get_plain(0), Some(0.8));
    assert_eq!(s.get_plain(1), Some(2000.0));
    assert_eq!(s.get_plain(2), Some(0.3));
}

#[test]
fn default_matches_new_for_mixed() {
    let a: Vec<(u32, f64)> = Synth::new()
        .param_infos()
        .iter()
        .map(|p| (p.id, p.default_plain))
        .collect();
    let b: Vec<(u32, f64)> = Synth::default()
        .param_infos()
        .iter()
        .map(|p| (p.id, p.default_plain))
        .collect();
    assert_eq!(a, b);
}

#[test]
fn set_get_reaches_nested_param() {
    let s = Synth::new();
    s.set_plain(1, 500.0);
    assert_eq!(s.get_plain(1), Some(500.0));
    s.set_normalized(2, 1.0);
    assert_eq!(s.get_plain(2), Some(1.0));
    assert_eq!(s.get_plain(9_999), None);
}

#[test]
fn static_infos_match_instance_for_mixed() {
    let from_instance = Synth::new().param_infos();
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

// --- reuse: the same group type in two slots --------------------------

#[derive(Params)]
struct Band {
    #[param(name = "Gain", range = "linear(-18, 18)", default = 0.0)]
    gain: FloatParam,
    #[param(name = "Q", range = "log(0.1, 10)", default = 0.7)]
    q: FloatParam,
}

// Auto bases pack the groups back to back: low 0-1, high 2-3. No ids
// anywhere - the whole point of the feature.
#[derive(Params)]
struct DualBand {
    #[nested]
    low: Band,
    #[nested]
    high: Band,
}

#[test]
fn same_group_reused_gets_distinct_ids() {
    let d = DualBand::new();
    assert_eq!(d.count(), 4);
    let ids: Vec<u32> = d.param_infos().iter().map(|p| p.id).collect();
    assert_eq!(ids, vec![0, 1, 2, 3]);

    // Writing one band leaves the other at its default.
    d.set_plain(0, -6.0);
    assert_eq!(d.get_plain(0), Some(-6.0));
    assert_eq!(d.get_plain(2), Some(0.0));
}

// Explicit bases place the groups by hand (wire-stability anchor).
#[derive(Params)]
struct PinnedBands {
    #[nested(base = 100)]
    low: Band,
    #[nested(base = 200)]
    high: Band,
}

#[test]
fn explicit_base_places_groups() {
    let ids: Vec<u32> = PinnedBands::new()
        .param_infos()
        .iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(ids, vec![100, 101, 200, 201]);
}

// --- construction-time collision panic --------------------------------

// Own param id 1 lands inside the group pinned at base 1 (cutoff -> 1).
#[derive(Params)]
struct BadParent {
    #[param(id = 1, name = "Dup", range = "linear(0, 1)")]
    dup: FloatParam,
    #[nested(base = 1)]
    filter: Filter,
}

#[test]
#[should_panic(expected = "duplicate parameter ID 1")]
fn parent_nested_id_collision_panics() {
    let _ = BadParent::new();
}

// --- multi-level nesting (auto bases compose) -------------------------

#[derive(Params)]
struct L2 {
    #[param(name = "Z", range = "linear(0, 1)", default = 0.9)]
    z: FloatParam,
}

#[derive(Params)]
struct L1 {
    #[param(name = "Y", range = "linear(0, 1)", default = 0.5)]
    y: FloatParam,
    #[nested]
    l2: L2,
}

#[derive(Params)]
struct L0 {
    #[param(name = "X", range = "linear(0, 1)", default = 0.1)]
    x: FloatParam,
    #[nested]
    l1: L1,
}

#[test]
fn deep_nesting_flattens_in_order() {
    let p = L0::new();
    assert_eq!(p.count(), 3);
    // x 0; l1 packs after at 1 (its own y), then l2 after that at 2.
    let ids: Vec<u32> = p.param_infos().iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![0, 1, 2]);

    assert_eq!(p.get_plain(2), Some(0.9));
    p.set_plain(2, 0.25);
    assert_eq!(p.get_plain(2), Some(0.25));
}

// --- meters inside a nested struct ------------------------------------

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
    assert_eq!(meter_ids.len(), 1);
    assert!(meter_ids[0] >= truce_params::METER_ID_BASE);
}

// Meter ids aren't rebased (they keep their dedicated range), so two
// nested meter-bearing groups alias and construction must reject it.
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
