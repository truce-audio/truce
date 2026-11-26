//! Frozen parameter ids and names for the default (hash) id scheme.
//!
//! Hosts save a plugin's parameter ids in sessions and automation, and LV2
//! hosts save the port symbols derived from its parameter names, so after
//! 7.0 ships these values can't change without breaking saved projects.
//! The other `#[nested]` tests compute their expected ids with the same
//! `mix_nested_id` they exercise, so a change to the hashing or layout
//! would still pass them; this file writes the values out literally. They
//! were taken from the 7.0 layout once and are not to be regenerated - a
//! failure here means the layout changed, which is a breaking change for
//! every plugin's users, not a test to update.
//!
//! Covers each shape that reaches a host: a top-level param, a single
//! nested group, a nested array, an array nested inside an array element,
//! and the static `<field>_id` resolver.

use truce_derive::Params;
use truce_params::{BoolParam, FloatParam, Params};

#[derive(Params)]
struct Step {
    #[param(name = "Level", range = "linear(0, 1)")]
    level: FloatParam,
    #[param(name = "On")]
    on: BoolParam,
}

#[derive(Params)]
struct Env {
    #[param(name = "Attack", range = "linear(0, 1)")]
    attack: FloatParam,
}

#[derive(Params)]
struct Bank {
    #[nested]
    step: [Step; 2],
}

#[derive(Params)]
struct Frozen {
    #[param(name = "Gain", range = "linear(0, 1)")]
    gain: FloatParam,
    #[nested]
    env: Env,
    #[nested]
    step: [Step; 3],
    #[nested]
    bank: [Bank; 2],
}

/// `(id, name)` for every param, in `param_infos()` order.
const FROZEN: &[(u32, &str)] = &[
    (0x0054_26fe, "Gain"),
    (0x0024_44b1, "Attack"),
    (0x00a6_9532, "Step 1 Level"),
    (0x0087_e5cc, "Step 1 On"),
    (0x00f1_1147, "Step 2 Level"),
    (0x00d2_61e1, "Step 2 On"),
    (0x003b_8d5c, "Step 3 Level"),
    (0x001c_ddf6, "Step 3 On"),
    (0x000f_ca92, "Bank 1 Step 1 Level"),
    (0x0041_d777, "Bank 1 Step 1 On"),
    (0x00cc_d64a, "Bank 1 Step 2 Level"),
    (0x004c_5cd0, "Bank 1 Step 2 On"),
    (0x005a_46a7, "Bank 2 Step 1 Level"),
    (0x008c_538c, "Bank 2 Step 1 On"),
    (0x0017_525f, "Bank 2 Step 2 Level"),
    (0x0096_d8e5, "Bank 2 Step 2 On"),
];

#[test]
fn hash_scheme_ids_and_names_are_frozen() {
    let live: Vec<(u32, &str)> = Frozen::new()
        .param_infos()
        .iter()
        .map(|i| (i.id, i.name))
        .collect();
    assert_eq!(live, FROZEN);
}

/// The static path a host scan or `cargo truce` can take without an
/// instance must agree with the frozen values too.
#[test]
fn static_infos_carry_the_frozen_ids() {
    let ids: Vec<u32> = Frozen::param_infos_static().iter().map(|i| i.id).collect();
    let frozen: Vec<u32> = FROZEN.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, frozen);
}

/// `<field>_id` resolves an element's param without an instance; it must
/// land on the same frozen id as the live field.
#[test]
fn the_static_resolver_lands_on_the_frozen_id() {
    assert_eq!(Frozen::step_id(2, StepParamId::Level), 0x003b_8d5c);
    assert_eq!(Frozen::new().step[2].level.id(), 0x003b_8d5c);
}
