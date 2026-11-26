//! Read the bus-layout table out of a real built plugin: the sidechain
//! example's cdylib, whose one layout has a main input, a sidechain input
//! and a main output. Needs `cargo build --workspace` first; skips if the
//! binary is absent.

use std::path::PathBuf;
use truce_build::bus_table::{BusRecord, read_from_binary};

const CRATE: &str = "truce-example-sidechain";

fn cdylib_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("target");
    path.push("debug");
    let stem = CRATE.replace('-', "_");
    path.push(format!(
        "{}{stem}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    path
}

fn bus(name: &str, channels: u32, sidechain: bool) -> BusRecord {
    BusRecord {
        name: name.to_owned(),
        channels,
        sidechain,
    }
}

#[test]
fn reads_the_sidechain_examples_layouts() {
    let path = cdylib_path();
    if !path.exists() {
        eprintln!("skipping: {} not found", path.display());
        eprintln!("build it first: cargo build --workspace");
        return;
    }
    let layouts = read_from_binary(&path, CRATE).unwrap();
    assert_eq!(layouts.len(), 1);
    assert_eq!(
        layouts[0].inputs,
        [bus("Main", 2, false), bus("Sidechain", 2, true)]
    );
    assert_eq!(layouts[0].outputs, [bus("Main", 2, false)]);
}

#[test]
fn another_crates_table_is_not_found() {
    let path = cdylib_path();
    if !path.exists() {
        return;
    }
    let err = read_from_binary(&path, "truce-example-gain").unwrap_err();
    assert!(
        err.contains("truce_bus_layouts_truce_example_gain"),
        "{err}"
    );
}
