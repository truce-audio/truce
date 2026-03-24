#![forbid(unsafe_code)]

//! Shared C header types for truce format shims.
//!
//! Provides the `au_shim_types.h` header as an embedded constant so both
//! `truce-au` (build.rs) and `truce-xtask` (AU v3 template) can use a
//! single source of truth.

/// The `au_shim_types.h` C header, embedded at compile time.
pub const AU_SHIM_TYPES_H: &str = include_str!("../include/au_shim_types.h");

/// Returns the path to the `include/` directory within this crate.
///
/// Useful for `truce-au`'s build.rs to set as a C include path.
pub fn include_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("include")
}
