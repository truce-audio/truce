//! Factory-preset library validation: every authored `.preset` file
//! parses, references only real param ids, stays inside each param's
//! range, and round-trips through the canonical state envelope - the
//! same path `cargo truce install` and host preset recall take.

use std::collections::HashSet;
use std::path::Path;

use truce_build::presets::read_presets_dir;
use truce_example_synth::SynthParams;
use truce_params::Params;
use truce_utils::state::deserialize_state;

fn library() -> Vec<truce_build::presets::AuthoredPreset> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("presets");
    read_presets_dir(&dir, false).expect("factory preset library parses")
}

#[test]
fn library_has_one_default_and_unique_names() {
    let presets = library();
    assert!(presets.len() >= 5);
    assert_eq!(presets.iter().filter(|p| p.meta.default).count(), 1);

    let names: HashSet<&str> = presets.iter().map(|p| p.meta.name.as_str()).collect();
    assert_eq!(names.len(), presets.len(), "duplicate preset display name");
}

#[test]
fn presets_reference_real_params_within_range() {
    let params = SynthParams::default();
    let ids: HashSet<u32> = params.param_infos().iter().map(|i| i.id).collect();

    for preset in library() {
        assert!(!preset.params.is_empty(), "{}: empty preset", preset.stem);
        for &(id, value) in &preset.params {
            assert!(
                ids.contains(&id),
                "{}: param id {id} does not exist",
                preset.stem
            );
            assert!(value.is_finite());
        }

        // Restore clamps to each param's range; a round-trip that
        // comes back unchanged proves the authored values are inside
        // the declared ranges.
        params.restore_values(&preset.params);
        let (read_ids, read_values) = params.collect_values();
        for &(id, authored) in &preset.params {
            let idx = read_ids.iter().position(|&r| r == id).unwrap();
            let restored = read_values[idx];
            assert!(
                (restored - authored).abs() < 1e-9,
                "{}: param {id} = {authored} came back as {restored} (out of range?)",
                preset.stem
            );
        }
    }
}

#[test]
fn presets_round_trip_through_state_envelope() {
    const TEST_HASH: u64 = 0x5eed_cafe;
    for preset in library() {
        let blob = preset.state_blob(TEST_HASH);
        let state = deserialize_state(&blob, TEST_HASH).expect("envelope parses");
        assert_eq!(state.params, preset.params, "{}", preset.stem);
        assert_eq!(state.extra.is_some(), !preset.extra.is_empty());
    }
}
