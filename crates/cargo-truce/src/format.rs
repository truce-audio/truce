//! Plug-in format identity (CLAP / VST3 / VST2 / AU / AAX, plus
//! whatever format helpers are installed).
//!
//! `Format` is decoupled from any particular subcommand: install,
//! uninstall, doctor, validate, package, and scaffold all need to
//! talk about "the CLAP version of this plug-in" without redefining
//! a label-or-extension table. Per-format methods live here; the
//! one method that depends on install scope ([`Format::dir`])
//! borrows [`InstallScope`] from `install_scope`.
//!
//! [`Format::External`] is how a format this tool was not compiled
//! against joins in - LV2 arrives that way now. The handle is
//! `&'static` because this enum is `Copy` and appears in `const`
//! tables; see [`crate::external`] for why that is a leak rather than
//! a lifetime parameter.

use std::path::PathBuf;

use crate::external::ExternalFormat;
use crate::install_scope::InstallScope;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    Clap,
    Vst3,
    Vst2,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Au2,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Au3,
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    Aax,
    /// A format described at runtime by a `truce-format-<name>` helper
    /// binary rather than known at compile time.
    External(&'static ExternalFormat),
}

impl Format {
    /// Human-readable display name (`"CLAP"`, `"VST3"`, `"AU v2"`,
    /// …). Used for log/UI labels in `doctor`, `validate`, and
    /// install/uninstall messaging.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Clap => "CLAP",
            Self::Vst3 => "VST3",
            Self::Vst2 => "VST2",
            Self::Au2 => "AU v2",
            Self::Au3 => "AU v3",
            Self::Aax => "AAX",
            Self::External(e) => &e.label,
        }
    }

    /// Per-format install directory for the requested scope. Returns
    /// `None` for combinations not addressable by [`InstallScope`]
    /// alone (AAX is system-only and lives at a fixed location;
    /// AU v3 ships as a host app), and for an external format that
    /// declares no install location on this OS.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn dir(self, scope: InstallScope) -> Option<PathBuf> {
        match self {
            Self::Clap => Some(scope.clap_dir()),
            Self::Vst3 => Some(scope.vst3_dir()),
            Self::Vst2 => Some(scope.vst2_dir()),
            #[cfg(target_os = "macos")]
            Self::Au2 => Some(scope.au_v2_dir()),
            #[cfg(not(target_os = "macos"))]
            Self::Au2 => None,
            Self::Au3 | Self::Aax => None,
            Self::External(e) => e.dir(scope),
        }
    }
}
