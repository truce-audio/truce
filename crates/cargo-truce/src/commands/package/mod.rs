//! `cargo truce package` — build, sign, and create installers.
//!
//! Top-level dispatch + the format-flag parsing shared between the macOS
//! `.pkg` pipeline (`macos.rs`) and the Windows Inno Setup pipeline
//! (`packaging_windows`).

#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::BoxErr;
#[cfg(target_os = "macos")]
use crate::PluginDef;
use crate::Res;

pub(crate) mod stage;

#[cfg(target_os = "macos")]
pub(crate) mod macos;

/// Parsed format flags for the package command.
/// Used by both `cmd_package_macos` and `packaging_windows::cmd_package`.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Clone, PartialEq)]
pub(crate) enum PkgFormat {
    Clap,
    Vst3,
    Vst2,
    Au2,
    Au3,
    Aax,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl std::str::FromStr for PkgFormat {
    type Err = BoxErr;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "clap" => Ok(PkgFormat::Clap),
            "vst3" => Ok(PkgFormat::Vst3),
            "vst2" => Ok(PkgFormat::Vst2),
            "au2" => Ok(PkgFormat::Au2),
            "au3" => Ok(PkgFormat::Au3),
            "aax" => Ok(PkgFormat::Aax),
            other => Err(format!("unknown format: {other}").into()),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl PkgFormat {
    /// Comma-separated list parser. Each token is fed through
    /// [`PkgFormat::from_str`] (the `FromStr` impl above), so an
    /// unknown token surfaces a "unknown format: …" error rather
    /// than a generic parse failure.
    pub(crate) fn parse_list(s: &str) -> Result<Vec<PkgFormat>, BoxErr> {
        s.split(',').map(|t| t.trim().parse()).collect()
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "CLAP",
            PkgFormat::Vst3 => "VST3",
            PkgFormat::Vst2 => "VST2",
            PkgFormat::Au2 => "AU2",
            PkgFormat::Au3 => "AU3",
            PkgFormat::Aax => "AAX",
        }
    }
}

// macOS-only `pkgbuild` / `productbuild` plumbing — extensions,
// install paths, PkgID suffixes, AU3 `.app` naming. Windows packaging
// drives Inno Setup directly and doesn't need any of this.
#[cfg(target_os = "macos")]
impl PkgFormat {
    pub(crate) fn extension(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "clap",
            PkgFormat::Vst3 => "vst3",
            PkgFormat::Vst2 => "vst",
            PkgFormat::Au2 => "component",
            PkgFormat::Au3 => "app",
            PkgFormat::Aax => "aaxplugin",
        }
    }

    pub(crate) fn install_location(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "/Library/Audio/Plug-Ins/CLAP/",
            PkgFormat::Vst3 => "/Library/Audio/Plug-Ins/VST3/",
            PkgFormat::Vst2 => "/Library/Audio/Plug-Ins/VST/",
            PkgFormat::Au2 => "/Library/Audio/Plug-Ins/Components/",
            PkgFormat::Au3 => "/Applications/",
            PkgFormat::Aax => "/Library/Application Support/Avid/Audio/Plug-Ins/",
        }
    }

    pub(crate) fn pkg_id_suffix(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "clap",
            PkgFormat::Vst3 => "vst3",
            PkgFormat::Vst2 => "vst2",
            PkgFormat::Au2 => "au2",
            PkgFormat::Au3 => "au3",
            PkgFormat::Aax => "aax",
        }
    }

    /// Whether pkgbuild recognizes this as a native macOS bundle type.
    /// If false, we use --root instead of --component.
    pub(crate) fn is_native_bundle(&self) -> bool {
        matches!(self, PkgFormat::Vst3 | PkgFormat::Au2 | PkgFormat::Au3)
    }

    /// Bundle directory name for a given plugin.
    pub(crate) fn bundle_name(&self, plugin: &PluginDef) -> String {
        match self {
            PkgFormat::Au3 => format!("{}.app", plugin.au3_app_name()),
            _ => format!("{}.{}", plugin.name, self.extension()),
        }
    }

    pub(crate) fn choice_description(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "For Reaper, Bitwig",
            PkgFormat::Vst3 => "For Ableton, FL Studio, Reaper, Cubase",
            PkgFormat::Vst2 => "Legacy — for hosts without VST3 support",
            PkgFormat::Au2 => "For Logic Pro, GarageBand, Ableton",
            PkgFormat::Au3 => "Audio Unit v3 (appex)",
            PkgFormat::Aax => "For Pro Tools",
        }
    }
}

// `args` is unused on platforms where the body falls through to the
// "not supported" Err branch — silence the unused-variable warning
// only on those targets.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows")),
    allow(unused_variables)
)]
pub(crate) fn cmd_package(args: &[String]) -> Res {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        crate::packaging_windows::cmd_package(args)
    }
    #[cfg(target_os = "macos")]
    {
        macos::cmd_package_macos(args)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Err("`cargo truce package` is not supported on this platform. \
         macOS produces signed `.pkg` installers; Windows produces Inno Setup `.exe` installers. \
         For Linux distribution, use `cargo truce build` and ship the bundles from \
         `target/bundles/` via your distro's native packaging (.deb / .rpm / AppImage / Flatpak)."
        .into())
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo truce package [-p <crate>] [--formats <list>] \
[--user|--system|--ask] [--no-notarize] [--no-sign|--no-pace-sign] \
[--host-only|--universal]

Build, sign, and package plugins into a signed installer:
  - macOS:   `target/dist/<Plugin>-<version>-<platform>.pkg` (productbuild)
  - Windows: `target/dist/<Plugin>-<version>-<platform>.exe` (Inno Setup)
  - Linux:   not supported. Use `cargo truce build` and ship the bundles
             from `target/bundles/` via .deb / .rpm / AppImage / Flatpak.

Selection:
  -p <crate>           Package only this plugin crate.
  --formats <list>     Comma-separated subset (clap,vst3,vst2,au2,au3,aax).
                       Default: every format in the plugin's `[features].default`.

Install scope (where the resulting installer puts files at the end user's machine):
  --ask                End user picks at install time. Default.
  --user               User-scope. CLAP/VST3 land in user paths with no
                       admin prompt. System-only formats (AAX, AU v3, Windows
                       VST2) stay system-scope; the user sees one admin prompt.
  --system             Hard-lock to system paths.
  Override the default project-wide via `[packaging] preferred_scope` in truce.toml.

Signing / notarization:
  --no-notarize        Skip macOS notarization (still codesigns).
  --no-pace-sign       Skip PACE (AAX) signing — useful for non-Pro Tools
                       sanity checks. Apple codesign always runs on macOS.
  --no-sign            Synonym for --no-pace-sign on macOS.

Build target (macOS):
  --host-only          Single-arch build of the host. Default is universal.
  --universal          Explicit universal (no-op; same as default).

Misc:
  -h, --help           Show this message."
    );
}
