//! Derive `TRUCE_AAX_ABI_VERSION` from the C header that the AAX C++
//! template includes, so the Rust side and the template can't drift.
//!
//! The bridge enforces strict equality between the cdylib's
//! `truce_aax_abi_version()` and the template's `#define
//! TRUCE_AAX_ABI_VERSION`. A mismatch refuses the load, which Pro
//! Tools reports as a pluginrunner timeout (the bridge stderr line
//! is silently captured). Reading the canonical value from the
//! header at build time makes the divergence impossible to ship.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const HEADER_REL_PATH: &str = "../cargo-truce/templates/aax/truce_aax_bridge.h";

fn main() {
    let header_path = Path::new(HEADER_REL_PATH);
    println!("cargo:rerun-if-changed={}", header_path.display());

    let contents = fs::read_to_string(header_path).unwrap_or_else(|e| {
        panic!(
            "truce-aax build.rs: failed to read {}: {e}",
            header_path.display()
        )
    });

    let version = parse_abi_version(&contents).unwrap_or_else(|| {
        panic!(
            "truce-aax build.rs: could not find `#define TRUCE_AAX_ABI_VERSION <N>u` in {}",
            header_path.display()
        )
    });

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"));
    let dest = out_dir.join("abi_version.rs");
    let body = format!(
        "/// AAX bridge ABI version. Derived at build time from
/// `cargo-truce/templates/aax/truce_aax_bridge.h` by `build.rs`, so
/// the Rust cdylib's `truce_aax_abi_version()` export and the C++
/// template's compile-time check can't drift.
pub const TRUCE_AAX_ABI_VERSION: u32 = {version};
"
    );
    fs::write(&dest, body).unwrap_or_else(|e| {
        panic!(
            "truce-aax build.rs: failed to write {}: {e}",
            dest.display()
        )
    });
}

/// Find the first line matching `#define TRUCE_AAX_ABI_VERSION <N>u`
/// in the header and return `N`. Tolerates additional whitespace, a
/// trailing `u`/`U` suffix, and surrounding comments on the line.
fn parse_abi_version(contents: &str) -> Option<u32> {
    for line in contents.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("#define") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix("TRUCE_AAX_ABI_VERSION") else {
            continue;
        };
        let digits: String = rest
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.is_empty() {
            continue;
        }
        return digits.parse::<u32>().ok();
    }
    None
}
