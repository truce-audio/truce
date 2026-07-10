//! End-to-end coverage for the range/smoothing DSL shapes added to
//! `#[param(range = ...)]` / `#[param(smooth = ...)]`: `skewed`,
//! `sym_skewed`, `reversed(...)`, and `log(...)` smoothing. Proves the
//! derive arms expand and, for `reversed`, that the `&'static ParamRange`
//! promotion compiles in both the runtime `FloatParam` constructor and
//! the `param_infos_static` `LazyLock` path.

// The point of the assertions below is exact mapped values (0.5, 25.0,
// endpoints), so float equality is the contract being checked.
#![allow(clippy::float_cmp)]

use truce_derive::Params;
use truce_params::{FloatParamReadF32, Params};

#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct ShapesParams {
    #[param(name = "Skewed", range = "skewed(0, 100, 0.5)")]
    skewed: truce_params::FloatParam,
    #[param(name = "Pan", range = "sym_skewed(-1, 1, 2, 0)")]
    pan: truce_params::FloatParam,
    #[param(name = "Reversed", range = "reversed(linear(0, 100))")]
    reversed: truce_params::FloatParam,
    #[param(name = "Reversed log", range = "reversed(log(20, 20000))")]
    reversed_log: truce_params::FloatParam,
    #[param(
        name = "Freq",
        range = "log(20, 20000)",
        default = 440.0,
        smooth = "log(20)"
    )]
    freq: truce_params::FloatParam,
}

#[test]
fn dsl_shapes_produce_the_expected_ranges() {
    let infos = ShapesParams::new().param_infos();
    let by_name = |n: &str| {
        infos
            .iter()
            .find(|i| i.name == n)
            .expect("param exists")
            .range
    };

    // skewed(0, 100, 0.5): the knob midpoint maps below the linear one.
    assert_eq!(by_name("Skewed").denormalize(0.5), 25.0);

    // sym_skewed anchors `center` at the knob midpoint.
    assert_eq!(by_name("Pan").normalize(0.0), 0.5);
    assert_eq!(by_name("Pan").denormalize(0.5), 0.0);

    // reversed(linear): min at the top of the knob, max at the bottom.
    let rev = by_name("Reversed");
    assert_eq!(rev.normalize(0.0), 1.0);
    assert_eq!(rev.normalize(100.0), 0.0);
    assert_eq!(rev.min(), 0.0);
    assert_eq!(rev.max(), 100.0);

    // reversed(log) still round-trips.
    let rev_log = by_name("Reversed log");
    let back = rev_log.denormalize(rev_log.normalize(2000.0));
    assert!(
        (back - 2000.0).abs() < 0.01,
        "reversed-log round trip: {back}"
    );
}

#[test]
fn static_path_matches_instance_for_new_shapes() {
    // The `reversed(&...)` promotion has to hold up in the `LazyLock`
    // static path too, not just the runtime constructor.
    let instance = ShapesParams::new().param_infos();
    let statics = ShapesParams::param_infos_static();
    assert_eq!(instance.len(), statics.len());
    for (i, s) in instance.iter().zip(statics.iter()) {
        assert_eq!(i.name, s.name);
        // Sample the mapping at a few points rather than the (identical
        // for reversed/linear) bounds, so a dropped `reversed` wrapper
        // would show up.
        for n in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert_eq!(
                i.range.denormalize(n),
                s.range.denormalize(n),
                "param {} diverges at n={n}",
                i.name
            );
        }
    }
}

#[test]
fn log_smoothing_stays_positive_and_converges() {
    // `smooth = "log(20)"` builds a multiplicative smoother: retargeting
    // ramps geometrically, never touching zero, and settles on target.
    let p = ShapesParams::new();
    p.freq.set_value(440.0);
    // Snap to the starting point, then retarget upward.
    let mut last = 440.0_f32;
    p.freq.set_value(4000.0);
    for _ in 0..8192 {
        last = p.freq.read();
        assert!(last > 0.0, "log smoothing dipped to {last}");
    }
    assert!(
        (f64::from(last) - 4000.0).abs() < 1.0,
        "did not converge: {last}"
    );
}
