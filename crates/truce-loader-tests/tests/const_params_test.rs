//! Consts in `#[param(...)]` attributes: `smooth`, `range` bounds, and
//! `default` may reference an `f64` const instead of a literal. The
//! derive emits the const expression into the generated code, so rustc
//! resolves it; `param_infos()` / `param_infos_static()` then carry the
//! resolved values.
//!
//! (A const in `range` / `default` under the LV2 feature is a build
//! error, since the install-time TTL can't evaluate it - that path is
//! exercised by a real plugin, not these bare derive structs.)

// Reading declared defaults / bounds back bit-exact is the point here.
#![allow(clippy::float_cmp)]

use truce_derive::Params;
use truce_params::{FloatParam, Params};

const SMOOTH_MS: f64 = 12.0;
const MIN_RATIO: f64 = 1.0;
const MAX_RATIO: f64 = 10.0;
const DEFAULT_RATIO: f64 = 4.0;

#[derive(Params)]
struct ConstParams {
    #[param(
        name = "Ratio",
        range = "linear(MIN_RATIO, MAX_RATIO)",
        smooth = "exp(SMOOTH_MS)",
        default = DEFAULT_RATIO
    )]
    ratio: FloatParam,
}

#[test]
fn const_range_and_default_resolve_at_runtime() {
    let p = ConstParams::new();
    let infos = p.param_infos();
    let info = &infos[0];
    assert_eq!(info.range.min(), 1.0);
    assert_eq!(info.range.max(), 10.0);
    assert_eq!(info.default_plain, 4.0);

    // The default resolves to the const and round-trips through the id.
    assert_eq!(p.get_plain(info.id), Some(4.0));
}

#[test]
fn const_params_static_matches_instance() {
    let inst = ConstParams::new().param_infos();
    let stat = ConstParams::param_infos_static();
    assert_eq!(stat.len(), inst.len());
    for (s, i) in stat.iter().zip(inst.iter()) {
        assert_eq!(s.id, i.id);
        assert_eq!(s.range.min(), i.range.min());
        assert_eq!(s.range.max(), i.range.max());
        assert_eq!(s.default_plain, i.default_plain);
    }
}

// The issue's exact shape: a const only in `smooth`, literal range /
// default. Smoothing style isn't surfaced in `ParamInfo`, so this is
// mainly a compile check - plus the param still constructs and reads.
const DEFAULT_SMOOTHING_MSEC: f64 = 5.0;

#[derive(Params)]
struct SmoothConstParams {
    #[param(
        name = "Mix",
        range = "linear(0.0, 1.0)",
        smooth = "linear(DEFAULT_SMOOTHING_MSEC)",
        default = 0.5
    )]
    mix: FloatParam,
}

#[test]
fn const_smoothing_time_compiles_and_reads() {
    let p = SmoothConstParams::new();
    let id = p.param_infos()[0].id;
    assert_eq!(p.get_plain(id), Some(0.5));
}

// A bare const `default` (no `std::f64::consts` whitelist) reads back.
const CUTOFF_DEFAULT: f64 = 2000.0;

#[derive(Params)]
struct ConstDefaultParams {
    #[param(name = "Cutoff", range = "log(20.0, 20000.0)", default = CUTOFF_DEFAULT)]
    cutoff: FloatParam,
}

#[test]
fn bare_const_default_resolves() {
    let p = ConstDefaultParams::new();
    let id = p.param_infos()[0].id;
    assert_eq!(p.get_plain(id), Some(2000.0));
}
