//! Regression test for the static-metadata registration path
//! (`Params::param_infos_static` / `PluginExport::param_infos_static`).
//!
//! Format wrappers (`truce-vst2` / `truce-vst3` / `truce-au` /
//! `truce-aax`) read parameter metadata at registration time without
//! constructing a plugin instance — the derive emits a `LazyLock`-
//! cached `Vec<ParamInfo>` for that path. This test asserts the
//! cached path matches `param_infos()` from a real instance and that
//! the `LazyLock` returns the same Vec twice.

// `ParamInfo` static-vs-instance equality is the *point* of this test —
// any drift between the two paths is a real bug, so float fields must
// compare bit-exact.
#![allow(clippy::float_cmp)]

use truce_derive::Params;
use truce_params::Params;

#[derive(Params)]
struct Inner {
    #[param(id = 100, name = "Inner A", range = "linear(0, 1)")]
    a: truce_params::FloatParam,
    #[param(id = 101, name = "Inner B", range = "linear(-1, 1)")]
    b: truce_params::FloatParam,
}

// Outer struct that's purely a composition of nested params. The
// derive skips generating `new()` when the outer has no own params
// or meters, so an Outer of this shape composes by hand from its
// inner pieces.
#[derive(Params)]
struct Outer {
    #[nested]
    a: Inner,
    #[nested]
    b: InnerB,
}

#[derive(Params)]
struct InnerB {
    #[param(id = 200, name = "BB", range = "linear(0, 1)")]
    bb: truce_params::FloatParam,
}

#[test]
fn static_infos_match_instance_infos_flat() {
    let inst = Inner::new();
    let from_instance = inst.param_infos();
    let from_static = Inner::param_infos_static();
    assert_eq!(from_static.len(), from_instance.len());
    for (s, i) in from_static.iter().zip(from_instance.iter()) {
        assert_eq!(s.id, i.id);
        assert_eq!(s.name, i.name);
        assert_eq!(s.unit, i.unit);
        assert_eq!(s.flags, i.flags);
        assert_eq!(s.range.min(), i.range.min());
        assert_eq!(s.range.max(), i.range.max());
        assert_eq!(s.default_plain, i.default_plain);
    }
}

#[test]
fn static_infos_match_instance_infos_nested() {
    // The derive skips auto-generating `new()` when the outer struct
    // has only `#[nested]` fields (and no own params/meters), so we
    // compose Outer by hand. This matches the documented usage shape.
    let inst = Outer {
        a: Inner::new(),
        b: InnerB::new(),
    };
    let from_instance = inst.param_infos();
    let from_static = Outer::param_infos_static();

    assert_eq!(from_static.len(), from_instance.len());
    let static_ids: Vec<u32> = from_static.iter().map(|p| p.id).collect();
    let instance_ids: Vec<u32> = from_instance.iter().map(|p| p.id).collect();
    assert_eq!(static_ids, instance_ids);
    assert_eq!(static_ids, vec![100, 101, 200]);
}

#[test]
fn static_infos_lazylock_returns_consistent_vec() {
    // First call populates the `LazyLock`, second call clones the
    // cached Vec. The contents must be identical.
    let first = Outer::param_infos_static();
    let second = Outer::param_infos_static();
    assert_eq!(first.len(), second.len());
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.name, b.name);
    }
}
