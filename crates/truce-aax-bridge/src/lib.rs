#![forbid(unsafe_code)]

//! C ABI contract between the truce AAX cdylib and the C++ AAX
//! template.
//!
//! Provides [`BRIDGE_HEADER`] (the `truce_aax_bridge.h` text,
//! embedded at compile time) and [`TRUCE_AAX_ABI_VERSION`] (the
//! Rust mirror of the header's `#define`). Both `truce-aax`
//! (the Rust cdylib that exports the C symbols) and `cargo-truce`
//! (the build tool that scaffolds the C++ template and writes the
//! header into the scaffolded project) read from this single
//! source of truth.
//!
//! The crate is small on purpose. It exists so `cargo-truce`
//! doesn't have to pull in `truce-aax`'s runtime dependency stack
//! (`truce-core`, `truce-params`, `crossbeam-queue`, ...) just to
//! get a 200-line C header string. Same pattern as the
//! [`truce-shim-types`](https://crates.io/crates/truce-shim-types)
//! split for the AU v3 bridging header.

/// AAX bridge ABI version. Source of truth for the C ABI the
/// Rust cdylib and the AAX C++ template agree on.
///
/// The header [`BRIDGE_HEADER`] mirrors this value as
/// `#define TRUCE_AAX_ABI_VERSION`; a unit test below asserts
/// the two stay in sync. Bumping requires editing both.
pub const TRUCE_AAX_ABI_VERSION: u32 = 9;

/// The C bridge header text, embedded at compile time.
///
/// Living in this crate (the contract owner) means no other
/// crate has to reach across the workspace to read the AAX C
/// ABI. `truce-aax` re-exports both consts for backwards
/// compatibility; `cargo-truce` exposes [`BRIDGE_HEADER`] as
/// `templates::aax::BRIDGE_HEADER` and writes it into the
/// scaffolded AAX project's `src/` dir alongside the C++
/// template files.
pub const BRIDGE_HEADER: &str = include_str!("../include/truce_aax_bridge.h");

#[cfg(test)]
mod tests {
    use super::*;

    /// The C header mirrors [`TRUCE_AAX_ABI_VERSION`] as a
    /// `#define`; the runtime check in the C++ template compares
    /// the cdylib's `truce_aax_abi_version()` (which returns
    /// [`TRUCE_AAX_ABI_VERSION`]) against the `#define`. If the
    /// two ever drift, the template refuses the plugin at load
    /// time. Catching the drift here turns a runtime Pro Tools
    /// failure into a build failure.
    #[test]
    fn bridge_header_abi_define_matches_rust_constant() {
        let parsed = parse_header_abi_define(BRIDGE_HEADER)
            .expect("BRIDGE_HEADER must contain a `#define TRUCE_AAX_ABI_VERSION <N>` line");
        assert_eq!(
            parsed, TRUCE_AAX_ABI_VERSION,
            "header `#define TRUCE_AAX_ABI_VERSION {parsed}` differs from Rust \
             `TRUCE_AAX_ABI_VERSION = {TRUCE_AAX_ABI_VERSION}`. Update both in lock-step."
        );
    }

    /// Extract `<N>` from the first `#define TRUCE_AAX_ABI_VERSION <N>u` line
    /// (tolerates surrounding whitespace and a trailing `u`/`U` suffix).
    fn parse_header_abi_define(contents: &str) -> Option<u32> {
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
}
