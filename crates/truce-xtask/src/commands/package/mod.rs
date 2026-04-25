//! `cargo truce package` — build, sign, and create installers.
//!
//! Top-level dispatch + the format-flag parsing shared between the macOS
//! `.pkg` pipeline (`macos.rs`) and the Windows Inno Setup pipeline
//! (`packaging_windows`).

use crate::Res;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::{BoxErr, PluginDef};

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
impl PkgFormat {
    pub(crate) fn parse_list(s: &str) -> Result<Vec<PkgFormat>, BoxErr> {
        let mut out = Vec::new();
        for token in s.split(',') {
            match token.trim() {
                "clap" => out.push(PkgFormat::Clap),
                "vst3" => out.push(PkgFormat::Vst3),
                "vst2" => out.push(PkgFormat::Vst2),
                "au2" => out.push(PkgFormat::Au2),
                "au3" => out.push(PkgFormat::Au3),
                "aax" => out.push(PkgFormat::Aax),
                other => return Err(format!("unknown format: {other}").into()),
            }
        }
        Ok(out)
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

pub(crate) fn cmd_package(_args: &[String]) -> Res {
    #[cfg(target_os = "windows")]
    {
        return crate::packaging_windows::cmd_package(_args);
    }
    #[cfg(target_os = "macos")]
    {
        macos::cmd_package_macos(_args)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Err("`cargo truce package` is not supported on this platform. \
         macOS produces signed `.pkg` installers; Windows produces Inno Setup `.exe` installers. \
         For Linux distribution, use `cargo truce build` and ship the bundles from \
         `target/bundles/` via your distro's native packaging (.deb / .rpm / AppImage / Flatpak)."
        .into())
}
