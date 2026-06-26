//! Behavioral tests for `#[nested]` params.
//!
//! Two id schemes:
//!
//! - **Ordinal** (`#[params(id_scheme = "ordinal")]`): a nested group's
//!   params number locally (0, 1, ...); the parent places the group at
//!   a base (explicit `#[nested(base = N)]` or auto-packed after the
//!   preceding params) and rebases the group's ids into its own space.
//!   Contiguous ranges; reordering shifts ids. The structs below pin
//!   this scheme so the exact-id assertions are meaningful - they double
//!   as the legacy opt-out's regression tests.
//! - **Hash** (default): each param's id is a stable hash of its field
//!   name, folded through the same base mechanism. Reordering / inserting
//!   fields leaves ids put. The `hash_*` tests at the bottom cover it.

// Reading a param's declared default back through `get_plain` is the
// point of several of these, so values compare bit-exact.
#![allow(clippy::float_cmp)]

use truce_derive::Params;
use truce_params::{FloatParam, MeterSlot, Params};

// A reusable group. No ids: its params number locally from 0.
#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct Filter {
    #[param(name = "Cutoff", range = "log(20, 20000)", default = 2000.0)]
    cutoff: FloatParam,
    #[param(name = "Reso", range = "linear(0, 1)", default = 0.3)]
    resonance: FloatParam,
}

// Mixes an own param with a nested group at a pinned base, so the
// flattened ids are fixed: gain 0, cutoff 1, reso 2.
#[derive(Params)]
#[params(id_scheme = "ordinal")]
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
#[params(id_scheme = "ordinal")]
struct Band {
    #[param(name = "Gain", range = "linear(-18, 18)", default = 0.0)]
    gain: FloatParam,
    #[param(name = "Q", range = "log(0.1, 10)", default = 0.7)]
    q: FloatParam,
}

// Auto bases pack the groups back to back: low 0-1, high 2-3. No ids
// anywhere - the whole point of the feature.
#[derive(Params)]
#[params(id_scheme = "ordinal")]
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
#[params(id_scheme = "ordinal")]
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
#[params(id_scheme = "ordinal")]
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
#[params(id_scheme = "ordinal")]
struct L2 {
    #[param(name = "Z", range = "linear(0, 1)", default = 0.9)]
    z: FloatParam,
}

#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct L1 {
    #[param(name = "Y", range = "linear(0, 1)", default = 0.5)]
    y: FloatParam,
    #[nested]
    l2: L2,
}

#[derive(Params)]
#[params(id_scheme = "ordinal")]
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

// --- hash scheme (the default) ----------------------------------------

// Same fields as `OrderTwo`, opposite declaration order. Under the hash
// default the id of a field follows its name, not its position.
#[derive(Params)]
struct OrderOne {
    #[param(name = "Alpha", range = "linear(0, 1)", default = 0.1)]
    alpha: FloatParam,
    #[param(name = "Beta", range = "linear(0, 1)", default = 0.2)]
    beta: FloatParam,
}

#[derive(Params)]
struct OrderTwo {
    #[param(name = "Beta", range = "linear(0, 1)", default = 0.2)]
    beta: FloatParam,
    // A field inserted ahead of the others under ordinal would shift
    // their ids; under hash it does not.
    #[param(name = "Gamma", range = "linear(0, 1)", default = 0.3)]
    gamma: FloatParam,
    #[param(name = "Alpha", range = "linear(0, 1)", default = 0.1)]
    alpha: FloatParam,
}

#[test]
fn hash_id_is_stable_across_reorder_and_insert() {
    assert_eq!(OrderOne::new().count(), 2);
    assert_eq!(OrderTwo::new().count(), 3);

    // Same field name -> same id, regardless of position or new
    // siblings. This is the property ordinal lacks.
    assert_eq!(OrderOneParamId::Alpha as u32, OrderTwoParamId::Alpha as u32);
    assert_eq!(OrderOneParamId::Beta as u32, OrderTwoParamId::Beta as u32);
    // Hash ids stay inside the parameter range (below the meter band).
    assert!((OrderOneParamId::Alpha as u32) < truce_params::METER_ID_BASE);
}

// Reusable group, default (hash) scheme.
#[derive(Params)]
struct HashBand {
    #[param(name = "Gain", range = "linear(-18, 18)", default = 0.0)]
    gain: FloatParam,
    #[param(name = "Q", range = "log(0.1, 10)", default = 0.7)]
    q: FloatParam,
}

#[derive(Params)]
struct HashDual {
    #[nested]
    low: HashBand,
    #[nested]
    high: HashBand,
}

#[test]
fn hash_reuse_gets_distinct_ids() {
    let d = HashDual::new();
    assert_eq!(d.count(), 4);

    let infos = d.param_infos();
    let mut ids: Vec<u32> = infos.iter().map(|p| p.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 4, "the reused group must get 4 distinct ids");
    assert!(ids.iter().all(|&id| id < truce_params::METER_ID_BASE));

    // param_infos is in declaration order: low.gain, low.q, high.gain,
    // high.q. Writing low's gain leaves high's gain at its default.
    let low_gain = infos[0].id;
    let high_gain = infos[2].id;
    d.set_plain(low_gain, -6.0);
    assert_eq!(d.get_plain(low_gain), Some(-6.0));
    assert_eq!(d.get_plain(high_gain), Some(0.0));
}

// Deep nesting under hash: every leaf still lands at a distinct id, and
// values round-trip.
#[derive(Params)]
struct HashInner {
    #[param(name = "Z", range = "linear(0, 1)", default = 0.9)]
    z: FloatParam,
}

#[derive(Params)]
struct HashMid {
    #[param(name = "Y", range = "linear(0, 1)", default = 0.5)]
    y: FloatParam,
    #[nested]
    inner: HashInner,
}

#[derive(Params)]
struct HashOuter {
    #[param(name = "X", range = "linear(0, 1)", default = 0.1)]
    x: FloatParam,
    #[nested]
    mid: HashMid,
}

#[test]
fn hash_deep_nesting_distinct_and_roundtrips() {
    let p = HashOuter::new();
    assert_eq!(p.count(), 3);

    let infos = p.param_infos();
    let mut ids: Vec<u32> = infos.iter().map(|i| i.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 3);

    // Static and instance paths agree on the (hashed, rebased) ids.
    let stat: Vec<u32> = HashOuter::param_infos_static()
        .iter()
        .map(|i| i.id)
        .collect();
    let inst: Vec<u32> = infos.iter().map(|i| i.id).collect();
    assert_eq!(stat, inst);

    let deepest = infos[2].id; // mid.inner.z
    assert_eq!(p.get_plain(deepest), Some(0.9));
    p.set_plain(deepest, 0.25);
    assert_eq!(p.get_plain(deepest), Some(0.25));
}

// --- triple nesting (3 levels deep), both schemes ---------------------

#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct OrdD {
    #[param(name = "W", range = "linear(0, 1)", default = 0.4)]
    w: FloatParam,
}
#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct OrdC {
    #[param(name = "V", range = "linear(0, 1)", default = 0.3)]
    v: FloatParam,
    #[nested]
    d: OrdD,
}
#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct OrdB {
    #[param(name = "U", range = "linear(0, 1)", default = 0.2)]
    u: FloatParam,
    #[nested]
    c: OrdC,
}
#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct OrdA {
    #[param(name = "T", range = "linear(0, 1)", default = 0.1)]
    t: FloatParam,
    #[nested]
    b: OrdB,
}

#[test]
fn ordinal_triple_nesting_packs_contiguously() {
    let p = OrdA::new();
    assert_eq!(p.count(), 4);
    // t 0, then b packs (u 1), then c (v 2), then d (w 3).
    let ids: Vec<u32> = p.param_infos().iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![0, 1, 2, 3]);
    // Static path agrees through all 3 levels.
    let stat: Vec<u32> = OrdA::param_infos_static().iter().map(|i| i.id).collect();
    assert_eq!(stat, ids);
    // Deepest param round-trips at its rebased id.
    assert_eq!(p.get_plain(3), Some(0.4));
    p.set_plain(3, 0.9);
    assert_eq!(p.get_plain(3), Some(0.9));
}

#[derive(Params)]
struct HashD {
    #[param(name = "W", range = "linear(0, 1)", default = 0.4)]
    w: FloatParam,
}
#[derive(Params)]
struct HashC {
    #[param(name = "V", range = "linear(0, 1)", default = 0.3)]
    v: FloatParam,
    #[nested]
    d: HashD,
}
#[derive(Params)]
struct HashB {
    #[param(name = "U", range = "linear(0, 1)", default = 0.2)]
    u: FloatParam,
    #[nested]
    c: HashC,
}
#[derive(Params)]
struct HashA {
    #[param(name = "T", range = "linear(0, 1)", default = 0.1)]
    t: FloatParam,
    #[nested]
    b: HashB,
}

#[test]
fn hash_triple_nesting_distinct_and_static_matches() {
    let p = HashA::new();
    assert_eq!(p.count(), 4);

    let infos = p.param_infos();
    let inst: Vec<u32> = infos.iter().map(|i| i.id).collect();
    let mut ids = inst.clone();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 4, "all 4 ids distinct through 3 levels");
    assert!(ids.iter().all(|&id| id < truce_params::METER_ID_BASE));

    // Static path agrees with the instance through all 3 levels - the
    // property the preset name resolver / LV2 TTL aggregator rely on.
    let stat: Vec<u32> = HashA::param_infos_static().iter().map(|i| i.id).collect();
    assert_eq!(stat, inst);

    // Deepest param (w) round-trips at its hashed, rebased id.
    let w = infos[3].id;
    assert_eq!(p.get_plain(w), Some(0.4));
    p.set_plain(w, 0.9);
    assert_eq!(p.get_plain(w), Some(0.9));
}

// --- nested param id stability (the hash scheme's whole point) ---------

#[derive(Params)]
struct StablePair {
    #[param(name = "P", range = "linear(0, 1)")]
    p: FloatParam,
    #[param(name = "Q", range = "linear(0, 1)")]
    q: FloatParam,
}

// Same params, opposite declaration order.
#[derive(Params)]
struct StablePairReordered {
    #[param(name = "Q", range = "linear(0, 1)")]
    q: FloatParam,
    #[param(name = "P", range = "linear(0, 1)")]
    p: FloatParam,
}

#[derive(Params)]
struct OneGroup {
    #[nested]
    foo: StablePair,
}

// A new own param inserted ahead of the nested group: under ordinal
// this would shift every nested id; under hash it must not.
#[derive(Params)]
struct OneGroupShifted {
    #[param(name = "X", range = "linear(0, 1)")]
    x: FloatParam,
    #[nested]
    foo: StablePair,
}

#[derive(Params)]
struct TwoSlots {
    #[nested]
    foo: StablePair,
    #[nested]
    bar: StablePair,
}

// Same two slots, swapped declaration order.
#[derive(Params)]
struct TwoSlotsSwapped {
    #[nested]
    bar: StablePair,
    #[nested]
    foo: StablePair,
}

#[test]
fn nested_param_ids_are_stable_across_reorder_and_insert() {
    use std::collections::HashSet;

    // (1) Reordering a struct's own params doesn't move them.
    assert_eq!(StablePairReordered::new().count(), 2);
    assert_eq!(
        StablePairParamId::P as u32,
        StablePairReorderedParamId::P as u32
    );
    assert_eq!(
        StablePairParamId::Q as u32,
        StablePairReorderedParamId::Q as u32
    );

    // (2) Inserting an own param ahead of a nested group leaves the
    // group's ids put - its base is a hash of its slot name, not a
    // packed offset.
    let plain: HashSet<u32> = OneGroup::new().param_infos().iter().map(|i| i.id).collect();
    let shifted: HashSet<u32> = OneGroupShifted::new()
        .param_infos()
        .iter()
        .map(|i| i.id)
        .collect();
    assert!(
        plain.is_subset(&shifted),
        "nested group ids shifted when an own param was inserted before it"
    );

    // (3) Reordering the nested slots leaves every nested param id put.
    let a: HashSet<u32> = TwoSlots::new().param_infos().iter().map(|i| i.id).collect();
    let b: HashSet<u32> = TwoSlotsSwapped::new()
        .param_infos()
        .iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(a.len(), 4);
    assert_eq!(a, b, "reordering nested slots moved a nested param id");
}
