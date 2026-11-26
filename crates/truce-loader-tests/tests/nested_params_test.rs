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

// --- nested arrays `#[nested] f: [T; N]` -------------------------------

// Own param, then a 3-element array pinned at base 1. `Band` has two
// ordinal params (gain 0, q 1), so the array spans ids 1..=6.
#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct BandArray {
    #[param(id = 0, name = "Master", range = "linear(0, 1)", default = 0.5)]
    master: FloatParam,
    #[nested(base = 1, label = "Band")]
    bands: [Band; 3],
}

#[test]
fn ordinal_array_packs_each_element_contiguously() {
    let p = BandArray::new();
    assert_eq!(p.count(), 7);
    let ids: Vec<u32> = p.param_infos().iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![0, 1, 2, 3, 4, 5, 6]);

    // The static table agrees id-for-id (preset / LV2 wire contract).
    let stat: Vec<u32> = BandArray::param_infos_static()
        .iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(stat, ids);

    // The public array field is indexable and each element's live id is
    // the rebased global id.
    assert_eq!(p.bands[0].gain.id(), 1);
    assert_eq!(p.bands[2].q.id(), 6);

    // Elements are independent: writing one leaves the others at default.
    p.set_plain(3, -6.0); // bands[1].gain
    assert_eq!(p.get_plain(3), Some(-6.0));
    assert_eq!(p.get_plain(1), Some(0.0)); // bands[0].gain still default
}

#[test]
fn array_elements_get_disambiguated_names() {
    // Every `Band` exposes leaf names `Gain` / `Q`; across a 3-element
    // array that would be three identical pairs, so hosts (and the GUI)
    // couldn't tell them apart. The derive prefixes each element with the
    // array's label (`label = "Band"` here) and 1-based index.
    let names: Vec<&str> = BandArray::new()
        .param_infos()
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(
        names,
        vec![
            "Master",
            "Band 1 Gain",
            "Band 1 Q",
            "Band 2 Gain",
            "Band 2 Q",
            "Band 3 Gain",
            "Band 3 Q",
        ],
    );

    // `short_name` gets a bare index suffix - distinct, but kept short for
    // hosts that show short names in limited-width strips.
    let short: Vec<&str> = BandArray::new()
        .param_infos()
        .iter()
        .map(|i| i.short_name)
        .collect();
    assert_eq!(
        short,
        vec!["Master", "Gain 1", "Q 1", "Gain 2", "Q 2", "Gain 3", "Q 3"],
    );

    // The static table (what format wrappers register from) must match the
    // live table exactly - name and short_name both - or hosts and the
    // running plugin would disagree.
    let stat: Vec<&str> = BandArray::param_infos_static()
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(stat, names);
    let stat_short: Vec<&str> = BandArray::param_infos_static()
        .iter()
        .map(|i| i.short_name)
        .collect();
    assert_eq!(stat_short, short);
}

#[test]
fn array_id_helper_matches_live_field() {
    let p = BandArray::new();
    // The shipped static helper resolves element ids without an instance.
    assert_eq!(BandArray::bands_id(0, BandParamId::Gain), 1);
    assert_eq!(BandArray::bands_id(2, BandParamId::Q), 6);
    // It returns exactly what the running plugin exposes.
    for (i, band) in p.bands.iter().enumerate() {
        assert_eq!(BandArray::bands_id(i, BandParamId::Gain), band.gain.id());
        assert_eq!(BandArray::bands_id(i, BandParamId::Q), band.q.id());
    }
}

// A nested array longer than 32. `Default::default()` can't build
// `[T; N]` for N > 32 (std's array `Default` impls stop at 32), so the
// derive's `new()` constructs the array with `core::array::from_fn`.
#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct BigArray {
    #[nested]
    bands: [Band; 40],
}

#[test]
fn nested_array_longer_than_32_constructs() {
    let p = BigArray::new();
    assert_eq!(p.count(), 80); // 40 elements * 2 params
    let names: Vec<&str> = p.param_infos().iter().map(|i| i.name).collect();
    // No `label`: the field name, title-cased.
    assert_eq!(names.first().copied(), Some("Bands 1 Gain"));
    assert_eq!(names.last().copied(), Some("Bands 40 Q"));
}

// Two arrays of one element type, whose element has a param and a single
// nested group sharing a leaf name: every flattened name must still be
// unique (it used to give three different `Level 1`s).
#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct NameEnv {
    #[param(name = "Level", range = "linear(0, 1)", default = 0.5)]
    level: FloatParam,
}

#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct NameVoice {
    #[param(name = "Level", range = "linear(0, 1)", default = 0.5)]
    level: FloatParam,
    #[nested]
    env: NameEnv,
}

#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct NameProbe {
    #[param(name = "Level", range = "linear(0, 1)", default = 0.5)]
    level: FloatParam,
    #[nested(label = "Osc")]
    osc: [NameVoice; 2],
    #[nested]
    sub_voices: [NameVoice; 2],
}

#[test]
fn nested_names_are_unique_across_arrays_and_inner_groups() {
    let names: Vec<&str> = NameProbe::new()
        .param_infos()
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(
        names,
        vec![
            "Level",
            "Osc 1 Level",
            "Osc 1 Env Level",
            "Osc 2 Level",
            "Osc 2 Env Level",
            "Sub Voices 1 Level",
            "Sub Voices 1 Env Level",
            "Sub Voices 2 Level",
            "Sub Voices 2 Env Level",
        ],
    );
    let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
    assert_eq!(unique.len(), names.len());
    // The static table registers the same names.
    let stat: Vec<&str> = NameProbe::param_infos_static()
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(stat, names);
    // A root-level single group keeps its raw names (6.x compatible): the
    // voice itself, used alone, isn't qualified.
    let voice: Vec<&str> = NameVoice::new()
        .param_infos()
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(voice, vec!["Level", "Level"]);
}

// One group type with an array inside, used in two root slots: the
// group's own params stay raw (6.x), but names from its array take the
// group's label too, or both slots would expose `Harm 1 Level`.
#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct NameHolder {
    #[param(name = "Gain", range = "linear(0, 1)", default = 0.5)]
    gain: FloatParam,
    #[nested]
    harm: [NameEnv; 2],
}

#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct NameTwoHolders {
    #[nested]
    osc_a: NameHolder,
    #[nested]
    osc_b: NameHolder,
    // A raw-identifier array field still derives its `_id` helper.
    #[nested]
    r#type: [NameEnv; 2],
}

#[test]
fn arrays_under_a_reused_root_group_take_its_label() {
    let names: Vec<&str> = NameTwoHolders::new()
        .param_infos()
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(
        names,
        vec![
            "Gain",
            "Osc A Harm 1 Level",
            "Osc A Harm 2 Level",
            "Gain",
            "Osc B Harm 1 Level",
            "Osc B Harm 2 Level",
            "Type 1 Level",
            "Type 2 Level",
        ],
    );
    let stat: Vec<&str> = NameTwoHolders::param_infos_static()
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(stat, names);
    assert_eq!(
        NameTwoHolders::type_id(1, NameEnvParamId::Level),
        NameTwoHolders::new().r#type[1].level.id()
    );
}

// An auto-based array between two other groups: the ordinal packer must
// advance by the array's *full* span, not one element.
#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct AutoArray {
    #[param(name = "G", range = "linear(0, 1)", default = 0.1)]
    g: FloatParam,
    #[nested]
    bands: [Band; 2],
    #[nested]
    tail: Band,
}

#[test]
fn ordinal_array_span_offsets_following_groups() {
    let p = AutoArray::new();
    // g 0; bands elem0 1-2, elem1 3-4; tail packs after the whole array
    // at 5-6.
    assert_eq!(p.count(), 7);
    let ids: Vec<u32> = p.param_infos().iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![0, 1, 2, 3, 4, 5, 6]);
    let stat: Vec<u32> = AutoArray::param_infos_static()
        .iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(stat, ids);
    assert_eq!(p.tail.gain.id(), 5);
}

// Hash scheme: each element's ids are mixed with its own salt for `bands[i]`.
#[derive(Params)]
struct HashArray {
    #[nested]
    bands: [HashBand; 3],
}

#[test]
fn hash_array_elements_get_distinct_ids() {
    let p = HashArray::new();
    assert_eq!(p.count(), 6);

    let infos = p.param_infos();
    let mut ids: Vec<u32> = infos.iter().map(|i| i.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids.len(),
        6,
        "every array element param must get a distinct id"
    );
    assert!(ids.iter().all(|&id| id < truce_params::METER_ID_BASE));

    // Static path agrees with the instance.
    let stat: Vec<u32> = HashArray::param_infos_static()
        .iter()
        .map(|i| i.id)
        .collect();
    let inst: Vec<u32> = infos.iter().map(|i| i.id).collect();
    assert_eq!(stat, inst);

    // Round-trips at a live element id.
    let id = p.bands[1].gain.id();
    p.set_plain(id, -3.0);
    assert_eq!(p.get_plain(id), Some(-3.0));
}

// `HashBand` plus one param. Same slot name as `HashArray`, so the only
// difference is the element type's size.
#[derive(Params)]
struct HashBandGrown {
    #[param(name = "Gain", range = "linear(-18, 18)", default = 0.0)]
    gain: FloatParam,
    #[param(name = "Q", range = "log(0.1, 10)", default = 0.7)]
    q: FloatParam,
    #[param(name = "Freq", range = "log(20, 20000)", default = 1000.0)]
    freq: FloatParam,
}

#[derive(Params)]
struct HashArrayGrown {
    #[nested]
    bands: [HashBandGrown; 3],
}

#[test]
fn hash_array_ids_survive_element_growth() {
    // Packing at `base + i * element_count` would move element 1's gain
    // onto element 0's grown range (and so on) once `freq` is added;
    // per-element salted ids keep every existing param put.
    let a = HashArray::new();
    let b = HashArrayGrown::new();
    for i in 0..3 {
        assert_eq!(
            a.bands[i].gain.id(),
            b.bands[i].gain.id(),
            "bands[{i}].gain moved"
        );
        assert_eq!(a.bands[i].q.id(), b.bands[i].q.id(), "bands[{i}].q moved");
        assert_eq!(
            HashArray::bands_id(i, HashBandParamId::Gain),
            HashArrayGrown::bands_id(i, HashBandGrownParamId::Gain),
        );
    }
    let stat: Vec<u32> = HashArrayGrown::param_infos_static()
        .iter()
        .map(|i| i.id)
        .collect();
    let inst: Vec<u32> = b.param_infos().iter().map(|i| i.id).collect();
    assert_eq!(stat, inst);
    for (i, band) in b.bands.iter().enumerate() {
        assert_eq!(
            HashArrayGrown::bands_id(i, HashBandGrownParamId::Freq),
            band.freq.id()
        );
    }
}

// Zero-length arrays: no params, no `_id` helper, and nothing in the
// generated code that trips clippy (the static table used to emit a
// `0u32..0u32` loop).
#[derive(Params)]
struct EmptyHashArray {
    #[param(name = "Mix", default = 1.0)]
    mix: FloatParam,
    #[nested]
    bands: [HashBand; 0],
}

#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct EmptyOrdinalArray {
    #[param(name = "Mix", default = 1.0)]
    mix: FloatParam,
    #[nested]
    bands: [Band; 0],
}

#[test]
fn zero_length_arrays_contribute_nothing() {
    assert_eq!(EmptyHashArray::new().count(), 1);
    assert_eq!(EmptyHashArray::param_infos_static().len(), 1);
    assert_eq!(EmptyOrdinalArray::new().count(), 1);
    assert_eq!(EmptyOrdinalArray::param_infos_static().len(), 1);
}

// The collisions the additive `hash("field[i]") + hash(child)` layout hit
// on ordinary names: masked FNV mixes weakly, so a difference between two
// element bases often equalled one between two child ids, and
// construction panicked with a duplicate id.
#[derive(Params)]
struct MaxMix {
    #[param(name = "Max", default = 0.0)]
    max: FloatParam,
    #[param(name = "Mix", default = 0.0)]
    mix: FloatParam,
}

#[derive(Params)]
struct MaxMixSteps {
    #[nested]
    step: [MaxMix; 32],
}

#[derive(Params)]
struct OnA2 {
    #[param(name = "On", default = 0.0)]
    on: FloatParam,
    #[param(name = "A2", default = 0.0)]
    a2: FloatParam,
}

#[derive(Params)]
struct OnA2Steps {
    #[nested]
    step: [OnA2; 16],
}

#[test]
fn hash_arrays_of_common_child_names_construct() {
    // Construction asserts no duplicate ids; these used to panic.
    let a = MaxMixSteps::new();
    assert_eq!(a.count(), 64);
    let b = OnA2Steps::new();
    assert_eq!(b.count(), 32);
    for (p, stat) in [
        (a.param_infos(), MaxMixSteps::param_infos_static()),
        (b.param_infos(), OnA2Steps::param_infos_static()),
    ] {
        let live: Vec<u32> = p.iter().map(|i| i.id).collect();
        let st: Vec<u32> = stat.iter().map(|i| i.id).collect();
        assert_eq!(live, st, "static table must match the live ids");
    }
    for (i, s) in a.step.iter().enumerate() {
        assert_eq!(MaxMixSteps::step_id(i, MaxMixParamId::Max), s.max.id());
        assert_eq!(MaxMixSteps::step_id(i, MaxMixParamId::Mix), s.mix.id());
    }
}

// A hash array inside a group that is itself nested (with a pinned base):
// the element mix happens first, then the ancestor's additive fold, the
// same order in the live tree, the static table and the `_id` helper.
#[derive(Params)]
struct Lane {
    #[param(name = "Lane Gain", default = 0.0)]
    lane_gain: FloatParam,
    #[nested]
    step: [MaxMix; 4],
}

#[derive(Params)]
struct TwoLanes {
    #[param(name = "Master", default = 0.0)]
    master: FloatParam,
    #[nested(base = 1000)]
    left: Lane,
    #[nested]
    right: Lane,
}

#[test]
fn hash_array_inside_nested_group_agrees_live_and_static() {
    let p = TwoLanes::new();
    assert_eq!(p.count(), 1 + 2 * (1 + 4 * 2));
    let live: Vec<u32> = p.param_infos().iter().map(|i| i.id).collect();
    let stat: Vec<u32> = TwoLanes::param_infos_static()
        .iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(live, stat);
    let mut sorted = live.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), live.len(), "ids must be distinct");
    // Inside its own struct, `Lane::step_id` gives the pre-fold id; the
    // left lane then adds its pinned base 1000.
    for (i, s) in p.left.step.iter().enumerate() {
        let local = Lane::step_id(i, MaxMixParamId::Mix);
        assert_eq!(
            (local + 1000) & (truce_params::METER_ID_BASE - 1),
            s.mix.id()
        );
    }
}

/// The layout's collision rate is the random 24-bit birthday rate, not the
/// weak additive one: a 64 x 16 grid of common child names (1024 ids)
/// stays distinct for this fixed set, where the additive layout collided
/// for a large share of such structs.
#[test]
fn mixed_element_ids_spread_like_random_ids() {
    let children = [
        "gain", "mix", "max", "min", "on", "a2", "level", "pan", "freq", "q", "attack", "decay",
        "sustain", "release", "drive", "tone",
    ];
    let mut ids = std::collections::HashSet::new();
    for field in ["step", "band", "voice", "lane"] {
        let fh = fnv1a32(field);
        for i in 0..64u32 {
            let salt = truce_params::nested_elem_salt(fh, i);
            for c in children {
                ids.insert((field, truce_params::mix_nested_id(fnv1a24(c), salt)));
            }
        }
    }
    for field in ["step", "band", "voice", "lane"] {
        let per_field = ids.iter().filter(|(f, _)| *f == field).count();
        assert_eq!(
            per_field,
            64 * children.len(),
            "{field}: collision in 1024 ids"
        );
    }
}

fn fnv1a32(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

fn fnv1a24(s: &str) -> u32 {
    fnv1a32(s) & (truce_params::METER_ID_BASE - 1)
}
