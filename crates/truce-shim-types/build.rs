//! Invalidate the crate's incremental cache whenever the bundled C
//! headers change.
//!
//! `src/lib.rs` publishes `include/au_shim_types.h` (and similar) as
//! `&str` constants via `include_str!`. Cargo's default rebuild
//! tracking only watches Rust sources, so a pure header edit doesn't
//! re-trigger the `include_str!` and downstream consumers
//! (`cargo-truce`, AU / AAX shims) silently ship stale bytes. Emitting
//! `rerun-if-changed` for each header forces a rebuild whenever the
//! header on disk no longer matches the one baked into the library.
fn main() {
    println!("cargo:rerun-if-changed=include/au_shim_types.h");
}
