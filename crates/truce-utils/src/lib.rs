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
//! - [`state`] - the canonical plugin-state wire format, shared by
//!   the runtime (`truce-core` re-exports it) and `cargo-truce`'s
//!   install-time preset emitters.
//! - [`preset`] - the `.trucepreset` container (metadata + state
//!   envelope), written at install time and read by format wrappers
//!   during host preset scans.
//! - [`slugify`] - ASCII-safe filesystem / IRI slug used by the LV2
//!   staging path and runtime bundle-name derivation.
//! - [`safe_filename`] - case-preserving sanitizer for plugin
//!   display names used as path components (`{name}.aaxplugin`,
//!   `{name}.vst3`, etc.). Replaces filesystem-reserved characters
//!   without lowercasing or collapsing words.
//!
//! `truce-core` re-exports the modules above so consumers that pull
//! `truce-core` don't need a second dependency. Crates that want to
//! avoid `truce-core`'s `truce-params` chain (notably `cargo-truce`)
//! depend on `truce-utils` directly.

pub mod cast;
pub mod midi;
pub mod preset;
pub mod shell_sidecar;
pub mod state;

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

/// Sanitize a plugin's display name into a filesystem-safe form,
/// preserving case and spaces. Use this whenever the name is going
/// to land in a path component (`{name}.aaxplugin`, `{name}.vst3`,
/// the executable inside an AAX `Contents/MacOS/`, etc.). The
/// in-Info.plist / in-host-browser display name should keep using
/// the raw `name` so users still see "Truce Dry/Wet" in their DAW.
///
/// Replacements:
/// - POSIX path separator `/`, Windows path separator `\`, NTFS /
///   HFS path-reserved chars `:<>"|?*`, NUL and ASCII control chars
///   → `-`.
/// - Leading and trailing whitespace + ASCII dots stripped (Windows
///   forbids trailing dots / spaces; trimming both keeps behaviour
///   identical across platforms).
/// - Runs of `-` collapsed to a single `-` so `Dry//Wet` doesn't
///   produce `Dry--Wet`.
#[must_use]
pub fn safe_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for c in name.chars() {
        let reserved = matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
            || c == '\0'
            || c.is_control();
        if reserved {
            if !prev_dash {
                out.push('-');
                prev_dash = true;
            }
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    out.trim_matches(|c: char| c.is_whitespace() || c == '.' || c == '-')
        .to_string()
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

#[cfg(test)]
mod safe_filename_tests {
    use super::safe_filename;

    #[test]
    fn replaces_path_separators() {
        assert_eq!(safe_filename("Truce Dry/Wet"), "Truce Dry-Wet");
        assert_eq!(safe_filename(r"Foo\Bar"), "Foo-Bar");
    }

    #[test]
    fn replaces_windows_reserved() {
        assert_eq!(safe_filename(r#"a:b<c>d"e|f?g*h"#), "a-b-c-d-e-f-g-h");
    }

    #[test]
    fn collapses_runs_of_replacements() {
        assert_eq!(safe_filename("Dry//Wet"), "Dry-Wet");
        assert_eq!(safe_filename("A//B\\\\C"), "A-B-C");
    }

    #[test]
    fn preserves_case_and_spaces() {
        assert_eq!(safe_filename("Truce DryWet"), "Truce DryWet");
        assert_eq!(safe_filename("ALL CAPS"), "ALL CAPS");
    }

    #[test]
    fn trims_whitespace_and_dots() {
        assert_eq!(safe_filename("  Foo  "), "Foo");
        assert_eq!(safe_filename(".hidden."), "hidden");
        assert_eq!(safe_filename(" . . trim . . "), "trim");
    }

    #[test]
    fn empty_in_empty_out() {
        assert_eq!(safe_filename(""), "");
        assert_eq!(safe_filename("///"), "");
    }
}
