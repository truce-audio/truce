//! Minimal `home_dir()` shim. We keep this in-tree to avoid pulling in
//! the `dirs` crate just for one lookup.
//!
//! Lookup order:
//! - Unix: `$HOME` (every login shell sets this).
//! - Windows: `%USERPROFILE%` first, falling back to `%HOME%` (some
//!   MSYS / Git Bash setups export `HOME` instead of `USERPROFILE`,
//!   so honoring both keeps `cargo truce` working in both shells
//!   without a `dirs` dependency).
//!
//! Returns `None` only when no usable env var is set; callers that
//! need a hard requirement (e.g. CLAP user-scope install) should
//! propagate the `None` as an error instead of `unwrap()`-ing.

use std::path::PathBuf;

pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Hard-required form of [`home_dir`]. Returns a typed error so the
/// surrounding command can print one line ("can't determine home
/// directory: set HOME / USERPROFILE") instead of panicking. New
/// callers should prefer `?`-propagation; `install_scope` calls this
/// via `.expect()` because routing the error through every `_dir`
/// helper would be a large refactor.
///
/// Windows callers go through `require_local_appdata` / `require_appdata`
/// instead; gate accordingly so the function isn't dead-code on Windows.
#[cfg(not(target_os = "windows"))]
pub(crate) fn require_home_dir() -> Result<PathBuf, crate::CargoTruceError> {
    home_dir().ok_or_else(|| -> crate::CargoTruceError {
        if cfg!(windows) {
            "can't determine home directory: neither USERPROFILE nor HOME is set".into()
        } else {
            "can't determine home directory: HOME is not set".into()
        }
    })
}

/// Windows `LOCALAPPDATA` (`%LOCALAPPDATA%`, e.g.
/// `C:\Users\alice\AppData\Local`) - used as the user-scope plug-in
/// install root for CLAP and VST3 on Windows.
#[cfg(target_os = "windows")]
pub(crate) fn require_local_appdata() -> Result<PathBuf, crate::CargoTruceError> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "LOCALAPPDATA env var not set".into())
}

/// Windows `APPDATA` (`%APPDATA%`, e.g.
/// `C:\Users\alice\AppData\Roaming`) - used as the user-scope LV2
/// install root on Windows. Distinct from `LOCALAPPDATA`: roaming
/// data follows the user across machines via Active Directory,
/// matching the LV2 convention of bundle-relative resources.
#[cfg(target_os = "windows")]
pub(crate) fn require_appdata() -> Result<PathBuf, crate::CargoTruceError> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "APPDATA env var not set".into())
}
