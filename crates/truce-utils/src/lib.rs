#![forbid(unsafe_code)]

//! Dependency-free utilities shared across the truce workspace.
//!
//! - [`cast`] - numeric-cast helpers for the audio-plugin → host FFI
//!   boundary (`usize` ↔ `u32` length casts, host `f64` ↔ DSP `f32`,
//!   discrete-index ↔ normalized).
//! - [`midi`] - MIDI value-domain normalize / denormalize between
//!   wire-native integers and `f32` ranges, plus the spec's MIDI 1.0
//!   ↔ MIDI 2.0 bit-replication bridges.
//! - [`shell_sidecar`] - sidecar-file path resolution shared by
//!   `cargo-truce` (writes the sidecar at install-time) and the
//!   `truce::plugin!` macro (reads it at runtime to locate the logic
//!   dylib for hot-reload).
//! - [`slugify`] - ASCII-safe filesystem / IRI slug used by the LV2
//!   staging path and runtime bundle-name derivation.
//!
//! `truce-core` re-exports the modules above so consumers that pull
//! `truce-core` don't need a second dependency. Crates that want to
//! avoid `truce-core`'s `truce-params` chain (notably `cargo-truce`)
//! depend on `truce-utils` directly.

pub mod cast;
pub mod midi;
pub mod shell_sidecar;

/// Slug a plugin's display name into a lowercase, hyphenated,
/// ASCII-safe identifier suitable for filesystem paths, LV2 bundle
/// names, and IRI components.
///
/// Rules: ASCII alphanumerics pass through lowercased; every other
/// character (including runs of them) collapses to a single `-`;
/// leading and trailing dashes are trimmed.
#[must_use]
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod slugify_tests {
    use super::slugify;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("My Plugin"), "my-plugin");
        assert_eq!(slugify("Hello!! World"), "hello-world");
        assert_eq!(slugify("--leading and trailing--"), "leading-and-trailing");
        assert_eq!(slugify("ABC123"), "abc123");
        assert_eq!(slugify(""), "");
    }
}
