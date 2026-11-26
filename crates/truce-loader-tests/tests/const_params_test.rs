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
use truce_params::{BoolParam, FloatParam, IntParam, Params};

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

// A const `default` on an Int / Bool param. Unlike a literal default
// (checked while the macro expands), a const default is only validated by
// the generated `const` assertion - which previously fired for floats
// only, silently skipping Int / Enum / Bool despite the derive comment
// claiming otherwise. A valid in-range const default must still compile
// (the assertion holds) and read back exactly.
const BITS_DEFAULT: f64 = 12.0;
const GATE_DEFAULT: f64 = 1.0;

#[derive(Params)]
struct IntBoolConstParams {
    #[param(name = "Bits", range = "discrete(2, 16)", default = BITS_DEFAULT)]
    bits: IntParam,
    #[param(name = "Gate", default = GATE_DEFAULT)]
    gate: BoolParam,
}

#[test]
fn int_bool_const_defaults_resolve() {
    let p = IntBoolConstParams::new();
    let infos = p.param_infos();
    assert_eq!(infos[0].default_plain, 12.0);
    assert_eq!(infos[1].default_plain, 1.0);
    assert_eq!(p.get_plain(infos[0].id), Some(12.0));
    assert_eq!(p.get_plain(infos[1].id), Some(1.0));
}

// Every range shape with const slots. Each shape's literal-only checks
// (positive `log` bounds, positive skew factor, `sym_skewed` center inside
// the range, ordered `discrete` bounds, `enum` with >= 2 variants) are
// re-applied to consts by the generated `const` assertion; these valid
// declarations must keep compiling and resolve to the const values.
const LOG_MIN: f64 = 20.0;
const LOG_MAX: f64 = 20_000.0;
const SKEW: f64 = 0.3;
const PAN_MIN: f64 = -1.0;
const PAN_MAX: f64 = 1.0;
const PAN_CENTER: f64 = 0.0;
const STEPS_MIN: i64 = 1;
const STEPS_MAX: i64 = 16;
const MODES: usize = 3;

#[derive(Params)]
struct ConstShapeParams {
    #[param(name = "Freq", range = "log(LOG_MIN, LOG_MAX)", default = 440.0)]
    freq: FloatParam,
    #[param(name = "Drive", range = "skewed(0.0, 10.0, SKEW)", default = 1.0)]
    drive: FloatParam,
    #[param(
        name = "Pan",
        range = "sym_skewed(PAN_MIN, PAN_MAX, SKEW, PAN_CENTER)",
        default = 0.0
    )]
    pan: FloatParam,
    #[param(
        name = "Steps",
        range = "discrete(STEPS_MIN, STEPS_MAX)",
        default = 8.0
    )]
    steps: IntParam,
    #[param(name = "Mode", range = "enum(MODES)", default = 1.0)]
    mode: IntParam,
}

#[test]
fn const_slots_in_every_range_shape_resolve() {
    let infos = ConstShapeParams::new().param_infos();
    let bounds: Vec<(f64, f64)> = infos
        .iter()
        .map(|i| (i.range.min(), i.range.max()))
        .collect();
    assert_eq!(
        bounds,
        [
            (20.0, 20_000.0),
            (0.0, 10.0),
            (-1.0, 1.0),
            (1.0, 16.0),
            (0.0, 2.0)
        ]
    );
}

// Consts whose names `str::parse::<f64>` reads as non-finite floats
// (`inf`, `infinity`, `nan` in any case). They used to be taken for float
// literals, and the derive panicked on proc_macro2's finite-literal assert.
const NAN: f64 = -6.0;
const INF: f64 = 6.0;
const INFINITY: f64 = 3.0;

#[derive(Params)]
struct NonFiniteNamedConsts {
    #[param(
        name = "Trim",
        range = "linear(NAN, INF)",
        smooth = "exp(INFINITY)",
        default = 0.0
    )]
    trim: FloatParam,
}

#[test]
fn consts_named_inf_or_nan_are_consts_not_literals() {
    let p = NonFiniteNamedConsts::new();
    let info = &p.param_infos()[0];
    assert_eq!(info.range.min(), -6.0);
    assert_eq!(info.range.max(), 6.0);
    let stat = &NonFiniteNamedConsts::param_infos_static()[0];
    assert_eq!((stat.range.min(), stat.range.max()), (-6.0, 6.0));
}

// A brace in a param name must not reach the const range check's
// `assert!` as a format string (`Q {x}` used to fail with "cannot find
// value `x`").
#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct BraceNamedConstRange {
    #[param(name = "Q {x}", range = "linear(MIN_RATIO, MAX_RATIO)", default = 4.0)]
    q: FloatParam,
}

#[test]
fn brace_in_name_with_const_range_compiles() {
    let p = BraceNamedConstRange::new();
    assert_eq!(p.q.info.name, "Q {x}");
}
