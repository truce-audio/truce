//! Sidecar file that pins the `--shell` logic dylib path at install time.
//!
//! `cargo truce install --shell` writes one of these per plugin; the
//! shell binary loaded by the DAW reads it at first hot-reload to find
//! the matching logic dylib.
//!
//! ## Path layout
//!
//! ```text
//! ~/.truce/shell/<crate_name>.path
//! ```
//!
//! `<crate_name>` matches the consuming crate's `CARGO_PKG_NAME`. The
//! file content is one line: the absolute path to the logic dylib
//! (e.g. `/Users/me/projects/my-plugin/target/shell/libmy_plugin.dylib`).
//! No TOML / no JSON - a single path keeps both writer and reader
//! trivial and parser-free.
//!
//! ## Why `~/.truce/` and not the bundle
//!
//! Per-bundle sidecars (e.g. `MyPlugin.clap/Contents/.truce-shell`)
//! were considered, but the runtime read would need `dladdr` /
//! `GetModuleFileName` to locate the shell binary's own path on disk.
//! Putting the sidecar at a `crate_name`-keyed home-relative path
//! sidesteps that: the shell binary already has `env!("CARGO_PKG_NAME")`
//! baked at compile time, so the read site needs only `$HOME` plus the
//! crate name. Trade-off: only one shell install per crate at a time,
//! which is fine - the only reason to install the same plugin twice is
//! beta/release coexistence, and shell-mode is a dev-loop feature.

use std::path::PathBuf;

/// Resolve `$HOME/.truce/shell/<crate_name>.path` for a given crate.
/// `crate_name` is the consuming crate's `CARGO_PKG_NAME` - the
/// reader passes `env!("CARGO_PKG_NAME")` and the writer passes the
/// resolved plugin's `crate_name` from `truce.toml`. Returns `None`
/// when neither `HOME` (Unix) nor `USERPROFILE` (Windows) is set -
/// the caller should fail loud rather than guess a path.
#[must_use]
pub fn sidecar_path(crate_name: &str) -> Option<PathBuf> {
    Some(
        home_dir()?
            .join(".truce")
            .join("shell")
            .join(format!("{crate_name}.path")),
    )
}

fn home_dir() -> Option<PathBuf> {
    // Unix: HOME. Windows: USERPROFILE. No external `dirs` dep - both
    // env vars are set by every shell / login session truce supports.
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(PathBuf::from(home));
    }
    if let Ok(profile) = std::env::var("USERPROFILE")
        && !profile.is_empty()
    {
        return Some(PathBuf::from(profile));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_path_layout() {
        // Don't mutate $HOME (truce-utils forbids unsafe blocks; the
        // 2024-edition `std::env::set_var` is unsafe). Instead, accept
        // both outcomes: when HOME / USERPROFILE is set the path ends
        // with the expected suffix; otherwise it's None and the writer
        // surfaces a clear error.
        if let Some(p) = sidecar_path("my-plugin") {
            assert!(p.ends_with(".truce/shell/my-plugin.path"));
        }
    }
}
